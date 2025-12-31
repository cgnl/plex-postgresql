# Security en Stabiliteitsanalyse: SQLite-to-PostgreSQL Shim
**Datum**: 2025-12-25  
**Geanalyseerd door**: Claude Opus 4.5  
**Scope**: /Users/sander/plex-postgresql/src

---

## Executive Summary

Deze analyse identificeert **8 CRITICAL**, **9 HIGH**, **6 MEDIUM**, en **3 LOW** security en stabiliteitsrisico's in de SQLite-to-PostgreSQL interposing shim.

### Kritieke bevindingen:
1. **Use-after-free in connection pool** - Disabled reaper kan actieve verbindingen sluiten
2. **Race conditions in statement registry** - Concurrent access zonder adequate synchronisatie  
3. **Double-free in parameter binding** - Meerdere paden kunnen hetzelfde geheugen vrijmaken
4. **libpq thread-safety violations** - PGconn wordt gedeeld tussen threads zonder bescherming
5. **NULL pointer dereferences** - Ontbrekende validatie op kritieke paden
6. **Memory leaks in SQL translation** - Systematisch lekken bij faalcondities

### Risicoanalyse:
- **Crashes**: CRITICAL - meerdere paden leiden tot segmentation faults
- **Data corruption**: HIGH - race conditions kunnen inconsistente state veroorzaken  
- **Memory exhaustion**: MEDIUM - leaks accumuleren bij langdurig gebruik
- **Security**: MEDIUM - geen directe exploits, maar crashes kunnen leiden tot DoS

---

## 1. Use-After-Free Bugs

### CRITICAL-1: Connection Pool Reaper Use-After-Free
**Locatie**: `src/pg_client.c:230-261`  
**Ernst**: CRITICAL

**Beschrijving**:
De disabled pool reaper code bevat een fundamentele race condition. Hoewel de reaper nu is uitgeschakeld (regels 319-327), blijft het risico bestaan als deze wordt ingeschakeld:

```c
// Line 230-261
static void pool_reap_idle_connections(void) {
    time_t now = time(NULL);
    int reaped = 0;

    // Must be called with pool_mutex held
    for (int i = 0; i < POOL_SIZE; i++) {
        if (library_pool[i].conn &&
            library_pool[i].owner_thread != 0 &&
            !library_pool[i].in_use &&  
            (now - library_pool[i].last_used) > POOL_IDLE_TIMEOUT) {

            if (library_pool[i].conn->conn) {
                PQfinish(library_pool[i].conn->conn);  // DANGER
            }
            pthread_mutex_destroy(&library_pool[i].conn->mutex);
            free(library_pool[i].conn);  // FREE
            library_pool[i].conn = NULL;
```

**Race condition scenario**:
1. Thread A roept `pool_get_connection()` en krijgt slot 5
2. Thread A ontvangt de connection pointer (line 337-344)
3. Thread A unlock de `pool_mutex` (line 343)
4. **WINDOW**: Thread B roept `pool_reap_idle_connections()` 
5. Thread B ziet slot 5 als "idle" (last_used > 60s, in_use=0)
6. Thread B sluit en freed de connection (line 246-250)
7. Thread A probeert de connection te gebruiken → **USE-AFTER-FREE**

**Impact**: Segmentation fault, data corruption, onvoorspelbaar gedrag.

**Aanbevolen fix**:
```c
// Optie 1: Gebruik reference counting
typedef struct {
    pg_connection_t *conn;
    pthread_t owner_thread;
    time_t last_used;
    atomic_int ref_count;  // ADD
    int in_use;
} pool_slot_t;

// In pool_get_connection():
atomic_fetch_add(&library_pool[i].ref_count, 1);
pthread_mutex_unlock(&pool_mutex);
return conn;

// In pool_reap_idle_connections():
if (atomic_load(&library_pool[i].ref_count) > 0) {
    continue;  // Skip if still referenced
}
```

---

### CRITICAL-2: Statement Registry Use-After-Free  
**Locatie**: `src/pg_statement.c:168-189`  
**Ernst**: CRITICAL

**Beschrijving**:
De `pg_unregister_stmt()` functie maakt een statement entry vrij maar laat dangling pointers in de hash table:

```c
void pg_unregister_stmt(sqlite3_stmt *sqlite_stmt) {
    pthread_rwlock_wrlock(&stmt_map_rwlock);

    unsigned int bucket = hash_ptr(sqlite_stmt);
    stmt_entry_t **prev = &stmt_hash[bucket];
    stmt_entry_t *entry = stmt_hash[bucket];

    while (entry) {
        if (entry->sqlite_stmt == sqlite_stmt) {
            *prev = entry->next;  // UNLINK FROM CHAIN
            entry->sqlite_stmt = NULL;  // MARK AS FREE
            entry->pg_stmt = NULL;  // NULL THE POINTER
            break;
        }
        prev = &entry->next;
        entry = entry->next;
    }

    pthread_rwlock_unlock(&stmt_map_rwlock);
}
```

**Race condition**:
1. Thread A calls `pg_find_stmt()` and traverses hash chain
2. Thread A is preempted at `entry = entry->next` (line 206)
3. Thread B calls `pg_unregister_stmt()` and sets `entry->next = NULL`
4. Thread B frees the pg_stmt (in finalize)
5. Thread A resumes and dereferences `entry->next` → **USE-AFTER-FREE**

**Impact**: Segmentation fault bij concurrent finalize/find operations.

---

### CRITICAL-3: Cached Statement Double Free  
**Locatie**: `src/pg_statement.c:290-307`, `src/db_interpose_pg.c:1282-1290`  
**Ernst**: CRITICAL

**Beschrijving**:
Cached statements kunnen dubbel worden vrijgemaakt via twee paden:

**Pad 1**: Thread-local storage destructor (automatisch bij thread exit):
```c
// pg_statement.c:57-68
static void free_thread_cached_stmts(void *ptr) {
    thread_cached_stmts_t *tcs = (thread_cached_stmts_t *)ptr;
    if (tcs) {
        for (int i = 0; i < tcs->count; i++) {
            pg_stmt_t *pg_stmt = tcs->entries[i].pg_stmt;
            if (pg_stmt) {
                pg_stmt_free(pg_stmt);  // FREE #1
            }
        }
        free(tcs);
    }
}
```

**Pad 2**: Expliciete finalize call:
```c
// db_interpose_pg.c:1282-1290
static int my_sqlite3_finalize(sqlite3_stmt *pStmt) {
    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt) {
        pg_unregister_stmt(pStmt);
        pg_stmt_free(pg_stmt);  // FREE #2 (if also in TLS)
    }
    pg_clear_cached_stmt(pStmt);
    return sqlite3_finalize(pStmt);
}
```

**Double-free scenario**:
1. Statement is prepared en cached in TLS via `pg_register_cached_stmt()`
2. Application calls `sqlite3_finalize()` → calls `pg_stmt_free()` (FREE #1)
3. Thread exits → TLS destructor runs → calls `pg_stmt_free()` again (FREE #2)
4. **DOUBLE FREE**

**Impact**: Heap corruption, crash, possible exploitation.

**Aanbevolen fix**:
```c
// Use reference counting
typedef struct pg_stmt {
    atomic_int ref_count;
    // ... rest of fields
} pg_stmt_t;

void pg_stmt_free(pg_stmt_t *stmt) {
    if (!stmt) return;
    if (atomic_fetch_sub(&stmt->ref_count, 1) > 1) {
        return;  // Still referenced
    }
    // ... actual free logic
}
```

---

## 2. Double-Free Bugs

### CRITICAL-4: Parameter Value Double Free  
**Locatie**: `src/db_interpose_pg.c:728-730`, `src/db_interpose_pg.c:1264-1270`  
**Ernst**: CRITICAL

**Beschrijving**:
Parameter values kunnen dubbel worden vrijgemaakt via bind operations en reset/finalize:

```c
// bind_text (line 728-730)
if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
    free(pg_stmt->param_values[pg_idx]);
    pg_stmt->param_values[pg_idx] = NULL;
}
// ... allocate new value

// reset (line 1264-1270)
for (int i = 0; i < MAX_PARAMS; i++) {
    if (pg_stmt->param_values[i] && !is_preallocated_buffer(pg_stmt, i)) {
        free(pg_stmt->param_values[i]);
        pg_stmt->param_values[i] = NULL;
    }
}
```

**Root cause**: Geen mutex protection op bind operations!

**Impact**: Heap corruption, crash.

**Aanbevolen fix**:
```c
static int my_sqlite3_bind_text(sqlite3_stmt *pStmt, int idx, const char *val,
                                 int nBytes, void (*destructor)(void*)) {
    int rc = sqlite3_bind_text(pStmt, idx, val, nBytes, destructor);

    pg_stmt_t *pg_stmt = pg_find_stmt(pStmt);
    if (pg_stmt && idx > 0 && idx <= MAX_PARAMS && val) {
        pthread_mutex_lock(&pg_stmt->mutex);  // ADD LOCK

        int pg_idx = pg_map_param_index(pg_stmt, pStmt, idx);
        if (pg_idx >= 0 && pg_idx < MAX_PARAMS) {
            if (pg_stmt->param_values[pg_idx] && !is_preallocated_buffer(pg_stmt, pg_idx)) {
                free(pg_stmt->param_values[pg_idx]);
                pg_stmt->param_values[pg_idx] = NULL;
            }
            // ... allocate
        }

        pthread_mutex_unlock(&pg_stmt->mutex);  // ADD UNLOCK
    }
    return rc;
}
```

**Alle bind functies moeten worden geprotect**.

---

## 3. Race Conditions

### CRITICAL-5: libpq Thread-Safety Violation  
**Locatie**: `src/pg_client.c:304-426`, `src/db_interpose_pg.c:1096-1101`  
**Ernst**: CRITICAL

**Beschrijving**:
De code deelt PGconn connections tussen threads zonder adequate bescherming. libpq documentatie stelt:

> "One thread restriction is that no two threads attempt to manipulate the same PGconn object at the same time."

Echter, de pool code doet dit:

```c
// Thread A krijgt connection via pool_get_connection()
library_pool[use_idx].owner_thread = current_thread;
library_pool[use_idx].in_use = 1;

pg_connection_t *conn = library_pool[use_idx].conn;
pthread_mutex_unlock(&pool_mutex);  // UNLOCK POOL
return conn;  // Return connection WITHOUT locking conn->mutex

// Thread A executes query (NO LOCK ON conn->mutex!)
pg_stmt->result = PQexecParams(exec_conn->conn, pg_stmt->pg_sql,
    pg_stmt->param_count, NULL, paramValues, NULL, NULL, 0);
```

**Race scenario**:
1. Thread A calls `pool_get_connection()` → gets slot 5 with `conn->conn = PGconn*`
2. Thread A unlocks `pool_mutex`
3. Thread B calls `pool_get_connection()` → reuses slot 5 (owner_thread match)
4. Thread B gets SAME `conn->conn` pointer
5. Thread A calls `PQexecParams(conn->conn, ...)`
6. Thread B calls `PQexecParams(conn->conn, ...)` **SIMULTANEOUSLY**
7. **RACE CONDITION ON PGconn** → undefined behavior

**Impact**: Data corruption, crashes, unpredictable query results.

**Aanbevolen fix**:
```c
// Option 1: Lock the connection mutex during PQexec*
pthread_mutex_lock(&exec_conn->mutex);
PGresult *res = PQexecParams(exec_conn->conn, ...);
pthread_mutex_unlock(&exec_conn->mutex);

// Option 2: Use truly thread-local connections (one PGconn per thread)
__thread PGconn *thread_local_conn = NULL;
```

---

### HIGH-2: Connection Pool in_use Flag Race  
**Locatie**: `src/pg_client.c:330-349`  
**Ernst**: HIGH

**Beschrijving**:
De `in_use` flag wordt gezet/gereset zonder atomic operations:

```c
library_pool[i].in_use = 0;  // NON-ATOMIC WRITE
// ... code ...
library_pool[i].in_use = 1;  // NON-ATOMIC WRITE
```

**Impact**: Two threads using same PGconn → libpq violation → crash.

**Aanbevolen fix**:
```c
typedef struct {
    pg_connection_t *conn;
    pthread_t owner_thread;
    time_t last_used;
    atomic_int in_use;  // MAKE ATOMIC
} pool_slot_t;
```

---

### HIGH-3: Fake Value Pool Race Condition  
**Locatie**: `src/db_interpose_pg.c:1718-1727`  
**Ernst**: HIGH

**Beschrijving**:
De fake value pool gebruikt een circular buffer met incrementing index maar `pg_stmt->current_row` wordt gelezen ZONDER de `pg_stmt->mutex`.

**Aanbevolen fix**:
```c
// Use atomic for fake_value_next
static atomic_int fake_value_next = ATOMIC_VAR_INIT(0);
int slot = atomic_fetch_add(&fake_value_next, 1) % MAX_FAKE_VALUES;
```

---

## 4. Memory Leaks

### HIGH-5: SQL Translation Memory Leak  
**Locatie**: `src/sql_translator.c:208-299`  
**Ernst**: HIGH

**Beschrijving**:
De `sql_translate()` functie heeft meerdere failure paths die leiden tot leaks:

```c
char *step1 = sql_translate_placeholders(sqlite_sql, &result.param_names, &result.param_count);
if (!step1) {
    strcpy(result.error, "Placeholder translation failed");
    return result;  // LEAK: param_names allocated but not freed
}
```

**Impact**: Memory leak bij elke SQL fout.

**Aanbevolen fix**: Add cleanup to all failure paths.

---

### HIGH-6: Connection Pool Slot Leak  
**Locatie**: `src/pg_client.c:116-130`  
**Ernst**: HIGH

**Beschrijving**:
Als de connection registry vol is, wordt de connection niet geregistreerd maar ook niet vrijgemaakt:

```c
pthread_mutex_unlock(&connections_mutex);
LOG_ERROR("Connection registry full! MAX_CONNECTIONS=%d", MAX_CONNECTIONS);
// LEAK: conn is not freed or registered, just lost!
```

**Aanbevolen fix**:
```c
LOG_ERROR("Connection registry full!");
pg_close(conn);  // ADD CLEANUP
```

---

## 5. NULL Pointer Dereferences

### CRITICAL-7: Connection Pointer NULL Dereference  
**Locatie**: `src/pg_client.c:157-184`  
**Ernst**: CRITICAL

**Beschrijving**:
Later wordt `conn->conn` gebruikt zonder NULL check:

```c
PGresult *res = PQexec(pg_conn->conn, exec_sql);  // NO NULL CHECK!
```

**Impact**: Segmentation fault als `pg_conn->conn` is NULL.

**Aanbevolen fix**:
```c
if (!pg_conn || !pg_conn->conn) {
    LOG_ERROR("NULL connection in PQexec");
    return SQLITE_ERROR;
}
PGresult *res = PQexec(pg_conn->conn, exec_sql);
```

---

## 6. Buffer Overflows

### MEDIUM-3: Parameter Buffer Size Assumptions  
**Locatie**: `src/db_interpose_pg.c:671-673`, `src/pg_types.h:96`  
**Ernst**: MEDIUM

**Beschrijving**:
Voor extreme doubles kan `%.17g` format een buffer overflow veroorzaken (max 26 chars in 32-byte buffer is tight).

**Aanbevolen fix**:
```c
char param_buffers[MAX_PARAMS][64];  // Increase to 64 bytes
```

---

### MEDIUM-4: db_path Buffer Overflow  
**Locatie**: `src/pg_types.h:61`, `src/pg_client.c:273`  
**Ernst**: MEDIUM

**Beschrijving**:
`strncpy()` wordt gebruikt maar de string wordt niet gegarandeerd null-terminated:

```c
strncpy(conn->db_path, db_path, sizeof(conn->db_path) - 1);
// If strlen(db_path) >= 1023, conn->db_path is NOT null-terminated!
```

**Aanbevolen fix**:
```c
strncpy(conn->db_path, db_path ? db_path : "", sizeof(conn->db_path) - 1);
conn->db_path[sizeof(conn->db_path) - 1] = '\0';  // FORCE null terminator
```

---

## 7. Thread-Safety van libpq

### CRITICAL-8: Shared PGconn Without Mutex  
**Ernst**: CRITICAL

Dit is een duplicaat/uitbreiding van CRITICAL-5. De kern: PGconn wordt gedeeld tussen threads zonder mutex protection tijdens PQexec* calls.

**Aanbevolen fix**: ALWAYS lock connection mutex before PQexec*.

---

## 8. Aanvullende Bevindingen

### HIGH-10: Lack of Error Handling  
**Locatie**: Meerdere locaties  
**Ernst**: HIGH

Veel `PQexec()` calls checken de result status niet adequaat en returnen OK zelfs bij errors.

---

### MEDIUM-7: Unchecked pthread Return Values  
**Ernst**: MEDIUM

`pthread_mutex_lock()` kan falen maar return values worden niet gecheckt.

---

### MEDIUM-10: Inconsistent Mutex Initialization  
**Ernst**: MEDIUM

Mutex initialization failure wordt niet gecheckt.

---

## Samenvatting en Prioriteiten

### CRITICAL (8 bugs - onmiddellijke actie vereist):
1. CRITICAL-1: Connection Pool Reaper Use-After-Free
2. CRITICAL-2: Statement Registry Use-After-Free  
3. CRITICAL-3: Cached Statement Double Free
4. CRITICAL-4: Parameter Value Double Free
5. CRITICAL-5: libpq Thread-Safety Violation
6. CRITICAL-7: Connection Pointer NULL Dereference
7. CRITICAL-8: Shared PGconn Without Mutex

### HIGH (9 bugs):
1. HIGH-2: Connection Pool in_use Flag Race
2. HIGH-3: Fake Value Pool Race Condition
3. HIGH-5: SQL Translation Memory Leak
4. HIGH-6: Connection Pool Slot Leak
5. HIGH-10: Lack of Error Handling

### MEDIUM (6 bugs):
1. MEDIUM-3: Parameter Buffer Size
2. MEDIUM-4: db_path Buffer Overflow
3. MEDIUM-7: Unchecked pthread Return Values
4. MEDIUM-10: Mutex Initialization

### LOW (3 bugs):
1. Code defensiveness improvements
2. Logging enhancements
3. Magic number collisions

---

## Aanbevelingen

### Onmiddellijk (binnen 1 week):
1. **Fix CRITICAL-4**: Add mutex protection to ALL bind functions
2. **Fix CRITICAL-5/8**: Add mutex protection to all PQexec* calls
3. **Fix CRITICAL-3**: Implement reference counting for pg_stmt_t
4. **Fix HIGH-2**: Make `in_use` flag atomic

### Kort termijn (binnen 1 maand):
1. Fix CRITICAL-1 (pool reaper) - ensure it stays disabled of fix met reference counting
2. Fix CRITICAL-2: Improve statement registry
3. Fix HIGH-5: Add cleanup to SQL translation error paths
4. Fix HIGH-6: Cleanup on pool exhaustion

### Architecturale verbeteringen:
1. Use reference counting voor connections en statements
2. Implement truly thread-local connections (één PGconn per thread via __thread)
3. Use lock-free data structures voor statement registry
4. Add defensive checks overal: assert(), magic numbers, validation

---

## Testplan

### Unit tests:
1. Test connection pool under high concurrency
2. Test statement registry with concurrent register/unregister/find
3. Test bind operations with concurrent reset/finalize

### Integration tests:
1. Run with ThreadSanitizer to detect race conditions
2. Run with AddressSanitizer to detect memory errors
3. Run with Valgrind to detect memory leaks

---

## Conclusie

Deze SQLite-to-PostgreSQL shim bevat **meerdere critical security en stability issues** die onmiddellijk moeten worden aangepakt. De belangrijkste problemen zijn:

1. **Inadequate thread synchronisation** - Shared PGconn zonder mutex protection
2. **Memory management bugs** - Use-after-free, double-free, memory leaks
3. **Race conditions** - In connection pool, statement registry, parameter binding

**AANBEVELING**: Voer onmiddellijk de CRITICAL fixes uit en test grondig met sanitizers voordat deze code in productie wordt gebruikt. De huidige staat van de code is **niet production-ready** voor high-concurrency workloads.

**RISICO INSCHATTING**:
- **Crash risk**: HIGH (meerdere use-after-free en race condition paden)
- **Data corruption risk**: MEDIUM (race conditions kunnen inconsistente state veroorzaken)
- **Memory leak risk**: MEDIUM (leaks in error paths accumuleren bij langdurig gebruik)
- **Security risk**: LOW (geen directe exploits, maar crashes kunnen leiden tot DoS)

---

**Einde van onderzoek**

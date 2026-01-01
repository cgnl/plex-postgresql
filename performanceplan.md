# PostgreSQL Shim Performance Optimalisatie Plan

## Huidige Situatie

| Metric | Waarde | Probleem |
|--------|--------|----------|
| Transacties/sec | 56.000 | Elke query = 1 transactie (autocommit) |
| Prepared statements | 0 | Elke query wordt opnieuw geparsed/gepland |
| PostgreSQL CPU | 60-65% | Overhead van parse/plan/commit |

De shim gebruikt `PQexecParams()` voor elke query, wat betekent:
1. PostgreSQL moet elke query opnieuw parsen
2. PostgreSQL moet elke query opnieuw plannen
3. Elke query is een aparte transactie (autocommit)

## Opties Analyse

### Optie 1: Prepared Statements ⭐ AANBEVOLEN

**Hoe het werkt:**
```
Huidige flow:
sqlite3_prepare_v2(sql) → opslaan SQL string
sqlite3_step()          → PQexecParams(sql, params)  ← PARSE + PLAN elke keer!

Nieuwe flow:
sqlite3_prepare_v2(sql) → PQprepare(conn, "stmt_123", sql) → cache plan
sqlite3_step()          → PQexecPrepared(conn, "stmt_123", params) ← hergebruik plan
```

**Voordelen:**
- Past bij SQLite's prepare/step/finalize model
- Elimineert parse tijd (~10% CPU)
- Elimineert plan tijd (~20-30% CPU)
- Geen risico voor data integriteit
- PostgreSQL beheert plan cache automatisch

**Nadelen:**
- Prepared statements zijn per-connectie
- Moet cache bijhouden per pool slot
- Memory overhead voor statement cache

**Complexiteit:** Medium

**Verwachte verbetering:** 30-50% minder PostgreSQL CPU

---

### Optie 2: Transactie Batching

**Hoe het werkt:**
```
Huidige: Query1 → COMMIT, Query2 → COMMIT, Query3 → COMMIT
Nieuw:   BEGIN → Query1, Query2, Query3 → COMMIT
```

**Voordelen:**
- Drastische reductie in commit overhead
- Minder WAL writes
- Minder fsync calls

**Nadelen:**
- ⚠️ Risico: Plex verwacht mogelijk directe commits
- ⚠️ Risico: Locks worden langer vastgehouden
- ⚠️ Risico: Data verlies bij crash voor commit
- Complexe logica om te bepalen wanneer te committen

**Complexiteit:** Hoog

**Verwachte verbetering:** 50-80% minder commits, maar riskant

---

### Optie 3: Async Commits

**Hoe het werkt:**
```sql
SET synchronous_commit = off;  -- per connectie
```

**Voordelen:**
- Zeer eenvoudig te implementeren (1 regel)
- Grote reductie in commit latency
- Geen logica wijzigingen nodig

**Nadelen:**
- ⚠️ Data kan verloren gaan bij crash (laatste ~100ms)
- Alleen geschikt als data verlies acceptabel is

**Complexiteit:** Laag

**Verwachte verbetering:** 20-40% snellere commits

---

### Optie 4: Query Plan Cache Hints

**Hoe het werkt:**
```sql
SET plan_cache_mode = force_generic_plan;
```

**Voordelen:**
- Server-side optimalisatie
- Geen shim wijzigingen

**Nadelen:**
- Werkt alleen met prepared statements
- Kan suboptimale plannen geven voor sommige queries

**Complexiteit:** Laag (maar vereist Optie 1)

---

## Aanbevolen Plan

### Fase 1: Prepared Statements (Hoogste impact)

**Implementatie stappen:**

#### 1. Statement Cache Structuur

```c
// In pg_types.h of pg_client.c

typedef struct {
    uint64_t sql_hash;           // FNV-1a hash van SQL string
    char stmt_name[32];          // "stmt_<hash>"
    int param_count;             // Aantal parameters
    int prepared;                // 1 = prepared op deze connectie
} prepared_stmt_cache_entry_t;

// Per pool slot - LRU cache
#define STMT_CACHE_SIZE 256
typedef struct {
    prepared_stmt_cache_entry_t entries[STMT_CACHE_SIZE];
    int count;
} stmt_cache_t;

// Toevoegen aan pool_slot_t:
typedef struct {
    pg_connection_t *conn;
    pthread_t owner_thread;
    time_t last_used;
    _Atomic pool_slot_state_t state;
    _Atomic uint32_t generation;
    stmt_cache_t stmt_cache;      // NEW: prepared statement cache
} pool_slot_t;
```

#### 2. Hash Functie voor SQL

```c
// FNV-1a hash - snel en goede distributie
static uint64_t hash_sql(const char *sql) {
    uint64_t hash = 14695981039346656037ULL;  // FNV offset basis
    while (*sql) {
        hash ^= (uint64_t)(unsigned char)*sql++;
        hash *= 1099511628211ULL;  // FNV prime
    }
    return hash;
}
```

#### 3. Wijzig `my_sqlite3_prepare_v2`

```c
// Pseudo-code voor prepare fase
int my_sqlite3_prepare_v2(sqlite3 *db, const char *sql, ...) {
    // ... bestaande code ...

    // NEW: Hash SQL en check cache
    pg_stmt->sql_hash = hash_sql(translated_sql);
    snprintf(pg_stmt->stmt_name, sizeof(pg_stmt->stmt_name),
             "stmt_%llx", pg_stmt->sql_hash);

    // Prepared statement wordt pas gemaakt bij eerste step()
    // omdat we dan pas de connectie weten
    pg_stmt->needs_prepare = 1;

    // ... rest van code ...
}
```

#### 4. Wijzig `my_sqlite3_step`

```c
// Pseudo-code voor step fase
int my_sqlite3_step(sqlite3_stmt *pStmt) {
    pg_statement_t *pg_stmt = get_pg_stmt(pStmt);
    pg_connection_t *conn = get_connection(pg_stmt);
    pool_slot_t *slot = get_pool_slot(conn);

    // Check of statement al prepared is op deze connectie
    stmt_cache_t *cache = &slot->stmt_cache;
    int cached_idx = find_in_cache(cache, pg_stmt->sql_hash);

    if (cached_idx < 0 || !cache->entries[cached_idx].prepared) {
        // Prepare statement op deze connectie
        PGresult *res = PQprepare(conn->conn,
                                   pg_stmt->stmt_name,
                                   pg_stmt->pg_sql,
                                   pg_stmt->param_count,
                                   NULL);  // Let PostgreSQL infer types

        if (PQresultStatus(res) != PGRES_COMMAND_OK) {
            // Handle error - mogelijk statement name conflict
            // Fallback naar PQexecParams
        }
        PQclear(res);

        // Add to cache
        add_to_cache(cache, pg_stmt->sql_hash, pg_stmt->stmt_name);
    }

    // Execute prepared statement
    pg_stmt->result = PQexecPrepared(conn->conn,
                                      pg_stmt->stmt_name,
                                      pg_stmt->param_count,
                                      paramValues,
                                      paramLengths,
                                      paramFormats,
                                      0);  // Text format result

    // ... rest van bestaande code ...
}
```

#### 5. Wijzig Pool Connection Reset

```c
// Bij PQreset() of nieuwe connectie
static void clear_stmt_cache(pool_slot_t *slot) {
    memset(&slot->stmt_cache, 0, sizeof(stmt_cache_t));
    // PostgreSQL ruimt prepared statements automatisch op
    // bij connectie reset, dus we hoeven alleen onze cache te clearen
}
```

#### 6. Handle Edge Cases

- **Statement name collision**: Gebruik langere hash of voeg counter toe
- **Cache vol**: LRU eviction - verwijder oudste entries
- **Schema wijziging**: PostgreSQL invalideert prepared statements automatisch
- **Connectie verloren**: Cache wordt gecleared bij reconnect

**Geschatte code wijzigingen:**
- `pg_types.h`: +20 regels (cache structuren)
- `pg_client.c`: +80 regels (cache management)
- `db_interpose_pg.c`: +100 regels (prepare/execute logica)

---

### Fase 2: Async Commits (Optioneel, laag risico)

Na Fase 1, indien meer performance nodig:

```c
// In pool_create_connection() of na PQreset():
PGresult *res = PQexec(conn->conn, "SET synchronous_commit = off");
PQclear(res);
```

⚠️ **Let op:** Alleen inschakelen als laatste ~100ms aan writes verloren mag gaan bij crash. Voor Plex library metadata is dit waarschijnlijk acceptabel.

---

## Risico Analyse

| Optie | Data Integriteit | Complexiteit | Stabiliteit |
|-------|------------------|--------------|-------------|
| Prepared Statements | ✅ Veilig | Medium | Hoog |
| Transactie Batching | ⚠️ Risico | Hoog | Medium |
| Async Commits | ⚠️ Risico | Laag | Hoog |

---

## Test Plan

### Unit Tests
1. Verify prepared statement wordt hergebruikt (check `pg_prepared_statements`)
2. Verify cache wordt gecleared na connectie reset
3. Verify correcte resultaten met prepared statements

### Performance Tests
1. Meet PostgreSQL CPU voor/na
2. Meet queries/sec voor/na
3. Meet `pg_stat_database.xact_commit` rate voor/na

### Stress Tests
1. 20+ parallelle Plex scanner processen
2. Langdurige scan (1+ uur)
3. Connectie pool churn (frequent reconnects)

---

## Verwachte Resultaten

| Metric | Huidig | Na Fase 1 | Na Fase 2 |
|--------|--------|-----------|-----------|
| PostgreSQL CPU | 60-65% | 30-40% | 25-35% |
| Parse overhead | ~10% | ~0% | ~0% |
| Plan overhead | ~25% | ~2% | ~2% |
| Commit latency | ~1ms | ~1ms | ~0.1ms |

---

## Conclusie

**Start met Fase 1: Prepared Statements**

- Grootste performance winst (30-50% CPU reductie)
- Geen data integriteit risico
- Past natuurlijk bij SQLite's API model
- Beheersbare complexiteit

Fase 2 (async commits) is optioneel en alleen nodig als Fase 1 onvoldoende verbetering geeft.

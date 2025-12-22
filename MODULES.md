# Code Organization Guide

Dit document beschrijft de modulaire structuur van de codebase.

## Project Structuur

```
plex-postgresql/
├── src/
│   ├── pg_types.h            (85 lines)  - Core type definitions
│   ├── pg_config.h/c         (137 lines) - Configuration loading
│   ├── pg_logging.h/c        (106 lines) - Logging infrastructure
│   ├── pg_client.h/c         (140 lines) - PostgreSQL connection
│   ├── pg_statement.h/c      (698 lines) - Statement lifecycle
│   ├── db_interpose_pg.c     (472 lines) - DYLD interpose entry
│   ├── sql_translator.c      (2209 lines)- SQL translation engine
│   └── fishhook.c            (264 lines) - Facebook's fishhook
├── include/
│   └── sql_translator.h      - SQL translator public interface
├── scripts/
│   └── analyze_fallbacks.sh  - Analyze PostgreSQL fallbacks
└── docs/
    ├── FALLBACK_IMPROVEMENT.md
    └── MODULES.md            - This file
```

**Totaal: ~6500 lines verdeeld over 13 bestanden**

---

## Module Overzicht

### pg_types.h (85 lines)
**Core type definities** - Alle structs en constanten

```c
// Belangrijke types:
pg_conn_config_t   // PostgreSQL connection config
pg_connection_t    // Active database connection
pg_stmt_t          // Prepared statement wrapper
pg_value_t         // Fake sqlite3_value for column access

// Constanten:
MAX_CONNECTIONS     16
MAX_PARAMS          64
MAX_CACHED_STMTS    64
PG_VALUE_MAGIC      0x50475641
```

---

### pg_config.h/c (137 lines)
**Configuratie management** - Laadt PostgreSQL settings uit environment

```c
// Public API:
void load_config(void);              // Load from environment
pg_conn_config_t* get_config(void);  // Get current config
int should_redirect(const char *path); // Check if DB should use PG

// Environment variabelen:
PLEX_PG_HOST      // PostgreSQL host (default: localhost)
PLEX_PG_PORT      // PostgreSQL port (default: 5432)
PLEX_PG_DATABASE  // Database name (default: plex)
PLEX_PG_USER      // Database user (default: plex)
PLEX_PG_PASSWORD  // Database password
PLEX_PG_SCHEMA    // Schema name (default: plex)
```

---

### pg_logging.h/c (106 lines)
**Logging infrastructure** - Thread-safe logging met levels

```c
// Public API:
void init_logging(void);                    // Initialize log file
void log_message(int level, const char *fmt, ...);

// Convenience macros:
LOG_DEBUG(fmt, ...)   // Verbose debugging
LOG_INFO(fmt, ...)    // General information
LOG_WARN(fmt, ...)    // Warnings
LOG_ERROR(fmt, ...)   // Errors

// Log levels (PLEX_PG_LOG_LEVEL):
0 = ERROR only
1 = INFO + ERROR (default)
2 = DEBUG + INFO + ERROR
```

---

### pg_client.h/c (140 lines)
**PostgreSQL connection management** - Connectie lifecycle

```c
// Public API:
int ensure_pg_connection(pg_connection_t *conn);  // Connect to PG
void pg_close(pg_connection_t *conn);             // Close connection

// Connection registry:
void pg_register_connection(sqlite3 *db, pg_connection_t *conn);
void pg_unregister_connection(sqlite3 *db);
pg_connection_t* find_pg_connection(sqlite3 *db);
```

---

### pg_statement.h/c (698 lines)
**Statement lifecycle** - Prepare, bind, execute, fetch

```c
// Statement Lifecycle:
pg_stmt_t* pg_prepare(pg_connection_t *conn, const char *sql, sqlite3_stmt *handle);
int pg_step(pg_stmt_t *stmt);         // Execute/fetch next row
int pg_reset(pg_stmt_t *stmt);        // Reset for re-execution
int pg_finalize(pg_stmt_t *stmt);     // Cleanup

// Parameter Binding:
int pg_bind_int(pg_stmt_t *stmt, int idx, int val);
int pg_bind_int64(pg_stmt_t *stmt, int idx, sqlite3_int64 val);
int pg_bind_double(pg_stmt_t *stmt, int idx, double val);
int pg_bind_text(pg_stmt_t *stmt, int idx, const char *val, int len, void(*dtor)(void*));
int pg_bind_blob(pg_stmt_t *stmt, int idx, const void *val, int len, void(*dtor)(void*));
int pg_bind_null(pg_stmt_t *stmt, int idx);

// Column Access:
int pg_column_count(pg_stmt_t *stmt);
int pg_column_type(pg_stmt_t *stmt, int idx);
const char* pg_column_name(pg_stmt_t *stmt, int idx);
int pg_column_int(pg_stmt_t *stmt, int idx);
sqlite3_int64 pg_column_int64(pg_stmt_t *stmt, int idx);
double pg_column_double(pg_stmt_t *stmt, int idx);
const unsigned char* pg_column_text(pg_stmt_t *stmt, int idx);
const void* pg_column_blob(pg_stmt_t *stmt, int idx);
int pg_column_bytes(pg_stmt_t *stmt, int idx);

// Statement Registry:
int is_pg_stmt(pg_stmt_t *stmt);      // Check if pointer is our stmt
void register_pg_stmt(pg_stmt_t *stmt);
void unregister_pg_stmt(pg_stmt_t *stmt);
```

---

### db_interpose_pg.c (472 lines)
**DYLD Interpose entry point** - Hooks SQLite API calls

```c
// Interposed functions:
sqlite3_open()           → my_sqlite3_open()
sqlite3_open_v2()        → my_sqlite3_open_v2()
sqlite3_close()          → my_sqlite3_close()
sqlite3_close_v2()       → my_sqlite3_close_v2()
sqlite3_prepare()        → my_sqlite3_prepare()
sqlite3_prepare_v2()     → my_sqlite3_prepare_v2()
sqlite3_prepare_v3()     → my_sqlite3_prepare_v3()
sqlite3_step()           → my_sqlite3_step()
sqlite3_reset()          → my_sqlite3_reset()
sqlite3_finalize()       → my_sqlite3_finalize()
sqlite3_exec()           → my_sqlite3_exec()
sqlite3_column_*()       → my_sqlite3_column_*()
sqlite3_bind_*()         → my_sqlite3_bind_*()
sqlite3_last_insert_rowid()
sqlite3_changes()
sqlite3_errmsg()
sqlite3_errcode()
sqlite3_wal_checkpoint*()
sqlite3_table_column_metadata()

// Symbol resolution:
resolve_sqlite_symbol()  // Find real SQLite in Plex bundle
get_bundled_sqlite_handle()  // Load Plex's libsqlite3.dylib
```

---

### sql_translator.c (2209 lines)
**SQL Translation Engine** - SQLite → PostgreSQL conversie

#### Sectie Overzicht:

| Lines       | Sectie                        | Beschrijving                              |
|-------------|-------------------------------|-------------------------------------------|
| 1-100       | Helpers                       | str_replace, extract_arg                  |
| 101-232     | Placeholders                  | ? → $1, :name → $2                        |
| 233-600     | Function translations         | iif, typeof, strftime, json_each          |
| 601-960     | Query fixes                   | GROUP BY, HAVING, FTS, DISTINCT           |
| 961-1114    | Main pipeline                 | sql_translate_functions()                 |
| 1115-1331   | Types & quotes                | BLOB→BYTEA, backticks→quotes              |
| 1332-1662   | DDL & keywords                | CREATE, REPLACE INTO, sqlite_master       |
| 1663-1838   | Main entry                    | sql_translate()                           |

#### Belangrijke Functies:

```c
// Main Entry Point
sql_translation_t sql_translate(const char *sql);

// Function translations
translate_iif()              // iif(c,a,b) → CASE WHEN c THEN a ELSE b END
translate_typeof()           // typeof(x) → pg_typeof(x)::text
translate_strftime()         // strftime('%s',x) → EXTRACT(EPOCH FROM x)
translate_json_each()        // json_each() → json_array_elements()
translate_datetime()         // datetime('now') → NOW()
simplify_typeof_fixup()      // Simplify typeof patterns for bigint cols

// Query fixes
fix_group_by_strict()        // Add missing GROUP BY columns
fix_having_aliases()         // Resolve aliases in HAVING
fix_empty_in_clause()        // IN () → IN (NULL)

// Helpers
should_skip_sql()            // Check if query should bypass PG
is_write_query()             // Detect INSERT/UPDATE/DELETE
```

---

## Execution Flow

### Query Lifecycle:

```
Plex App
    ↓
sqlite3_open_v2()  [db_interpose_pg.c]
    ↓
pg_register_connection()  [pg_client.c]
    ↓
sqlite3_prepare_v2()  [db_interpose_pg.c]
    ↓
pg_prepare()  [pg_statement.c]
    ↓
sql_translate()  [sql_translator.c]
    ↓
PQprepare() → PostgreSQL
    ↓
sqlite3_bind_*()  [db_interpose_pg.c]
    ↓
pg_bind_*()  [pg_statement.c]
    ↓
sqlite3_step()  [db_interpose_pg.c]
    ↓
pg_step()  [pg_statement.c]
    ↓
PQexecPrepared() → PostgreSQL
    ↓
sqlite3_column_*()  [db_interpose_pg.c]
    ↓
pg_column_*()  [pg_statement.c]
    ↓
Result → Plex App
```

### Translation Pipeline:

```
SQLite SQL
    ↓
1. should_skip_sql()      → Skip PRAGMA, FTS, etc.
    ↓
2. Placeholders           → ? → $1, :name → $2
    ↓
3. Functions              → iif→CASE, strftime→EXTRACT
    ↓
4. Types                  → BLOB→BYTEA, AUTOINCREMENT→SERIAL
    ↓
5. Keywords               → REPLACE INTO→INSERT ON CONFLICT
    ↓
6. DDL                    → IF NOT EXISTS, quote fixes
    ↓
7. Operators              → !=-1 → != -1
    ↓
PostgreSQL SQL
```

---

## Development Guide

### Adding New Function Translation:

1. Open `src/sql_translator.c`
2. Add translation function:
   ```c
   static char* translate_myfunction(const char *sql) {
       return str_replace_nocase(sql, "SQLITE_FUNC", "PG_FUNC");
   }
   ```
3. Add to pipeline in `sql_translate_functions()`:
   ```c
   temp = translate_myfunction(current);
   free(current);
   if (!temp) return NULL;
   current = temp;
   ```
4. Rebuild: `make clean && make`

### Adding New Interposed Function:

1. Open `src/db_interpose_pg.c`
2. Add wrapper function:
   ```c
   static int my_sqlite3_newfunction(...) {
       pg_connection_t *conn = find_pg_connection(db);
       if (conn && conn->is_pg_active) {
           // Handle via PostgreSQL
       }
       // Fallback to real SQLite
       static int (*orig)(...) = NULL;
       if (!orig) orig = resolve_sqlite_symbol("sqlite3_newfunction");
       return orig ? orig(...) : SQLITE_ERROR;
   }
   ```
3. Add DYLD_INTERPOSE at bottom:
   ```c
   DYLD_INTERPOSE(my_sqlite3_newfunction, sqlite3_newfunction)
   ```

### Debugging:

```bash
# Enable verbose logging
export PLEX_PG_LOG_LEVEL=2

# Watch logs
tail -f /tmp/plex_redirect_pg.log

# Analyze fallbacks
./scripts/analyze_fallbacks.sh

# Check specific query
psql -h localhost -U plex -d plex -c "YOUR_QUERY"
```

---

## Build System

### Makefile Targets:

```bash
make              # Build macOS shim (db_interpose_pg.dylib)
make linux        # Build Linux shim (db_interpose_pg.so)
make clean        # Remove build artifacts
make run          # Start Plex with shim loaded
make stop         # Stop Plex
make test         # Run basic tests
```

### Dependencies:

- **macOS**: libpq (from PostgreSQL), Xcode CLI tools
- **Linux**: libpq-dev, libsqlite3-dev, gcc

### Build Output:

```
db_interpose_pg.dylib  - macOS DYLD_INSERT_LIBRARIES shim
libpq_plex.dylib       - Bundled PostgreSQL client library
```

---

## Common Issues

### "operator does not exist"
**Oorzaak**: Type mismatch
**Fix**: Add cast in sql_translator.c

### "must appear in GROUP BY"
**Oorzaak**: PostgreSQL strict mode
**Fix**: Handled by `fix_group_by_strict()`

### "function does not exist"
**Oorzaak**: SQLite function not translated
**Fix**: Add translation in `sql_translate_functions()`

### Stack overflow / 100% CPU
**Oorzaak**: Excessive logging in hot path
**Fix**: Remove LOG_* calls from column/step functions

---

## Quick Reference

```bash
# Build
make clean && make

# Run Plex with PostgreSQL
make run

# Stop
make stop

# Logs
tail -f /tmp/plex_redirect_pg.log

# Analyze
./scripts/analyze_fallbacks.sh

# PostgreSQL check
psql -h localhost -U plex -d plex -c "SELECT COUNT(*) FROM plex.metadata_items;"
```

**Happy coding!**

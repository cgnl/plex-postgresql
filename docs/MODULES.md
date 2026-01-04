# Code Organization

## Project Structure

```
plex-postgresql/
├── src/
│   ├── pg_types.h              Core type definitions
│   ├── pg_config.c/h           Configuration loading
│   ├── pg_logging.c/h          Logging infrastructure
│   ├── pg_client.c/h           Connection pool management
│   ├── pg_statement.c/h        Statement lifecycle
│   ├── db_interpose_pg.c       DYLD interpose entry point
│   ├── sql_translator.c        SQL translation engine
│   ├── sql_tr_*.c              Translation submodules
│   └── fishhook.c              Runtime symbol rebinding
├── include/
│   └── sql_translator.h        Translator public interface
├── scripts/
│   ├── install_wrappers.sh     Install Plex wrappers
│   ├── uninstall_wrappers.sh   Restore original binaries
│   ├── migrate_sqlite_to_pg.sh SQLite to PostgreSQL migration
│   └── analyze_fallbacks.sh    Analyze fallback queries
├── schema/
│   └── plex_schema.sql         PostgreSQL schema
└── docs/
    └── MODULES.md              This file
```

## Module Overview

### pg_types.h
Core type definitions shared across all modules.

```c
pg_connection_t    // Database connection with pool slot
pg_stmt_t          // Statement with result caching
pg_value_t         // Fake sqlite3_value for column access
```

### pg_config.c/h
Loads PostgreSQL settings from environment variables.

### pg_logging.c/h
Thread-safe logging with levels (ERROR, INFO, DEBUG).

### pg_client.c/h
Connection pool management with configurable size (default 50, max 100).

### pg_statement.c/h
Statement lifecycle: prepare, bind, execute, fetch, finalize.

### db_interpose_pg.c
Main entry point. Intercepts SQLite API calls via DYLD_INTERPOSE.

### sql_translator.c + sql_tr_*.c
SQL translation pipeline: placeholders, functions, types, keywords.

## Execution Flow

```
Plex App
    ↓
sqlite3_open_v2()     → Pool connection
    ↓
sqlite3_prepare_v2()  → sql_translate() → PQprepare()
    ↓
sqlite3_bind_*()      → Store parameters
    ↓
sqlite3_step()        → PQexecPrepared()
    ↓
sqlite3_column_*()    → PQgetvalue()
    ↓
Result → Plex App
```

## Translation Pipeline

```
SQLite SQL
    ↓
1. Placeholders    ? → $1, :name → $2
2. Functions       iif → CASE, strftime → EXTRACT
3. Types           BLOB → BYTEA
4. Keywords        REPLACE INTO → ON CONFLICT
5. Quotes          backticks → double quotes
    ↓
PostgreSQL SQL
```

## Development

### Build
```bash
make clean && make
```

### Debug
```bash
export PLEX_PG_LOG_LEVEL=2
tail -f /tmp/plex_redirect_pg.log
./scripts/analyze_fallbacks.sh
```

### Adding Function Translation

1. Add to `src/sql_tr_functions.c`
2. Register in translation pipeline
3. Rebuild

# Code Organization

## Project Structure

```
plex-postgresql/
├── src/
│   ├── db_interpose_core.c       macOS: DYLD_INTERPOSE + fishhook initialization
│   ├── db_interpose_core_linux.c Linux: LD_PRELOAD + dlsym(RTLD_NEXT)
│   ├── db_interpose_open.c       sqlite3_open/close interception
│   ├── db_interpose_prepare.c    sqlite3_prepare_v2 interception
│   ├── db_interpose_bind.c       sqlite3_bind_* interception
│   ├── db_interpose_step.c       sqlite3_step interception
│   ├── db_interpose_column.c     sqlite3_column_* interception
│   ├── db_interpose_metadata.c   sqlite3_column_name/count/decltype
│   ├── db_interpose_exec.c       sqlite3_exec interception
│   ├── pg_types.h                Core type definitions
│   ├── pg_config.c/h             Configuration loading
│   ├── pg_logging.c/h            Logging infrastructure
│   ├── pg_client.c/h             Connection pool management
│   ├── pg_statement.c/h          Statement lifecycle
│   ├── pg_query_cache.c/h        Query result caching
│   ├── sql_translator.c          SQL translation orchestrator
│   ├── sql_tr_helpers.c          String utilities
│   ├── sql_tr_placeholders.c     ? → $1 placeholder translation
│   ├── sql_tr_functions.c        Function translations (iif, strftime, etc.)
│   ├── sql_tr_query.c            Query structure fixes
│   ├── sql_tr_groupby.c          GROUP BY rewriting
│   ├── sql_tr_types.c            Type translations
│   ├── sql_tr_quotes.c           Quote translations
│   ├── sql_tr_keywords.c         Keyword translations
│   ├── sql_tr_upsert.c           UPSERT/ON CONFLICT handling
│   ├── sql_translator_internal.h Internal translator interfaces
│   └── fishhook.c                macOS runtime symbol rebinding
├── include/
│   └── sql_translator.h          Translator public interface
├── scripts/
│   ├── install_wrappers.sh       Install Plex wrappers (macOS)
│   ├── install_wrappers_linux.sh Install Plex wrappers (Linux)
│   ├── uninstall_wrappers.sh     Restore original binaries
│   ├── migrate_sqlite_to_pg.sh   SQLite to PostgreSQL migration
│   ├── analyze_fallbacks.sh      Analyze fallback queries
│   ├── benchmark.sh              PostgreSQL raw benchmark
│   ├── benchmark_compare.py      SQLite vs PostgreSQL comparison
│   ├── benchmark_plex_stress.py  Library scan + playback simulation
│   ├── benchmark_multiprocess.py Multi-process concurrent access test
│   └── benchmark_locking.py      Database locking test
├── tests/
│   ├── src/
│   │   ├── test_sql_translator.c SQL translation tests (32 tests)
│   │   ├── test_recursion.c      Recursion guards (11 tests)
│   │   ├── test_crash.c          Crash scenario tests (21 tests)
│   │   ├── test_query_cache.c    Query cache tests (16 tests)
│   │   ├── test_tls_cache.c      Thread-local storage tests (7 tests)
│   │   └── test_benchmark.c      Micro-benchmarks
│   ├── bench_cache.c             Cache implementation benchmark
│   └── bench_sqlite_vs_pg.py     SQLite vs PostgreSQL latency
├── schema/
│   └── plex_schema.sql           PostgreSQL schema
└── docs/
    └── modules.md                This file
```

## Module Overview

### Core Interposition

#### db_interpose_core.c (macOS)
Entry point for macOS. Uses `DYLD_INTERPOSE` for static interposition and `fishhook` for runtime symbol rebinding to intercept SQLite calls from dynamically loaded libraries (like SOCI).

#### db_interpose_core_linux.c (Linux)
Entry point for Linux. Uses `LD_PRELOAD` with `dlsym(RTLD_NEXT)` to intercept SQLite calls.

### Interception Modules

| Module | Functions Intercepted |
|--------|----------------------|
| `db_interpose_open.c` | `sqlite3_open`, `sqlite3_open_v2`, `sqlite3_close`, `sqlite3_close_v2` |
| `db_interpose_prepare.c` | `sqlite3_prepare_v2`, `sqlite3_prepare16_v2`, `sqlite3_finalize` |
| `db_interpose_bind.c` | `sqlite3_bind_*` (int, int64, double, text, blob, null), `sqlite3_clear_bindings`, `sqlite3_bind_parameter_index` |
| `db_interpose_step.c` | `sqlite3_step`, `sqlite3_reset` |
| `db_interpose_column.c` | `sqlite3_column_*` (int, int64, double, text, blob, bytes, type), `sqlite3_column_value` |
| `db_interpose_metadata.c` | `sqlite3_column_name`, `sqlite3_column_count`, `sqlite3_column_decltype`, `sqlite3_changes`, `sqlite3_last_insert_rowid` |
| `db_interpose_exec.c` | `sqlite3_exec` |

### Translation Modules

| Module | Responsibility |
|--------|---------------|
| `sql_translator.c` | Main orchestrator, thread-local cache management |
| `sql_tr_helpers.c` | String utilities (strdup, replace, etc.) |
| `sql_tr_placeholders.c` | `?` → `$1`, `:name` → `$2` |
| `sql_tr_functions.c` | `iif` → `CASE`, `strftime` → `EXTRACT`, `IFNULL` → `COALESCE` |
| `sql_tr_query.c` | Query structure fixes (ORDER BY, LIMIT) |
| `sql_tr_groupby.c` | GROUP BY expression rewriting |
| `sql_tr_types.c` | `BLOB` → `BYTEA`, type casts |
| `sql_tr_quotes.c` | Backticks → double quotes |
| `sql_tr_keywords.c` | `GLOB` → `LIKE`, `COLLATE NOCASE` → `ILIKE` |
| `sql_tr_upsert.c` | `INSERT OR REPLACE` → `ON CONFLICT DO UPDATE` |

### PostgreSQL Client

| Module | Responsibility |
|--------|---------------|
| `pg_client.c` | Connection pool (50 connections default, max 100) |
| `pg_statement.c` | Statement lifecycle, reference counting |
| `pg_query_cache.c` | Query result caching (thread-local, TTL-based eviction) |
| `pg_config.c` | Environment variable configuration |
| `pg_logging.c` | Thread-safe logging |

## Caching Architecture

### Three-Layer Cache System

```
┌─────────────────────────────────────────────────────────────────┐
│                     Plex Query                                   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Layer 1: SQL Translation Cache (Thread-Local)                  │
│  ─────────────────────────────────────────────────────────────  │
│  • 512 entries per thread                                       │
│  • Lock-free (no mutex contention)                              │
│  • FNV-1a hash with linear probing                              │
│  • Hit: 22.6 ns → Miss: 17.5 µs (775x speedup)                  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Layer 2: Prepared Statement Cache                              │
│  ─────────────────────────────────────────────────────────────  │
│  • PostgreSQL server-side prepared statements                   │
│  • Avoids re-parsing SQL on PostgreSQL                          │
│  • Automatic per-connection                                     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Layer 3: Query Result Cache (Thread-Local)                     │
│  ─────────────────────────────────────────────────────────────  │
│  • Caches SELECT results for identical queries                  │
│  • TTL-based eviction (configurable)                            │
│  • Hit rate tracking for statistics                             │
└─────────────────────────────────────────────────────────────────┘
```

### Cache Performance

| Cache Type | Hit Latency | Miss Latency | Speedup |
|------------|-------------|--------------|---------|
| Translation (TLS) | 22.6 ns | 17.5 µs | **775x** |
| Query Result | ~1 ns (hash) | ~20 µs (PG query) | **20,000x** |

### Why Thread-Local?

Benchmark of different cache implementations (8 threads, 1M ops/thread):

| Implementation | Latency | Throughput | Notes |
|----------------|---------|------------|-------|
| Mutex | 507 ns | 15.8 M/sec | Global lock contention |
| RWLock | 2,246 ns | 3.6 M/sec | Worse than mutex (!) |
| **Thread-Local** | **22.6 ns** | **354 M/sec** | No contention |
| Lock-Free | 22.9 ns | 350 M/sec | Similar to TLS |

Thread-local storage is **22x faster** than mutex-protected global cache.

## Execution Flow

### Query Execution

```
Plex App
    │
    ▼
sqlite3_prepare_v2(sql)
    │
    ├─ Check: Is this a PostgreSQL table?
    │     No  → Pass to real SQLite (shadow DB)
    │     Yes ↓
    │
    ├─ Translation Cache Lookup (22.6 ns)
    │     Hit  → Use cached PostgreSQL SQL
    │     Miss → sql_translate() (17.5 µs) → Cache result
    │
    ├─ Create pg_stmt_t with translated SQL
    │
    ▼
sqlite3_bind_*(stmt, ...)
    │
    ├─ Store parameters in pg_stmt_t
    │
    ▼
sqlite3_step(stmt)
    │
    ├─ Query Result Cache Lookup
    │     Hit  → Return cached PGresult
    │     Miss ↓
    │
    ├─ PQexecPrepared() → PostgreSQL
    │
    ├─ Cache result if cacheable
    │
    ▼
sqlite3_column_*(stmt, idx)
    │
    ├─ PQgetvalue(result, row, idx)
    │
    ├─ Convert to SQLite type
    │
    ▼
Result → Plex App
```

### Connection Pool Flow

```
sqlite3_open_v2("plex.db")
    │
    ├─ pg_pool_get_connection()
    │     ├─ Check TLS cached connection (fast path, 99% of calls)
    │     ├─ If miss: Find free slot in pool
    │     ├─ If no free: Create new connection (up to pool_size)
    │     └─ Return pg_connection_t*
    │
    ▼
... execute queries ...
    │
    ▼
sqlite3_close(db)
    │
    ├─ pg_pool_release_connection()
    │     └─ Mark slot as available (don't close actual connection)
    │
    ▼
Connection stays in pool for reuse
```

## SQL Translation Pipeline

```
SQLite SQL
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  1. Schema Prefix                                         │
│     metadata_items → plex.metadata_items                  │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  2. Placeholder Translation                               │
│     ? → $1, $2, $3...                                     │
│     :name → $N (with mapping table)                       │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  3. Function Translation                                  │
│     iif(a,b,c) → CASE WHEN a THEN b ELSE c END           │
│     strftime('%s',x) → EXTRACT(EPOCH FROM x)::bigint     │
│     IFNULL(a,b) → COALESCE(a,b)                          │
│     datetime('now') → NOW()                               │
│     SUBSTR(a,b,c) → SUBSTRING(a FROM b FOR c)            │
│     INSTR(a,b) → POSITION(b IN a)                        │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  4. Type Translation                                      │
│     BLOB → BYTEA                                          │
│     INTEGER PRIMARY KEY → SERIAL (on CREATE)              │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  5. Keyword Translation                                   │
│     GLOB '*term*' → LIKE '%term%'                         │
│     COLLATE NOCASE → ILIKE / LOWER()                      │
│     WHERE 0 / WHERE 1 → WHERE FALSE / WHERE TRUE          │
│     LIMIT -1 → (removed)                                  │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  6. UPSERT Translation                                    │
│     INSERT OR REPLACE → INSERT ... ON CONFLICT DO UPDATE  │
│     INSERT OR IGNORE → INSERT ... ON CONFLICT DO NOTHING  │
└──────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│  7. Quote Translation                                     │
│     `column` → "column"                                   │
│     [column] → "column"                                   │
└──────────────────────────────────────────────────────────┘
    │
    ▼
PostgreSQL SQL
```

## Development

### Build

```bash
# macOS
make clean && make

# Linux
make clean && make linux

# With debug symbols
make DEBUG=1
```

### Test

```bash
# All unit tests (87 tests)
make unit-test

# Individual test suites
make test-sql          # SQL translation
make test-recursion    # Recursion guards
make test-crash        # Crash scenarios
make test-cache        # Query cache
make test-tls          # Thread-local storage

# Benchmarks
make benchmark                        # Shim micro-benchmarks
./tests/bin/bench_cache              # Cache comparison
python3 tests/bench_sqlite_vs_pg.py  # SQLite vs PostgreSQL
```

### Debug

```bash
export PLEX_PG_LOG_LEVEL=2  # Enable DEBUG logging
tail -f /tmp/plex_redirect_pg.log

# Analyze fallback queries (queries passed to SQLite)
./scripts/analyze_fallbacks.sh
```

### Adding Function Translation

1. Add to `src/sql_tr_functions.c`:
```c
// In translate_functions()
result = replace_function(result, "new_func(", "pg_equivalent(");
```

2. Add test to `tests/src/test_sql_translator.c`

3. Rebuild: `make clean && make`

### Adding a New Intercepted Function

1. Declare in `src/db_interpose.h`:
```c
VISIBLE extern return_type (*orig_sqlite3_func)(args);
EXPORT return_type my_sqlite3_func(args);
```

2. Define in appropriate `db_interpose_*.c`:
```c
VISIBLE return_type (*orig_sqlite3_func)(args) = NULL;

return_type my_sqlite3_func(args) {
    LOG_DEBUG("FUNC: ...");
    pg_stmt_t *pg_stmt = pg_find_stmt(stmt);
    if (pg_stmt && pg_stmt->is_pg == 2) {
        // PostgreSQL path
    }
    // Fallback to original
    return orig_sqlite3_func ? orig_sqlite3_func(args) : default_value;
}
```

3. Add to rebindings array in `db_interpose_core.c`:
```c
{"sqlite3_func", my_sqlite3_func, (void**)&orig_sqlite3_func},
```

4. Add dlsym fallback:
```c
if (!orig_sqlite3_func) orig_sqlite3_func = dlsym(sqlite_handle, "sqlite3_func");
```

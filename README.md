# plex-postgresql

**Run Plex Media Server with PostgreSQL instead of SQLite.**

This project provides a shim library that intercepts Plex's SQLite calls and redirects them to PostgreSQL, allowing you to use a more scalable and robust database backend with **99%+ query compatibility**.

> **Platform Status**:
> - ✅ **macOS** - Fully working (uses `DYLD_INTERPOSE`)
> - 🚧 **Linux** - Work in progress (uses `LD_PRELOAD`)

## Features

- **Transparent interception** - Uses `DYLD_INTERPOSE` (macOS) or `LD_PRELOAD` (Linux)
- **Advanced SQL translation** - Comprehensive SQLite to PostgreSQL conversion
- **99%+ PostgreSQL coverage** - Nearly all queries run natively on PostgreSQL
- **Intelligent fallback system** - Automatic fallback to SQLite for edge cases
- **Zero Plex modifications** - Works with stock Plex Media Server
- **Full sqlite3_value support** - Proper handling of Plex's value-based column access

## Recent Fixes

### sqlite3_value Interception (Dec 2025)

Fixed the "column index out of range" error that caused Movies library to return HTTP 500.

**Root Cause**: Plex uses two different patterns to access query results:
1. Direct column access: `sqlite3_column_text()`, `sqlite3_column_int()`, etc.
2. Value-based access: `sqlite3_column_value()` returns an `sqlite3_value*` pointer, then `sqlite3_value_type()`, `sqlite3_value_text()`, etc. extract the data

Our shim handled pattern 1 correctly, but for pattern 2, we were passing through to real SQLite's `sqlite3_column_value()`. Since our statement pointers aren't real SQLite statements, this returned garbage pointers that crashed Plex when it tried to decode them.

**Solution**: Created a fake `sqlite3_value` mechanism:

```c
// Structure encoding PostgreSQL result info
typedef struct {
    uint32_t magic;      // Magic number for validation (0x50475641 = "PGVA")
    void *pg_stmt;       // Pointer to our pg_stmt_t
    int col_idx;         // Column index
    int row_idx;         // Row index
} pg_fake_value_t;
```

When `sqlite3_column_value()` is called on a PostgreSQL result:
1. We return a pointer to a fake value from our pool
2. The fake value encodes the statement, column, and row indices
3. All `sqlite3_value_*` functions check if the pointer is one of our fakes
4. If so, they extract data from PostgreSQL; otherwise pass to real SQLite

Intercepted functions:
- `sqlite3_value_type()` - Returns proper SQLITE_* type from PostgreSQL OID
- `sqlite3_value_text()` - Returns `PQgetvalue()` result
- `sqlite3_value_int()` / `sqlite3_value_int64()` - Converts text to integer
- `sqlite3_value_double()` - Converts text to double
- `sqlite3_value_bytes()` - Returns `PQgetlength()`
- `sqlite3_value_blob()` - Returns raw `PQgetvalue()` pointer

### Other SQL Translation Fixes
- **JSON functions**: `json_each()` to `json_array_elements()` with proper type casting
- **GROUP BY strict mode**: Automatically adds missing non-aggregate columns
- **GROUP BY NULL removal**: SQLite allows `GROUP BY NULL`, PostgreSQL doesn't
- **HAVING alias resolution**: Resolves column aliases in HAVING clauses
- **Empty IN clause handling**: `IN ()` and `IN (  )` to `IN (NULL)`
- **50+ function translations**: iif, typeof, strftime, unixepoch, datetime, and more
- **Boolean type mapping**: PostgreSQL OID 16 correctly mapped to SQLITE_INTEGER
- **NULLS LAST ordering**: Proper NULL ordering for compatibility

## Requirements

### macOS
- Apple Silicon or Intel
- PostgreSQL 15+
- Plex Media Server
- Xcode Command Line Tools (`xcode-select --install`)

### Linux (WIP)
> ⚠️ Linux support is under development. The code compiles but has not been fully tested.

- GCC and build tools
- libpq-dev (PostgreSQL client library)
- libsqlite3-dev
- Plex Media Server

## Quick Start (macOS)

### 1. Install PostgreSQL

```bash
brew install postgresql@15
brew services start postgresql@15
```

### 2. Create Database & User

```bash
createuser -U postgres plex
createdb -U postgres -O plex plex
psql -U postgres -c "ALTER USER plex PASSWORD 'plex';"
psql -U plex -d plex -c "CREATE SCHEMA plex;"
```

### 3. Build the Shim

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make clean && make
```

### 4. Install Wrappers

This installs wrapper scripts that auto-inject the PostgreSQL shim whenever Plex starts:

```bash
# Stop Plex first
pkill -x "Plex Media Server" 2>/dev/null

# Install wrappers (backs up original binaries to .original)
./scripts/install_wrappers.sh
```

The wrappers configure these environment variables with defaults:
- `PLEX_PG_HOST=localhost`
- `PLEX_PG_PORT=5432`
- `PLEX_PG_DATABASE=plex`
- `PLEX_PG_USER=plex`
- `PLEX_PG_PASSWORD=plex`
- `PLEX_PG_SCHEMA=plex`

### 5. Start Plex

Simply start Plex normally - the shim is auto-injected by the wrapper:

```bash
open "/Applications/Plex Media Server.app"
```

Or via launchd, watchdog, etc. No special environment setup needed.

### 6. Monitor & Verify

```bash
# Check logs
tail -f /tmp/plex_redirect_pg.log

# Analyze fallbacks
./scripts/analyze_fallbacks.sh
```

### Uninstall

To restore original Plex binaries and stop using PostgreSQL:

```bash
pkill -x "Plex Media Server.original" 2>/dev/null
./scripts/uninstall_wrappers.sh
```

## How It Works

### Architecture

```
Plex Media Server
      |
      v
SQLite API calls (sqlite3_prepare_v2, sqlite3_step, etc.)
      |
      v
DYLD_INTERPOSE shim (db_interpose_pg.dylib)
      |
      +-- SQL Translator (SQLite syntax -> PostgreSQL syntax)
      |
      v
PostgreSQL Database (libpq)
```

### Key Interposed Functions

| SQLite Function | Our Handler | Purpose |
|----------------|-------------|---------|
| `sqlite3_open*` | `my_sqlite3_open_v2` | Establish PostgreSQL connection |
| `sqlite3_prepare*` | `my_sqlite3_prepare_v2` | Translate SQL and prepare statement |
| `sqlite3_step` | `my_sqlite3_step` | Execute query, fetch rows |
| `sqlite3_column_*` | `my_sqlite3_column_*` | Return column values from PG result |
| `sqlite3_column_value` | `my_sqlite3_column_value` | Return fake value pointer |
| `sqlite3_value_*` | `my_sqlite3_value_*` | Decode fake value, return PG data |
| `sqlite3_finalize` | `my_sqlite3_finalize` | Clean up PG result |

### SQL Translation Pipeline

1. **Placeholders**: `?` and `:name` to `$1`, `$2`, ...
2. **Functions**: `iif()` to `CASE WHEN`, `strftime()` to `EXTRACT()`, etc.
3. **Types**: `AUTOINCREMENT` to `SERIAL`, `BLOB` to `BYTEA`
4. **Keywords**: `REPLACE INTO` to `INSERT ... ON CONFLICT`
5. **Identifiers**: Backticks to double quotes
6. **Operators**: Fix spacing (`!=-1` to `!= -1`)

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PLEX_PG_HOST` | localhost | PostgreSQL host |
| `PLEX_PG_PORT` | 5432 | PostgreSQL port |
| `PLEX_PG_DATABASE` | plex | Database name |
| `PLEX_PG_USER` | plex | Database user |
| `PLEX_PG_PASSWORD` | (empty) | Database password |
| `PLEX_PG_SCHEMA` | plex | Schema name |

## Project Structure

```
plex-postgresql/
├── src/
│   ├── db_interpose_pg.c       # Main shim with DYLD_INTERPOSE
│   ├── pg_types.h              # Core type definitions
│   ├── pg_config.c/h           # Configuration loading
│   ├── pg_logging.c/h          # Logging infrastructure
│   ├── pg_client.c/h           # PostgreSQL connection management
│   ├── pg_statement.c/h        # Statement lifecycle
│   ├── sql_translator.c        # SQL translation engine
│   ├── sql_tr_*.c              # Translation submodules
│   └── fishhook.c              # Runtime symbol rebinding
├── include/
│   └── sql_translator.h        # Translator public interface
├── scripts/
│   └── analyze_fallbacks.sh    # Fallback analysis tool
├── Makefile
└── README.md
```

## Troubleshooting

### Plex won't start

```bash
# Check PostgreSQL is running
pg_isready -h localhost -U plex

# Check logs
tail -50 /tmp/plex_redirect_pg.log
```

### Connection errors

```bash
# Test PostgreSQL connection
psql -h localhost -U plex -d plex -c "SELECT 1;"

# Check credentials
env | grep PLEX_PG
```

### Query failures

```bash
# Check for translation errors in log
grep -i error /tmp/plex_redirect_pg.log | tail -20

# Analyze fallbacks
./scripts/analyze_fallbacks.sh
```

## License

MIT License - see [LICENSE](LICENSE) file for details.

## Disclaimer

**This is an unofficial project and is not affiliated with Plex Inc.**

Use at your own risk. Always maintain backups of your Plex database.

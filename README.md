# plex-postgresql

[![en](https://img.shields.io/badge/lang-en-red.svg)](README.md)
[![es](https://img.shields.io/badge/lang-es-yellow.svg)](README.es.md)

**Run Plex Media Server with PostgreSQL instead of SQLite.**

A shim library that intercepts Plex's SQLite calls and redirects them to PostgreSQL. Zero Plex modifications required.

| Platform | Status |
|----------|--------|
| macOS | ✅ Production tested |
| Linux (Docker) | ✅ Production tested (65K+ items, full library queries) |
| Linux (Native) | ⚠️ Untested |

## Why PostgreSQL?

SQLite is great for most Plex installations, but has one major limitation: **database locking**.

- **No more locking** - SQLite locks the entire database during writes. Library scans block playback. Concurrent scans queue up. With PostgreSQL, everything runs simultaneously - scan your libraries while streaming without interruption.
- **Remote storage** - Better I/O patterns for rclone, Real-Debrid, or cloud storage setups.
- **Large libraries** - PostgreSQL's query optimizer handles 10K+ movies and 50K+ episodes efficiently.
- **Standard tooling** - pg_dump for backups, replication, any PostgreSQL client for debugging.

## Benchmark Results

Real-world test: **Plex + Kometa + PMM + 4 concurrent streams** (7 separate processes, 15 seconds):

| Metric | SQLite | PostgreSQL |
|--------|--------|------------|
| **Write Errors** | **281,095** | **0** |
| Playback write success rate | 0% | 100% |
| Kometa errors | 140,817 | 0 |
| PMM errors | 135,080 | 0 |

**What this means:**
- SQLite: Watch progress not saved during scans (100% failure)
- SQLite: ~1.1 million errors per minute under load
- PostgreSQL: Zero errors, everything works simultaneously

Run the benchmark yourself:
```bash
python3 scripts/benchmark_multiprocess.py
```

For rclone/Real-Debrid setups with Kometa/PMM, **SQLite becomes unusable** during library scans. PostgreSQL handles it without issues.

## Quick Start (Docker)

The easiest way to run Plex with PostgreSQL:

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# Start Plex + PostgreSQL
docker-compose up -d

# Check logs
docker-compose logs -f plex
```

Plex will be available at http://localhost:8080

PostgreSQL is automatically configured with schema initialization.

### Configuration

Edit `docker-compose.yml` to customize:

```yaml
environment:
  - PLEX_PG_HOST=postgres
  - PLEX_PG_PORT=5432
  - PLEX_PG_DATABASE=plex
  - PLEX_PG_USER=plex
  - PLEX_PG_PASSWORD=plex
  - PLEX_PG_SCHEMA=plex
  - PLEX_PG_POOL_SIZE=50
```

Mount your media:
```yaml
volumes:
  - /path/to/media:/media:ro
```

## Quick Start (macOS)

### 1. Setup PostgreSQL

```bash
brew install postgresql@17
brew services start postgresql@17

createuser plex
createdb -O plex plex
psql -d plex -c "ALTER USER plex PASSWORD 'plex';"
psql -U plex -d plex -c "CREATE SCHEMA plex;"
```

### 2. Build & Install

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make clean && make

# Stop Plex, install wrappers
pkill -x "Plex Media Server" 2>/dev/null
./scripts/install_wrappers.sh
```

### 3. Start Plex

```bash
open "/Applications/Plex Media Server.app"
```

The shim is auto-injected. Check logs: `tail -f /tmp/plex_redirect_pg.log`

### Uninstall

```bash
pkill -x "Plex Media Server" 2>/dev/null
./scripts/uninstall_wrappers.sh
```

## Quick Start (Linux Native) - Untested

### 1. Setup PostgreSQL

```bash
sudo apt install postgresql-15
sudo -u postgres createuser plex
sudo -u postgres createdb -O plex plex
sudo -u postgres psql -c "ALTER USER plex PASSWORD 'plex';"
psql -U plex -d plex -c "CREATE SCHEMA plex;"
```

### 2. Build & Install

```bash
# Install dependencies
sudo apt install build-essential libsqlite3-dev libpq-dev

git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make linux
sudo make install

# Stop Plex, install wrappers
sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh
```

### 3. Configure & Start

```bash
# Add to /etc/default/plexmediaserver:
# PLEX_PG_HOST=localhost
# PLEX_PG_DATABASE=plex
# PLEX_PG_USER=plex
# PLEX_PG_PASSWORD=plex

sudo systemctl start plexmediaserver
```

### Uninstall

```bash
sudo systemctl stop plexmediaserver
sudo ./scripts/uninstall_wrappers_linux.sh
```

## Migration from SQLite

To migrate an existing Plex library to PostgreSQL:

```bash
# macOS
./scripts/migrate_sqlite_to_pg.sh

# Docker (mount source database first)
docker exec plex-postgresql bash -c '
  export PGHOST=postgres PGUSER=plex PGPASSWORD=plex PGDATABASE=plex
  SQLITE_DB="/source-db/com.plexapp.plugins.library.db"

  for TABLE in $(sqlite3 "$SQLITE_DB" ".tables"); do
    COLS=$(sqlite3 "$SQLITE_DB" "PRAGMA table_info($TABLE);" | cut -d"|" -f2 | tr "\n" ",")
    sqlite3 -header -csv "$SQLITE_DB" "SELECT * FROM $TABLE;" > /tmp/$TABLE.csv
    psql -c "\\copy plex.$TABLE($COLS) FROM /tmp/$TABLE.csv WITH CSV HEADER"
  done
'
```

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `PLEX_PG_HOST` | localhost | PostgreSQL host |
| `PLEX_PG_PORT` | 5432 | PostgreSQL port |
| `PLEX_PG_DATABASE` | plex | Database name |
| `PLEX_PG_USER` | plex | Database user |
| `PLEX_PG_PASSWORD` | (empty) | Database password |
| `PLEX_PG_SCHEMA` | plex | Schema name |
| `PLEX_PG_POOL_SIZE` | 50 | Connection pool size (max 100) |
| `PLEX_PG_LOG_LEVEL` | 1 | 0=ERROR, 1=INFO, 2=DEBUG |

## How It Works

```
macOS:  Plex → SQLite API → DYLD_INTERPOSE shim → SQL Translator → PostgreSQL
Linux:  Plex → SQLite API → LD_PRELOAD shim    → SQL Translator → PostgreSQL
Docker: Plex → SQLite API → LD_PRELOAD shim    → SQL Translator → PostgreSQL (container)
```

The shim intercepts all `sqlite3_*` calls, translates SQL syntax (placeholders, functions, types), and executes on PostgreSQL via libpq.

### Architecture

The codebase uses a modular architecture with platform-specific cores:

```
src/
├── db_interpose_core.c        # macOS: DYLD_INTERPOSE + fishhook
├── db_interpose_core_linux.c  # Linux: LD_PRELOAD + dlsym(RTLD_NEXT)
├── db_interpose_*.c           # Shared: open, exec, prepare, bind, step, column, metadata
├── sql_translator.c           # SQLite → PostgreSQL SQL translation
├── sql_tr_*.c                 # Translation modules: functions, types, quotes, etc.
└── pg_*.c                     # PostgreSQL client, connection pool, statement cache
```

### Key Features

- **Connection pooling** - Efficient reuse of PostgreSQL connections
- **SQL translation** - Automatic SQLite → PostgreSQL syntax conversion
- **Prepared statements** - Query caching for performance
- **Schema initialization** - Auto-creates PostgreSQL schema on first run
- **Circular reference protection** - Trigger prevents self-referential parent_id crashes
- **Stack overflow protection** - Multi-layer defense against crashes (see below)
- **Auto-build** - Wrapper automatically rebuilds shim if dylib is missing

### SQL Translation Features

The translator handles SQLite-specific syntax automatically:

| SQLite | PostgreSQL |
|--------|------------|
| `COLLATE NOCASE` | `LOWER()` comparisons |
| `WHERE column LIKE '%x%' COLLATE NOCASE` | `WHERE column ILIKE '%x%'` |
| `WHERE 0` / `WHERE 1` | `WHERE FALSE` / `WHERE TRUE` |
| `iif(cond, a, b)` | `CASE WHEN cond THEN a ELSE b END` |
| `strftime('%s', x)` | `EXTRACT(EPOCH FROM x)::bigint` |
| `IFNULL(a, b)` | `COALESCE(a, b)` |
| `title MATCH 'action -comedy'` | FTS with `!` negation |
| `title MATCH 'term1 AND term2'` | FTS with `&` operator |
| `title MATCH '"exact phrase"'` | FTS with `<->` adjacency |
| `?` placeholders | `$1, $2, ...` numbered params |

### Stack Protection

Plex uses small thread stacks (544KB) which can overflow during complex queries. The shim provides multi-layer protection:

| Layer | Threshold | Action |
|-------|-----------|--------|
| Worker delegation | < 400KB remaining | Delegate to 8MB worker thread |
| Hard protection (normal) | < 64KB remaining | Return SQLITE_NOMEM |
| Hard protection (worker) | < 32KB remaining | Return SQLITE_NOMEM |

This prevents stack overflow crashes that occurred with deep recursive queries (e.g., OnDeck with 218 recursive frames).

## Testing

Run unit tests to validate the shim:

```bash
# All unit tests (87 tests total)
make unit-test

# Individual test suites
make test-recursion      # Recursion guards, loop detection (11 tests)
make test-crash          # Production crash scenarios (21 tests)
make test-sql            # SQL translation (32 tests)
make test-cache          # Query cache logic (16 tests)
make test-tls            # Thread-local storage (7 tests)

# Benchmarks
make benchmark           # Shim component micro-benchmarks
```

### Benchmarks

Compare SQLite vs PostgreSQL performance:

```bash
# Multi-process stress test (the definitive proof)
python3 scripts/benchmark_multiprocess.py

# Library scan + playback simulation
python3 scripts/benchmark_plex_stress.py

# Concurrent writers test
python3 scripts/benchmark_locking.py

# Query performance comparison
python3 scripts/benchmark_compare.py
```

The stack protection test validates all protection layers by simulating low-stack conditions without running Plex.

## Troubleshooting

```bash
# Check PostgreSQL
pg_isready -h localhost -U plex

# Check logs (macOS)
tail -50 /tmp/plex_redirect_pg.log

# Check logs (Docker)
docker-compose logs -f plex

# Analyze fallbacks
./scripts/analyze_fallbacks.sh
```

### Common Issues

**Plex won't start**: Check if PostgreSQL is running and accessible.

**Database errors**: Ensure the schema exists: `psql -U plex -d plex -c "CREATE SCHEMA IF NOT EXISTS plex;"`

**Docker port conflict**: Change port in `docker-compose.yml` if 8080 is in use.

## License

MIT - See [LICENSE](LICENSE)

---
*Unofficial project, not affiliated with Plex Inc. Use at your own risk.*

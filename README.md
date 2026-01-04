# plex-postgresql

**Run Plex Media Server with PostgreSQL instead of SQLite.**

A shim library that intercepts Plex's SQLite calls and redirects them to PostgreSQL. Zero Plex modifications required.

| Platform | Status |
|----------|--------|
| macOS | ✅ Working |
| Linux | ⚠️ Untested |

## Quick Start (macOS)

### 1. Setup PostgreSQL

```bash
brew install postgresql@15
brew services start postgresql@15

createuser -U postgres plex
createdb -U postgres -O plex plex
psql -U postgres -c "ALTER USER plex PASSWORD 'plex';"
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

## Quick Start (Linux) - Untested

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

### Docker

```bash
docker-compose up -d
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

## How It Works

```
macOS: Plex → SQLite API → DYLD_INTERPOSE shim → SQL Translator → PostgreSQL
Linux: Plex → SQLite API → LD_PRELOAD shim    → SQL Translator → PostgreSQL
```

The shim intercepts all `sqlite3_*` calls, translates SQL syntax (placeholders, functions, types), and executes on PostgreSQL via libpq.

## Troubleshooting

```bash
# Check PostgreSQL
pg_isready -h localhost -U plex

# Check logs
tail -50 /tmp/plex_redirect_pg.log

# Analyze fallbacks
./scripts/analyze_fallbacks.sh
```

## License

MIT - See [LICENSE](LICENSE)

---
*Unofficial project, not affiliated with Plex Inc. Use at your own risk.*

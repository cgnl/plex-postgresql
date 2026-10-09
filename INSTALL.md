# Installation Guide

Simple install steps for Docker, macOS, and Linux.

---

## 📦 Choose Your Platform

**Latest Release:** [v1.0.0](https://github.com/cgnl/plex-postgresql/releases/tag/v1.0.0)

Release assets are zip-only:
- `plex-postgresql-v1.0.0-macos.zip`
- `plex-postgresql-v1.0.0-linux.zip`

- **[Docker](#docker-all-platforms)** - Easiest option, works on Linux/macOS/Windows
- **[macOS](#macos-native)** - Native installation for Apple Silicon
- **[Linux](#linux-native)** - Native installation for production servers

---

## 🐳 Docker (All Platforms)

**Best for:** quick setup, testing, and multi-platform deployments

### Prerequisites
- Docker with the Compose plugin (`docker compose`) installed
- 2GB RAM minimum
- 10GB disk space

### Fresh Installation (No Existing Plex Data)

```bash
# 1. Clone repository
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# 2. Start containers
docker compose up -d

# 3. Check startup progress
docker compose logs -f plex

# Wait for: "PostgreSQL initialization complete"
# Then open: http://localhost:8080/web
```

**Setup steps:**
1. Open http://localhost:8080/web in browser
2. Sign in with Plex account to claim server
3. Add libraries through web interface
4. Done. Your library data is now in PostgreSQL.

**Use a different host port (if local Plex already uses 32400):**
```bash
PLEX_HOST_PORT=32410 PLEX_DLNA_PORT=32471 docker compose up -d
# Web UI: http://localhost:32410/web
```

**What happens:**
- ✅ PostgreSQL 15 and Plex start automatically
- ✅ An empty PostgreSQL schema is created
- ✅ Current shim fixes are active
- ✅ Works on x86_64 and ARM64
- ✅ Required folders are created
- ✅ Default test media mount: `./fixtures/media:/media:ro`

To use real libraries, set `PLEX_MEDIA_PATH` in `.env`:

```bash
# macOS example
PLEX_MEDIA_PATH=/Volumes/media

# Linux example
PLEX_MEDIA_PATH=/srv/media
```

Then recreate:
```bash
docker compose up -d
docker compose logs -f plex | grep -E "Media mount|WARNING: Media mount"
```

### Migration from Existing Plex Database

If you already have a Plex SQLite library, follow these steps:

**1. Edit `docker-compose.yml`**

Uncomment and update the database mount path:

```yaml
volumes:
  - plex_config:/config
  - postgres_socket:/var/run/postgresql
  # Uncomment and edit the path below:
  - "/path/to/your/Plex Media Server/Plug-in Support/Databases:/source-db:ro"
```

**Platform-specific paths:**

| Platform | Default Path |
|----------|-------------|
| **macOS** | `${HOME}/Library/Application Support/Plex Media Server/Plug-in Support/Databases` |
| **Linux** | `/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases` |
| **Windows** | `C:\Users\YourName\AppData\Local\Plex Media Server\Plug-in Support\Databases` |

**Example (macOS):**
```yaml
- "${HOME}/Library/Application Support/Plex Media Server/Plug-in Support/Databases:/source-db:ro"
```

**2. Start containers with migration**

```bash
docker compose up -d

# Monitor migration progress
docker compose logs -f plex | grep -E "migration|Migration"
```

**Migration process:**
- Detects the SQLite database automatically
- Migrates all tables
- Shows progress per table
- Keeps source SQLite unchanged (read-only mount)
- Usually takes a few minutes, depending on library size

### Configuration

Default connection in the included compose files uses TCP:

```yaml
environment:
  - PLEX_PG_HOST=postgres
  - PLEX_PG_PORT=5432
  - PLEX_PG_DATABASE=plex
  - PLEX_PG_USER=plex
  - PLEX_PG_PASSWORD=plex
  - PLEX_PG_SCHEMA=plex
  - PLEX_PG_POOL_SIZE=50
  - PLEX_PG_IDLE_TIMEOUT=300  # seconds, default 300
  - PLEX_PG_OPENSSL_ARMCAP=0  # optional ARM fallback for container/VM SIGILL crashes
  - PLEX_PG_LOG_LEVEL=DEBUG  # 0=ERROR, 1=INFO, 2=DEBUG
  - PLEX_PG_LOG_TRUNCATE_ON_START=1  # clear log file on startup
  - PLEX_PG_SKIP_CLEAR_BINDINGS_FINALIZED=1  # default ON; set 0 to disable (debug only)
  - PLEX_PG_VALIDATE_OUTPUT=off  # off|sample|all (default: off)
  - PLEX_PG_VALIDATE_OUTPUT_SAMPLE_PCT=5  # only used when mode=sample
```

**To use Unix socket mode instead:**
```yaml
  - PLEX_PG_HOST=/var/run/postgresql
```

### Mount Media Libraries

Edit `docker-compose.yml` to add your media:

```yaml
volumes:
  - plex_config:/config
  - postgres_socket:/var/run/postgresql
  - /path/to/movies:/movies:ro
  - /path/to/tv:/tv:ro
```

### Manage Containers

```bash
# View logs
docker compose logs -f plex

# Restart Plex
docker compose restart plex

# Stop everything
docker compose down

# Stop and remove data (fresh start)
docker compose down -v

# Update to latest version
git pull
docker compose build --no-cache
docker compose up -d
```

### Troubleshooting

**Plex shows "Maintenance" for a long time:**
- This is normal on first start (database migrations)
- Check logs: `docker compose logs plex | tail -50`
- Wait a few minutes for initialization

**Port 8080 already in use:**
```yaml
ports:
  - "8081:32400"  # Change 8080 to 8081
```

**PostgreSQL connection failed:**
```bash
# Check PostgreSQL is healthy
docker compose ps
# Should show "healthy" status for plex-postgres
```

---

## 🍎 macOS Native

**Best for:** production macOS setups

### Prerequisites
- macOS ARM64 (M1/M2/M3 Apple Silicon)
- Plex Media Server 1.40+ installed
- PostgreSQL 15.x
- Homebrew (for PostgreSQL)

### Option 1: Pre-compiled ZIP (Recommended)

**Latest Release:** [v1.0.0](https://github.com/cgnl/plex-postgresql/releases/tag/v1.0.0)

**1. Setup PostgreSQL**

```bash
# Install PostgreSQL
brew install postgresql@15
brew services start postgresql@15

# Create database and user
createuser plex
createdb -O plex plex
psql -d plex -c "ALTER USER plex PASSWORD 'plex';"
```

**2. Download and Install**

```bash
# Download latest macOS zip
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.0.0/plex-postgresql-v1.0.0-macos.zip -o /tmp/plex-pg-macos.zip

# Extract
mkdir -p /tmp/plex-pg-macos && cd /tmp/plex-pg-macos
unzip /tmp/plex-pg-macos.zip

# Stop Plex and install wrappers
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
```

**What the installer does:**
- Copies `db_interpose_pg.dylib` into `Plex Media Server.app`
- Backs up original server/scanner binaries (`.original`)
- Installs a bash wrapper for the Server (`DYLD_INSERT_LIBRARIES`)
- Patches the Scanner binary with `@loader_path` dylib injection
- Syncs schema migration state between PostgreSQL and SQLite

After a Plex update, re-run `install_wrappers.sh` to re-install.

**3. Start Plex**

```bash
open "/Applications/Plex Media Server.app"
```

### Option 2: Build from Source

```bash
# Install dependencies
brew install postgresql@15

# Clone and build
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make

# Stop Plex and install wrappers
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
```

### Configuration

Set environment variables in the start script or shell:

```bash
export PLEX_PG_HOST=localhost
export PLEX_PG_PORT=5432
export PLEX_PG_DATABASE=plex
export PLEX_PG_USER=plex
export PLEX_PG_PASSWORD=plex
export PLEX_PG_SCHEMA=plex
export PLEX_PG_POOL_SIZE=50
export PLEX_PG_IDLE_TIMEOUT=300  # seconds, default 300
```

### Verify Installation

Check the logs:

```bash
tail -f /tmp/plex_redirect_pg.log
```

You should see:
```
[SHIM_INIT] Constructor starting (macOS)...
[SHIM_INIT] All modules initialized
=== Plex PostgreSQL Interpose Shim loaded (macOS) ===
PostgreSQL config: plex@localhost:5432/plex (schema: plex)
```

Test endpoints:
```bash
# Should return HTTP 200 with your libraries
curl -s http://localhost:32400/library/sections | head -10
```

---

## 🐧 Linux Native

**Best for:** production Linux servers

### Prerequisites
- Linux x86_64 or ARM64 (aarch64)
- Plex Media Server 1.40+ installed
- PostgreSQL 15.x
- Root access (sudo)

### Option 1: Pre-compiled ZIP (Recommended)

**Latest Release:** [v1.0.0](https://github.com/cgnl/plex-postgresql/releases/tag/v1.0.0)

**Available architectures:**
- ✅ x86_64 (Intel/AMD 64-bit) - `db_interpose_pg-linux-x86_64.so`
- ✅ ARM64 (aarch64) - `db_interpose_pg-linux-aarch64.so`

**1. Setup PostgreSQL**

```bash
# Install PostgreSQL (Debian/Ubuntu)
sudo apt update
sudo apt install postgresql-15

# Or RedHat/CentOS/Rocky
sudo yum install postgresql15-server
sudo postgresql-15-setup initdb
sudo systemctl start postgresql-15

# Create database and user
sudo -u postgres createuser plex
sudo -u postgres createdb -O plex plex
sudo -u postgres psql -c "ALTER USER plex PASSWORD 'yourpassword';"
```

**2. Download and Install**

```bash
# Download latest Linux zip
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.0.0/plex-postgresql-v1.0.0-linux.zip -o /tmp/plex-pg-linux.zip

# Extract
mkdir -p /tmp/plex-pg-linux
cd /tmp/plex-pg-linux
unzip /tmp/plex-pg-linux.zip

# Install shim binary
sudo mkdir -p /usr/local/lib/plex-postgresql
if [ "$(uname -m)" = "x86_64" ]; then
  sudo install -m 755 db_interpose_pg-linux-x86_64.so /usr/local/lib/plex-postgresql/db_interpose_pg.so
else
  sudo install -m 755 db_interpose_pg-linux-aarch64.so /usr/local/lib/plex-postgresql/db_interpose_pg.so
fi

# Stop Plex and install wrappers
sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh
```

**What the installer does:**
- ✅ Checks the Plex installation
- ✅ Backs up original binaries
- ✅ Migrates SQLite to PostgreSQL
- ✅ Installs wrapper scripts to `/usr/lib/plexmediaserver/`
- ✅ Wraps both `Plex Media Server` and `Plex Media Scanner`
- ✅ No Plex source changes needed

**3. Configure Connection**

Edit `/etc/default/plexmediaserver`:

```bash
sudo nano /etc/default/plexmediaserver
```

Add these lines:
```bash
# PostgreSQL connection
PLEX_PG_HOST=localhost
PLEX_PG_PORT=5432
PLEX_PG_DATABASE=plex
PLEX_PG_USER=plex
PLEX_PG_PASSWORD=yourpassword
PLEX_PG_SCHEMA=plex
PLEX_PG_POOL_SIZE=50
PLEX_PG_IDLE_TIMEOUT=300
```

**4. Start Plex**

```bash
sudo systemctl start plexmediaserver

# Check status
sudo systemctl status plexmediaserver

# View logs
sudo journalctl -u plexmediaserver -f
```

### Option 2: Build from Source

```bash
# Install dependencies (Debian/Ubuntu)
sudo apt install build-essential libsqlite3-dev libpq-dev postgresql-15

# Clone and build
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
make linux
sudo make install

# Install wrappers
sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh

# Configure and start (see Option 1 steps 3-4)
```

### Verify Installation

Check logs for shim initialization:

```bash
sudo journalctl -u plexmediaserver | grep -E "SHIM_INIT|PostgreSQL"
```

Expected output:
```
[SHIM_INIT] Constructor starting (Linux)...
[SHIM_INIT] All modules initialized
=== Plex PostgreSQL Interpose Shim loaded (Linux) ===
PostgreSQL config: plex@localhost:5432/plex (schema: plex)
```

### Uninstall

```bash
# Stop Plex
sudo systemctl stop plexmediaserver

# Run uninstaller (restores original binaries)
sudo ./scripts/uninstall_wrappers_linux.sh

# Start Plex (back to SQLite)
sudo systemctl start plexmediaserver
```

---

## 🔧 Advanced Configuration

### Performance Tuning

**Connection Pooling:**
```bash
export PLEX_PG_POOL_SIZE=100   # Default: 50
export PLEX_PG_IDLE_TIMEOUT=300  # Seconds before idle connections are reaped (default: 300)
```

**Unix Socket (7% faster than TCP):**
```bash
export PLEX_PG_HOST=/var/run/postgresql  # macOS: /tmp, Linux: /var/run/postgresql
# No PLEX_PG_PORT needed for Unix socket
```

**Logging Levels:**
```bash
export PLEX_PG_LOG_LEVEL=0  # 0=ERROR only (production)
export PLEX_PG_LOG_LEVEL=1  # 1=INFO (recommended)
export PLEX_PG_LOG_LEVEL=2  # 2=DEBUG (troubleshooting)
```

### Database Maintenance

```bash
# Vacuum database (weekly recommended)
psql -U plex -d plex -c "VACUUM ANALYZE;"

# Check database size
psql -U plex -d plex -c "SELECT pg_size_pretty(pg_database_size('plex'));"

# Backup database
pg_dump -U plex plex | gzip > plex_backup_$(date +%Y%m%d).sql.gz

# Restore database
gunzip -c plex_backup_20260113.sql.gz | psql -U plex plex
```

### Native SQLite export

Stop Plex and retain the original SQLite databases and `Preferences.xml`.
A normal data export is not a native Plex rollback. Native export additionally
requires the `Plex SQLite` executable and original companion database from the
same Plex build. Its complete schema, bootstrap data and migration history are
preserved while PostgreSQL supplies the current blob rows.

With the PostgreSQL connection configured through the usual `PGHOST`, `PGPORT`,
`PGUSER`, `PGDATABASE` and libpq authentication settings, run:

```bash
python3 scripts/export_pg_to_sqlite.py --schema plex \
  --output-dir /path/to/export \
  --sqlite-schema schema/sqlite_schema.sql \
  --native-plex-sqlite "/path/to/Plex SQLite" \
  --native-blobs-template /path/to/original/com.plexapp.plugins.library.blobs.db
```

Verify the exported pair in an isolated instance of the matching Plex build,
including library identity, watched state, search, artwork and decoded playback,
before installing it. Keep the original database pair and PostgreSQL backup.
`--native-rollback` remains unavailable until the complete acceptance matrix is
certified.

---

## 🆘 Troubleshooting

### Plex Won't Start

**Check logs:**
```bash
# macOS
tail -f /tmp/plex_redirect_pg.log

# Linux
sudo journalctl -u plexmediaserver -n 50

# Docker
docker compose logs plex --tail 50
```

**Common issues:**
- PostgreSQL not running: `brew/systemctl status postgresql`
- Wrong credentials: Check `PLEX_PG_*` environment variables
- Database doesn't exist: `createdb -O plex plex`
- Schema missing: `psql -U plex -d plex -c "CREATE SCHEMA plex;"`

### TV Shows Return HTTP 500

Latest builds include this fix. Verify it is active:

```bash
# Check logs for this message:
grep "DECLTYPE_AGGREGATE" /tmp/plex_redirect_pg.log

# Should see:
# DECLTYPE_AGGREGATE: col='max' OID=20 (BIGINT) -> returning TEXT to avoid SOCI bad_cast bug
```

If not present, ensure you're running a current release:
```bash
ls -la "/Applications/Plex Media Server.app/Contents/MacOS/db_interpose_pg.dylib"  # macOS
ls -la /usr/local/lib/plex-postgresql/db_interpose_pg.so  # Linux
```

### Migration Failed

**Check source database exists:**
```bash
ls -la "/Users/$(whoami)/Library/Application Support/Plex Media Server/Plug-in Support/Databases/"
```

**Re-run migration manually:**
```bash
# macOS/Linux
./scripts/migrate_sqlite_to_pg.sh

# Docker - restart containers
docker compose down
docker compose up -d
docker compose logs -f plex | grep migration
```

### Performance Issues

**Enable Unix socket (faster):**
```bash
export PLEX_PG_HOST=/tmp  # or /var/run/postgresql
```

**Increase pool size:**
```bash
export PLEX_PG_POOL_SIZE=100
export PLEX_PG_IDLE_TIMEOUT=300  # seconds before idle connections are reaped
```

**Check PostgreSQL performance:**
```bash
psql -U plex -d plex -c "SELECT * FROM pg_stat_activity WHERE datname='plex';"
```

---

## 📚 Additional Resources

- **GitHub Repository:** https://github.com/cgnl/plex-postgresql
- **Latest Release:** https://github.com/cgnl/plex-postgresql/releases/latest
- **Issue Tracker:** https://github.com/cgnl/plex-postgresql/issues
- **Changelog:** [CHANGELOG.md](CHANGELOG.md)
- **Release Notes:** [RELEASE_NOTES.md](RELEASE_NOTES.md)

---

## ✅ Verification Checklist

After installation, verify these work:

- [ ] Docker setup: Plex web interface accessible (http://localhost:8080/web, or your `PLEX_HOST_PORT`)
- [ ] Native setup: Plex web interface accessible (http://localhost:32400/web)
- [ ] Libraries visible and load correctly
- [ ] Playback works
- [ ] TV shows endpoint returns HTTP 200 (not 500)
- [ ] No `std::bad_cast` errors in logs
- [ ] PostgreSQL receiving queries: `psql -U plex -d plex -c "SELECT COUNT(*) FROM plex.metadata_items;"`
- [ ] Shim loaded: Check logs for "Plex PostgreSQL Interpose Shim loaded"

If all checked ✅ - **Installation successful!** 🎉

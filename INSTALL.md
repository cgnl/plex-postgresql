# Installation Guide

## Quick Install (macOS)

```bash
# 1. Extract the release
tar -xzf plex-postgresql-v0.8.0-macos.tar.gz
cd plex-postgresql-v0.8.0

# 2. Setup PostgreSQL (if not already done)
brew install postgresql@15
brew services start postgresql@15
createuser plex
createdb -O plex plex
psql -d plex -c "ALTER USER plex PASSWORD 'plex';"
psql -U plex -d plex -c "CREATE SCHEMA plex;"

# 3. Stop Plex and install the shim
pkill -x "Plex Media Server"
./scripts/install_wrappers.sh

# 4. Start Plex
open "/Applications/Plex Media Server.app"
```

## Quick Install (Linux)

```bash
# 1. Extract the release
tar -xzf plex-postgresql-v0.8.0-macos.tar.gz
cd plex-postgresql-v0.8.0

# 2. Setup PostgreSQL
sudo apt install postgresql-15
sudo -u postgres createuser plex
sudo -u postgres createdb -O plex plex
sudo -u postgres psql -c "ALTER USER plex PASSWORD 'plex';"

# 3. Stop Plex and install
sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh

# 4. Configure and start
# Add to /etc/default/plexmediaserver:
#   PLEX_PG_HOST=localhost
#   PLEX_PG_DATABASE=plex
#   PLEX_PG_USER=plex
#   PLEX_PG_PASSWORD=plex
sudo systemctl start plexmediaserver
```

## Docker

See `docker-compose.yml` in the repository or use:

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
docker-compose up -d
```

## Migration from SQLite

To migrate an existing Plex library:

```bash
./scripts/migrate_sqlite_to_pg.sh
```

## Configuration

Set these environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `PLEX_PG_HOST` | localhost | PostgreSQL host (or `/tmp` for Unix socket) |
| `PLEX_PG_PORT` | 5432 | PostgreSQL port |
| `PLEX_PG_DATABASE` | plex | Database name |
| `PLEX_PG_USER` | plex | Database user |
| `PLEX_PG_PASSWORD` | (empty) | Database password |
| `PLEX_PG_SCHEMA` | plex | Schema name |

## Uninstall

```bash
# macOS
pkill -x "Plex Media Server"
./scripts/uninstall_wrappers.sh

# Linux
sudo systemctl stop plexmediaserver
sudo ./scripts/uninstall_wrappers_linux.sh
```

## Verify Installation

Check the logs:

```bash
# macOS
tail -f /tmp/plex_redirect_pg.log

# Docker
docker-compose logs -f plex
```

You should see:
```
[SHIM_INIT] Constructor starting...
[SHIM_INIT] All modules initialized
```

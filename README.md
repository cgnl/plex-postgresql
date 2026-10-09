# plex-postgresql

**Run Plex Media Server with PostgreSQL instead of SQLite.**

🇩🇪 [Deutsch](README.de.md) | 🇳🇱 [Nederlands](README.nl.md) | 🇮🇪 [Gaeilge](README.ga.md) | 🟢 [Esperanto](README.eo.md) | 🇪🇸 [Espa&ntilde;ol](README.es.md)

A small shim library that catches Plex SQLite calls and sends them to PostgreSQL. You do not need to change Plex source code.

## Current stability candidate

`test/stability-native-e2e-20261009` is a prerelease test branch. **Production
promotion is blocked:** the final candidate's full native matrix and 5h50 soak
have not completed. Earlier passing probes used intermediate artifacts and do
not certify this candidate. The published release below is release history,
not evidence that the current branch has passed these gates.

The acceptance matrix is **LinuxServer and PlexInc × native amd64 and arm64 ×
PostgreSQL 15 and 18** (eight combinations), with 100 restart cycles per lane
and a separate 5-hour-50-minute soak using the same immutable image digest.
See [release gates and known gaps](docs/release-readiness.md) and
[recorded validation evidence](docs/stability-validation-2026-10-09.md).

## Latest published release: [v1.3.20](https://github.com/cgnl/plex-postgresql/releases/tag/v1.3.20)

Published on 5 October 2026. The stability fixes and real-media acceptance tests
on the candidate branch are not included in this release.

### Runtime architecture introduced in v1.2.0

**100% Rust shim runtime** — the entire interpose layer is now pure Rust. All C runtime code has been eliminated. The shim compiles to a single static library linked into the dylib/so.

- 🆕 **Full Rust runtime:** all interpose, column access, step execution, and connection management in Rust — eliminating memory leaks, buffer overflows, and use-after-free bugs by design
- 🆕 **Module architecture:** monolithic files split into focused submodules for maintainability
- 🔧 **Deadlock fixes:** recursive connection mutex, reduced lock hold times, lock-free logging fallback
- 🔧 **Stack safety:** heap-allocated thread-local buffers for Plex's 544K worker thread stacks
- ✅ **712 tests passing** across unit, integration, and compatibility suites

[📥 Download v1.2.0](https://github.com/cgnl/plex-postgresql/releases/tag/v1.2.0) | [📋 Full Changelog](CHANGELOG.md)

Linux and macOS release zips are built by GitHub Actions on tag push via `.github/workflows/release-linux-artifacts.yml` and `.github/workflows/release-macos-artifacts.yml`.
Pull requests and `main`/`develop` pushes run `.github/workflows/ci.yml` (script validation + Linux amd64 build check + full test suite + FFI header verification).
The current `.github/workflows/docker-publish.yml` probes uniquely tagged GHCR
candidates and blocks production promotion. The image repositories are:
- `ghcr.io/cgnl/plex-postgresql-linuxserver`
- `ghcr.io/cgnl/plex-postgresql-plexinc`

**Available for:** macOS ARM64 • Linux x86_64 • Linux ARM64 • Docker (multi-arch)

### Quick Install

**macOS:**
```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.0.0/plex-postgresql-v1.0.0-macos.zip \
  -o /tmp/plex-pg-macos.zip
mkdir -p /tmp/plex-pg-macos && cd /tmp/plex-pg-macos
unzip /tmp/plex-pg-macos.zip
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
```

**Linux (x86_64):**
```bash
sudo curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.0.0/plex-postgresql-v1.0.0-linux.zip \
  -o /tmp/plex-postgresql-linux.zip
sudo unzip -j /tmp/plex-postgresql-linux.zip db_interpose_pg-linux-x86_64.so -d /usr/local/lib
sudo mv /usr/local/lib/db_interpose_pg-linux-x86_64.so /usr/local/lib/db_interpose_pg.so
# Then configure LD_PRELOAD in systemd service
```

**Docker:**
```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql
docker compose up -d
```

See detailed installation instructions below for each platform.

## Platform Support

| Platform | Architecture | Status |
|----------|-------------|---------|
| macOS | ARM64 (M1/M2/M3/M4) | ✅ Production tested |
| Linux | x86_64 | ✅ Pre-compiled binary |
| Linux | ARM64 | ✅ Pre-compiled binary |
| Docker | x86_64 + ARM64 | ✅ Multi-arch support |

## Why PostgreSQL?

SQLite works well for many Plex setups, but it can lock the full database during writes.

- **Fewer locks:** scans and playback can run together more smoothly.
- **Better for remote media:** useful for rclone and similar cloud setups.
- **Better at scale:** handles large libraries more easily.
- **Better tools:** backups and checks are easier with standard PostgreSQL tools.

## Benchmarks

In one stress test (Plex + Kometa + PMM + 4 streams), **82% of SQLite writes failed**. PostgreSQL had **zero errors**.

| Metric | SQLite | PostgreSQL |
|--------|--------|------------|
| Write Errors (15s test) | 8,019,177 (82%) | **0** |
| Shim overhead (cached) | — | 0.11 µs (<1%) |

Full results and how to run them: **[wiki/Benchmarks](https://github.com/cgnl/plex-postgresql/wiki/Benchmarks)**

## Migration & Maintenance

```bash
./scripts/migrate_sqlite_to_pg.sh   # SQLite → PostgreSQL
./scripts/migrate_pg_to_sqlite.sh   # PostgreSQL → SQLite (beta)
./scripts/doctor.sh                  # Check and fix schema + data
```

`doctor.sh` checks for missing tables, functions, and triggers, then fixes them. It also finds common data issues (self-parent rows, cross-section parents, orphan seasons). Data fixes ask for confirmation unless you use `--fix`.

```
$ ./scripts/doctor.sh
=== plex-postgresql doctor ===

Tables:
  maintenance_control                        OK
Functions:
  prevent_self_referential_parent()          OK
  maybe_cleanup_statistics()                 MISSING → FIXED
Triggers:
  trg_clean_statistics_resources             MISSING → FIXED
Data:
  self-referential parent_id                 OK
  orphan seasons (no parent)                 2 rows
  Fix them? [y/N]: y
  fixing orphan seasons... 2 rows
```

Flags: `--check` (only report, don't fix anything), `--fix` (fix everything without asking).

## Quick Start (Docker)

This is the easiest setup. It works on **Linux, macOS, and Windows**.

### Fresh Installation (No Existing Plex Database)

```bash
git clone https://github.com/cgnl/plex-postgresql.git
cd plex-postgresql

# Start Plex + PostgreSQL
docker compose up -d

# Check logs
docker compose logs -f plex
```

**Setup:**
1. Open http://localhost:8080/web (or your `PLEX_HOST_PORT`)
2. Claim your server with Plex account (or set `PLEX_CLAIM` in `docker-compose.yml` for headless claim)
3. Add libraries via web interface
4. Done. Your library data now lives in PostgreSQL.

**Run on a different local port (recommended when local Plex is already running):**
```bash
PLEX_HOST_PORT=32410 PLEX_DLNA_PORT=32471 docker compose up -d
```
Then use `http://localhost:32410/web`.

For local test media, the default Docker mount is `./fixtures/media:/media:ro`.
For real libraries, set `PLEX_MEDIA_PATH` in `.env`:

```bash
# Example (macOS)
PLEX_MEDIA_PATH=/Volumes/media

# Example (Linux)
PLEX_MEDIA_PATH=/srv/media
```

Then restart:
```bash
docker compose up -d
docker compose logs -f plex | grep -E "Media mount|WARNING: Media mount"
```

Docker mode does **not** write `db_interpose_pg.dylib` into your local `Plex Media Server.app`.  
Do **not** run `scripts/install_wrappers.sh` for this workflow.

**What happens:**
- ✅ PostgreSQL schema auto-created (empty)
- ✅ Fresh install works out of the box (v1.0.0 fixes blobs.db crash)
- ✅ Claim flow works with both linuxserver and plexinc images
- ✅ Multi-arch support (x86_64 + ARM64)
- ✅ All directories pre-created (Plug-ins, Metadata, Cache)

### Migration from Existing SQLite Database

If you already have a Plex library in SQLite, do this:

1. **Edit `docker-compose.yml`**, uncomment and update the source database path:
   ```yaml
   volumes:
     - plex_config:/config
     - postgres_socket:/var/run/postgresql
     # Uncomment and edit this line:
     - "/path/to/your/Plex Media Server/Plug-in Support/Databases:/source-db:ro"
   ```

2. **Platform-specific paths:**
   - **macOS**: `"${HOME}/Library/Application Support/Plex Media Server/Plug-in Support/Databases:/source-db:ro"`
   - **Linux**: `"/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases:/source-db:ro"`
   - **Windows**: `"C:/Users/YourName/AppData/Local/Plex Media Server/Plug-in Support/Databases:/source-db:ro"`

3. **Start containers:**
   ```bash
   docker compose up -d
   ```

4. **Monitor migration:**
   ```bash
   docker compose logs -f plex | grep -E "migration|Migration"
   ```

**Migration includes:**
- ✅ Detect SQLite database automatically
- ✅ Move full library data (tables, metadata, posters)
- ✅ Move `blobs.db` data (thumbnails and artwork)
- ✅ Keep source SQLite unchanged (read-only mount)
- ✅ Update PostgreSQL sequences automatically
- ✅ Show progress per table

### Configuration

Default PostgreSQL connection (TCP in the included compose files):
```yaml
environment:
  - PLEX_PG_HOST=postgres
  - PLEX_PG_PORT=5432
  - PLEX_PG_DATABASE=plex
  - PLEX_PG_USER=plex
  - PLEX_PG_PASSWORD=plex
  - PLEX_PG_SCHEMA=plex
  - PLEX_PG_POOL_SIZE=50
  - PLEX_PG_POOL_MAX=100
  - PLEX_PG_OPENSSL_ARMCAP=0  # optional ARM fallback for container/VM SIGILL crashes
  - PLEX_PG_LOG_LEVEL=DEBUG  # 0=ERROR, 1=INFO, 2=DEBUG
  - PLEX_PG_LOG_TRUNCATE_ON_START=1  # clear log file on startup
  - PLEX_PG_SKIP_CLEAR_BINDINGS_FINALIZED=1  # default ON; set 0 to disable (debug only)
  - PLEX_PG_VALIDATE_OUTPUT=off  # off|sample|all (default: off)
  - PLEX_PG_VALIDATE_OUTPUT_SAMPLE_PCT=5  # only used when mode=sample
```

To use a Unix socket instead of TCP:
```yaml
environment:
  - PLEX_PG_HOST=/var/run/postgresql
```

Mount your media libraries:
```yaml
volumes:
  - /path/to/movies:/movies:ro
  - /path/to/tv:/tv:ro
```

## Quick Start (macOS)

Use the latest macOS zip and run the wrapper installer. The installer copies the shim dylib into `Plex Media Server.app`, patches the binaries, and sets up the wrapper script. Everything lives inside the Plex app bundle.

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.0.0/plex-postgresql-v1.0.0-macos.zip -o /tmp/plex-pg-macos.zip
mkdir -p /tmp/plex-pg-macos && cd /tmp/plex-pg-macos
unzip /tmp/plex-pg-macos.zip

pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/install_wrappers.sh
open "/Applications/Plex Media Server.app"
```

After a Plex update, re-run `install_wrappers.sh` to re-install the shim.

To uninstall:

```bash
pkill -f "Plex Media Server" 2>/dev/null || true
./scripts/uninstall_wrappers.sh
```

## Quick Start (Linux Native)

Use the latest Linux zip and install the binary for your CPU.

```bash
curl -L https://github.com/cgnl/plex-postgresql/releases/download/v1.0.0/plex-postgresql-v1.0.0-linux.zip -o /tmp/plex-pg-linux.zip
mkdir -p /tmp/plex-pg-linux
cd /tmp/plex-pg-linux
unzip /tmp/plex-pg-linux.zip

sudo mkdir -p /usr/local/lib/plex-postgresql
if [ "$(uname -m)" = "x86_64" ]; then
  sudo install -m 755 db_interpose_pg-linux-x86_64.so /usr/local/lib/plex-postgresql/db_interpose_pg.so
else
  sudo install -m 755 db_interpose_pg-linux-aarch64.so /usr/local/lib/plex-postgresql/db_interpose_pg.so
fi

sudo systemctl stop plexmediaserver
sudo ./scripts/install_wrappers_linux.sh
sudo systemctl start plexmediaserver
```

To uninstall:

```bash
sudo systemctl stop plexmediaserver
sudo ./scripts/uninstall_wrappers_linux.sh
```

For full steps (PostgreSQL setup, environment variables, troubleshooting), see `INSTALL.md`.

## Migration from SQLite

Use this command to migrate an existing Plex library to PostgreSQL:

```bash
# macOS / Linux
./scripts/migrate_sqlite_to_pg.sh

# The script migrates:
# - library.db (metadata, media items, tags, etc.)
# - blobs.db (thumbnails, artwork, posters)
```

**What gets migrated:**
- All 34+ tables from library.db
- All blobs from blobs.db (thumbnails, artwork) via hex encoding
- Sequences automatically updated
- No Python dependencies required

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `PLEX_PG_HOST` | localhost | PostgreSQL host (or socket directory like `/tmp`) |
| `PLEX_PG_PORT` | 5432 | PostgreSQL port |
| `PLEX_PG_DATABASE` | plex | Database name |
| `PLEX_PG_USER` | plex | Database user |
| `PLEX_PG_PASSWORD` | (empty) | Database password |
| `PLEX_PG_SCHEMA` | plex | Schema name |
| `PLEX_PG_POOL_SIZE` | 50 | Initial connection pool size |
| `PLEX_PG_POOL_MAX` | (inherits `PLEX_PG_POOL_SIZE`) | Runtime pool cap; auto-aligned to PostgreSQL `max_connections` at startup |
| `PLEX_PG_IDLE_TIMEOUT` | 300 | Seconds before idle connections are reaped |
| `PLEX_PG_OPENSSL_ARMCAP` | 0 (Docker ARM only) | Override `OPENSSL_armcap` for Plex process to avoid ARM `SIGILL` on unsupported crypto instructions |
| `PLEX_PG_LOG_LEVEL` | 1 | 0=ERROR, 1=INFO, 2=DEBUG |
| `PLEX_PG_RETRY_DELAYS` | 500,1000,2000,3000,4000 | PG reconnect backoff in ms (comma-separated, max 10 values) |
| `PLEX_PG_SKIP_CLEAR_BINDINGS_FINALIZED` | 1 | Skip `sqlite3_clear_bindings` for recently finalized statements to avoid UAF crashes (set 0 to disable) |

### Unix Socket vs TCP

For local PostgreSQL, Unix sockets are ~5-6% faster than TCP:

```bash
# Use Unix socket (recommended for local PostgreSQL)
export PLEX_PG_HOST=/tmp  # or /var/run/postgresql on Linux

# Use TCP (required for remote PostgreSQL)
export PLEX_PG_HOST=localhost
```

The speed difference is small. The main win is fewer database locks.

## How It Works

The shim catches `sqlite3_*` calls, rewrites SQLite SQL to PostgreSQL SQL, and runs it through libpq.

```
Layer 4+3: C interposer (~9,400 lines)        — fishhook, DYLD_INTERPOSE, LD_PRELOAD
Layer 2:   Rust PG modules (hybrid C/Rust)     — pool, statement, cache, config, logging
Layer 1:   Rust SQL translator (sqlparser-rs)   — full AST-based SQLite → PostgreSQL translation
```

**Streaming mode** (v0.9.28+): READ queries use PostgreSQL's single-row streaming (`PQsetSingleRowMode`) to fetch results row by row instead of loading the entire result set into memory. This is always on with automatic fallback to eager fetch if streaming fails. Each streaming query gets exclusive use of its connection — other queries automatically use a different connection from the pool.

More technical details are in **[wiki/How It Works](https://github.com/cgnl/plex-postgresql/wiki/How-It-Works)**.

Translator scope, coverage, and known gaps are tracked in `docs/translator/README.md`.

## Testing

The native container runner creates a real movie library with **Big Buck Bunny
(640x360 H.264 with audio, CC BY 3.0)** and a TV library with **The Beverly
Hillbillies S01E01 and S01E02** (the source uploaders mark these copies Public
Domain). It checks library identities, server file delivery, byte-range seeking,
and full video/audio decoding during candidate smoke. The 5h50 soak decodes
20-second HTTP samples on each iteration. Native testing is in progress; the
complete GitHub matrix has not passed. These checks do not yet certify browser
playback or server-side transcoding.

Fixture sources, attribution, evidence and runner instructions are in
[runtime E2E](docs/runtime-e2e.md). The proposed wiki page is
[Stability Testing](docs/wiki-stability-testing.md).

```bash
make unit-test       # Focused Rust regression suites
make ci-test         # CI-safe regression subset
cargo test           # Full Rust suite — in rust/plex-pg-core/
make ffi-header      # Regenerate include/plex_pg_core_ffi.h from rust/plex-pg-abi/
make ffi-header-check  # Verify generated header is up to date
make benchmark       # Shim micro-benchmarks
```

`include/plex_pg_core_ffi.h` is generated from the small ABI contract crate in `rust/plex-pg-abi/`.  
`include/sql_translator.h` is the thin translator compatibility wrapper used by the legacy C ABI surface.  
Legacy shim headers now live under `include/legacy/`; the old `src/*.h` and `src/interpose/*.h` paths remain as compatibility wrappers only.

### Exception Parity (macOS)

Build the shim first, then run the harness against the built dylib:

```bash
make
cargo build --release --bin exception_parity_test --manifest-path rust/plex-pg-core/Cargo.toml
PLEX_PG_DISABLE_SHIM_INIT=1 ./rust/plex-pg-core/target/release/exception_parity_test
```

Optional terminate-path check (currently skipped; requires a real C++ exception object):

```bash
PLEX_PG_DISABLE_SHIM_INIT=1 ./rust/plex-pg-core/target/release/exception_parity_test --terminate
```

### Build Notes

If `libpq` is not in the default search path, set:

```bash
export PLEX_PG_LIBPQ_DIR=/path/to/libpq
```

### Exception ABI Assumptions (macOS)

Exception `what()` extraction assumes the Itanium C++ ABI layout for `std::exception` and the
vtable slot used by `what()` (slot 2) as implemented by libc++ on macOS.

## Memory Tracking

The shim includes opt-in allocation tracking to monitor memory usage and detect leaks. Disabled by default (zero overhead).

| Environment Variable | Effect |
|---|---|
| `PLEX_PG_ALLOC_TRACK=1` | Logs shim memory summary every 60s: live/peak bytes, alloc/free counts |
| `PLEX_PG_ALLOC_TRACE=1` | Same as TRACK + logs aggregate live allocations/bytes from the Rust tracker |

Example output with `PLEX_PG_ALLOC_TRACE=1`:
```
SHIM_ALLOC: live=3424KB peak=3502KB allocs=19092 frees=22269 total_alloc=26800KB total_freed=23376KB
SHIM_ALLOC_TRACE: 17 live allocations, 1414096 bytes total (per-site file:line not available in Rust backend)
```

If `live` keeps growing over hours, there's a leak. The Rust backend currently reports totals, not per-site file:line breakdowns.

## Troubleshooting

```bash
pg_isready -h localhost -U plex          # Check PostgreSQL
./scripts/doctor.sh                       # Check and fix schema + data
tail -50 /tmp/plex_redirect_pg.log       # Check logs (macOS)
docker compose logs -f plex              # Check logs (Docker)
```

More: **[wiki/Troubleshooting](https://github.com/cgnl/plex-postgresql/wiki/Troubleshooting)**

## License

MIT - See [LICENSE](LICENSE)

---
*Unofficial project, not affiliated with Plex Inc. Use at your own risk.*

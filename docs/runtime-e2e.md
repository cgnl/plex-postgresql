# Rust shared-shim runtime E2E

## Real movie and TV playback in native runners

`scripts/plex-container-e2e.sh` additionally creates real movie and TV libraries
on the isolated native Plex candidate. It downloads the checksum-pinned files
in `scripts/media-fixtures.json` on the host before entering the internal Docker
network. Plex gets temporary outbound access on an owned bootstrap network to
install its own H.264/AAC codecs, then that network is disconnected before
restarts and the soak. Decoder build, codec path and checksum are recorded.
Downloads are cached under `PLEX_E2E_MEDIA_CACHE_DIR` (default:
the system temporary directory's `plex-e2e-media-cache`). Media binaries are not
committed or included in evidence artifacts.

- **Big Buck Bunny (2008), 640x360 H.264/audio.** Blender Foundation,
  [CC BY 3.0](https://peach.blender.org/about/). Attribution: (c) copyright 2008,
  Blender Foundation / www.bigbuckbunny.org. Full credits are retained.
- **The Beverly Hillbillies, season 1, episodes 1 and 2.**
  [The Clampetts Strike Oil](https://archive.org/details/Beverly_Hillbillies_Ep01_The_Clampetts_Strike_Oil)
  and [Getting Settled](https://archive.org/details/Beverly_Hillbillies_Ep02_Getting_Settled)
  are marked Public Domain by their Internet Archive uploaders. This records the
  source's designation for these copies, not a rights claim for the entire series.

The verifier checks Plex scanner analysis, exact show/season/episode identities,
PostgreSQL hierarchy and absence of TV media in shadow SQLite, byte-range seeks,
full HTTP delivery hashes, and decoding the video and audio to the end with
Plex's bundled decoder. Candidate smoke checks full playback before restarts and
after live PostgreSQL recovery. The 5h50 job uses 20-second HTTP samples for
setup, recovery and every soak iteration to keep overhead inside its six-hour
runner limit. Every soak iteration decodes 20 seconds from
each real Plex HTTP stream. JSON and decoder logs preserve full and sample
evidence separately. The short generated AVI remains the fast fixture for all
100 restart cycles and four concurrent readers.

These are native direct-play delivery/decoding checks. They do not certify a
browser's player or server-side transcoding. The 5h50 soak refuses prior smoke
evidence without both `real_movie_and_tv_playback_verified=true` and
`real_media_full_decode_verified=true`; production promotion
remains blocked pending the complete matrix and release gates.

Run `bash scripts/run-runtime-e2e.sh` from any directory. The script builds the
existing Makefile shared shim with `--features interpose`, builds the Rust
`runtime_e2e` binary, creates a disposable PostgreSQL cluster, and removes it on
exit. This is an executable integration runner, not an addition to the unit suite.
It needs Cargo, the existing native libpq/SQLite link prerequisites, and PostgreSQL
`initdb`, `pg_ctl`, and `psql`. On macOS, Homebrew PostgreSQL 15 is discovered;
override with `RUNTIME_E2E_PG_BIN=/path/to/postgresql/bin`.

## CLI contract

```sh
bash scripts/run-runtime-e2e.sh [--shim /absolute/path/to/db_interpose_pg.dylib] [--reconnect]
```

`--shim` tests an already-built artifact without rebuilding it (Linux uses `.so`).
`--reconnect` additionally terminates only this disposable database/user's shim
backends and requires read/write recovery. CI always requests reconnect. Every
missing prerequisite, missing export, unexpected return code, failed case, or
isolation check returns nonzero; nothing is silently skipped. Cases print
`PASS case_name` or `FAIL case_name: evidence`, followed by a success summary only
when all requested cases pass. Independent cases continue after failures to
collect evidence; no runtime fixes are applied by this runner.

The binary accepts `runtime_e2e --shim ABSOLUTE_PATH [--reconnect]` or
`runtime_e2e --shim-env [--reconnect]` with `RUNTIME_E2E_SHIM=ABSOLUTE_PATH`, but
invoke the script: it supplies validated configuration and copies the executable
under the name `Plex Media Server`, required by the Linux constructor's process
filter. The script uses `--shim-env` because the current Linux filter scans for
the last slash across the entire command line, including arguments. Keeping the
absolute artifact path in environment prevents it from being mistaken for the
executable name.
The selected shared artifact is loaded with `dlopen` and `RTLD_GLOBAL`, matching
the global lookup scope of Linux preload and letting musl resolve real SQLite
dependencies through the shim's `RTLD_NEXT`; public SQLite ABI functions
are obtained from that artifact with `dlsym`. Linux exports `sqlite3_*`; macOS
exports the `my_sqlite3_*` replacement functions used by fishhook, with the same
SQLite ABI. No translator, private Rust runtime helper, or independent behavioral
model is called. The open function's `dladdr` provenance must match the requested
artifact. PostgreSQL creates the fixture and independently checks persistence;
all operations under test go through the shim. A PostgreSQL-only table proves
library opens are redirected rather than accidentally using shadow SQLite.
Linux linking retains the existing system SQLite dependency with
`--no-as-needed` so the constructor's `RTLD_NEXT` lookup has real SQLite to find.

## Coverage

- FTS rebuilds preserve PostgreSQL source metadata, logical FTS searches still
  read PostgreSQL, missing spellfix tables do not prevent skipped statements
  from preparing, and temporary SQLite tokenizer cleanup stays on SQLite.
- Normal Plex library filename/open and PostgreSQL-only table visibility.
- Real SQLite RTree construction/root-node access, with engine-internal backing
  tables kept off PostgreSQL and schema-qualified internal reads preserved.
- Prepared writes, bind/reset/clear reuse and re-prepare cache paths.
- Real PostgreSQL UNIQUE, NOT NULL, and foreign-key errors, SQLite primary and
  extended codes, nonempty diagnostics, and subsequent connection usability.
- Two simultaneously open handles and both API/SQL last-insert-rowid isolation.
- BEGIN, multiple writes, cross-handle invisibility, ROLLBACK,
  SAVEPOINT, ROLLBACK TO, RELEASE, and COMMIT persistence.
- Aborted COMMIT rejection, and the exported `sqlite3_exec` ABI's transaction,
  constraint-diagnostic, error-buffer release, and persistence behavior.
- NULL storage class, large signed int64, binary BLOB including NUL/high bytes,
  and integer-bound PostgreSQL boolean roundtrips with independent PG verification.
- Repeated `sqlite3_column_int`, `sqlite3_column_int64` and `sqlite3_column_double`
  reads of numeric results through the actual shared-artifact ABI.
- Prepared and `sqlite3_exec` maintenance no-ops (VACUUM, REINDEX and PRAGMA
  optimize), with independent verification that PostgreSQL library data is unchanged.
- Optional real backend disconnect/reconnect, terminal errors after interrupted
  streaming reads without replay or false DONE, and fail-closed active transaction
  interruption without partial writes. Only fixture-owned backends are terminated.
- SELECT result exhaustion requires ROW followed by DONE; checked separately so
  an exhaustion defect cannot hide transaction/type coverage.

## Isolation and CI

Inherited `PLEX_PG_*`, preload variables, and PostgreSQL service/default variables
are cleared. The local cluster listens only on its private temporary Unix socket.
A unique `runtime_e2e_*` database and login role are created with a database comment
marker. The driver checks explicit host/port/database/user/password/schema,
connection identity, comment marker, and temporary directory marker **before**
loading the shim. It rejects production names/defaults and remote hosts. The
driver deliberately refuses to reuse an existing schema.
Shim logs live inside the fixture directory; on failure the script emits the
tail of shim and local PostgreSQL logs before cleanup.

For a disposable CI PostgreSQL service only, set `RUNTIME_E2E_EXTERNAL_FIXTURE=1`
and all five `RUNTIME_E2E_ADMIN_HOST`, `RUNTIME_E2E_ADMIN_PORT`,
`RUNTIME_E2E_ADMIN_USER`, `RUNTIME_E2E_ADMIN_PASSWORD`, and
`RUNTIME_E2E_ADMIN_DATABASE` values. Host must be `127.0.0.1`; this is an explicit
attestation that the service is disposable, not permission to use production.
The admin user and database must also use `runtime_e2e_` names.
The script creates and drops only its uniquely named database/role. The workflow
uses separate PostgreSQL 15 and 18 service jobs and treats all failures as failures.

No new dependencies or C implementation are introduced. The existing Makefile
compiler invocation links the Rust static library into the shared artifact.

On musl Linux, only the E2E executable is built with `-crt-static` disabled:
musl's statically linked executable cannot `dlopen` the shared shim. The runner
preserves existing Rust flags and scopes this adjustment to its binary build;
it does not change production shim build settings.

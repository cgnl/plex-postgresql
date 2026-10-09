# Stability validation — 9 October 2026

This report records local validation evidence. Read alongside
[stability-assessment.md](stability-assessment.md) and
[runtime-e2e.md](runtime-e2e.md). This continuation adds no unit tests or
dependencies. Additional E2E runner/gate corrections are recorded below.

## Candidate identity

Both native probe result files identify the same LinuxServer ARM64 candidate:

- Candidate image SHA: `sha256:2c77eb3b7dd3d3d3182f825420e34a5d55a1d647e67c0eff71b9aefedf26a1b2`.
- Plex version: `1.43.4.10903-e5521bd8c`.
- Variant: `linuxserver`; architecture: `arm64`.

The image SHA identifies the native candidate, not a source commit or the macOS
shared-shim artifact.

## Observed results

| Existing evidence | Result |
| --- | --- |
| `/tmp/plex-tests-final-resumed.log` | Across 34 test-result summaries: **1,049 passed, 0 failed, 2 ignored**. Ignored tests are not passes. |
| `/tmp/plex-clippy-latest.log` | Compilation finished in the dev profile; no warnings or errors appear in this log. The command and flags are not recorded. |
| `/tmp/plex-runtime-maintenance-after.log` | **macOS ARM64 shared-shim runtime E2E: 12 cases passed, zero skips**, including maintenance no-ops and reconnect against isolated PostgreSQL. |
| `/tmp/plex-candidate-maintenance-evidence/result.json` | Earlier native ARM64 probe failed at `api-write-routing`, exit code `1`, `promotion_allowed: false`. |
| `/tmp/plex-candidate-maintenance-long-evidence/result.json` | Later native ARM64 probe failed at `first-start`, exit code `1`, `promotion_allowed: false`. |
| `/tmp/plex-native-crash-gate-evidence/result.json` | A subsequent run completed the empty-library API routing/persistence, restart, 80-second fresh-start PostgreSQL-unavailable observation and recovery smoke. It deliberately exits `1` at `workload-smoke-complete-certification-incomplete`; promotion remains forbidden. |

The runtime E2E log tests `db_interpose_pg.dylib` through the `my_sqlite3_*` ABI;
it does not establish native Plex startup or workload success. It includes a
macOS linker warning: an object built for macOS 26.2 was linked for 26.0. The
administrator-terminated connection precedes `PASS reconnect` and is part of
that exercised disconnect path, not a failed case.

The earlier probe's companion `/tmp/plex-candidate-maintenance-probe.log` ends
with `TimeoutError: timed out` while waiting for an HTTP response, corroborating
the prior API timeout. The later companion
`/tmp/plex-candidate-maintenance-long-probe.log` reports
`first-start: actual Plex HTTP readiness failed`. These are failed native gates;
the result JSON alone does not identify a crash cause.

## Additional gate checks

The container probe now rejects native `.dmp` reports during readiness and after
recovery, so a later successful restart cannot conceal a crash. A separate
network-isolated disposable container accepted an empty report directory and
rejected an injected empty `fixture.dmp`; this validates report detection, not
a native crash fix. Only fixture-labelled containers/volumes were removed.

The musl E2E executable originally failed before exercising SQLite with
`Dynamic loading not supported`. Its binary build now disables static CRT
linkage, without changing production shim flags. The driver loads the shim in
global scope, matching preload, rather than musl's restricted local scope.
The test shim retains system SQLite for real `RTLD_NEXT` resolution; this is
not the published Plex candidate artifact.
The local runner image manifest is
`sha256:d9d7da23a42c9d975da8f194600493b43af7540d185a296180b99a6fa38eec92`;
PostgreSQL 15 was resolved before execution to
`postgres@sha256:c961aa287d8698297cb26cdfadfbe9fd2cbaf77e53cfffe9636e8d8a1e4d842c`.

After changing driver lookup scope, the macOS shared-artifact run again passed
all 12 cases (`/tmp/plex-runtime-global-macos.log`), and
`cargo clippy --manifest-path rust/plex-pg-core/Cargo.toml --bin runtime_e2e -- -D warnings`
passed (`/tmp/plex-runtime-global-clippy.log`). Shell syntax checks passed for
both runner scripts. No runtime-suite failure was reclassified as a skip.

The additional Linux ARM64/musl PostgreSQL 15.19 run loaded the shim and resolved
real SQLite functions, but stalled before the first case completed. The shim
log shows two redirected opens and the PostgreSQL observer sees the first
`SELECT COUNT(*) FROM runtime_items` connection idle; no case PASS was emitted.
The fixture was stopped rather than left running. Evidence:
`/tmp/plex-runtime-linux-pg15.log` and
`/tmp/plex-runtime-linux-pg15-shim-hang.log`.
This is an unresolved runtime/driver integration failure, not certification.
PostgreSQL 18.6 showed the same stall before any case PASS. Its runner was
forcibly stopped after a minute; the timeout's SIGTERM alone did not stop the
PID-1 test process. Logs are `/tmp/plex-runtime-linux-pg18.log` and
`/tmp/plex-runtime-linux-pg18-shim-hang.log`; the service image was pinned to
`postgres@sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336`.
Neither Linux PostgreSQL lane passed, and this musl experiment does not replace
the pending Linux/glibc CI lanes.

### Post-change schema-delegation regression

Debug logging captured worker delegation completing for the first PostgreSQL
query, then stalling on nested SQLite schema preparation:
`SELECT*FROM"main".sqlite_master ORDER BY rowid`.
The working hypothesis is a cross-thread SQLite mutex dependency; stripped GDB
stacks could not establish the precise mutex. The prepare delegation policy now
keeps `sqlite_master` and `sqlite_schema` queries on their calling thread, without
changing their SQLite routing or the FTS-tokenizer worker policy.

After this two-line runtime fix, the rebuilt Linux ARM64/musl test image
`sha256:ae837fa562e3ed116d0285c6e7349fb369b5c12b2602010ea9a8ab739d821f1a`
passed all **12 cases on PostgreSQL 15.19 and all 12 on PostgreSQL 18.6**.
`/tmp/plex-runtime-linux-schema-matrix.log` contains both successful runs.
The earlier failed attempts remain failures; the current `pg15.log`/`pg18.log`
paths were reused for the successful reruns. Pre-change evidence remains in
`/tmp/plex-runtime-linux-schema-before.log` and the `*-shim-hang.log` files.
This does not execute the pending Linux/glibc CI lanes.

The rebuilt macOS shim again passed all 12 cases
(`/tmp/plex-schema-macos-e2e.log`); the three existing prepare-delegation unit
tests passed (`/tmp/plex-schema-existing-tests.log`), and interpose-library
Clippy with `-D warnings` passed (`/tmp/plex-schema-clippy.log`). No unit tests
were added or changed.

The updated real LinuxServer ARM64/Plex candidate
`sha256:8778456b654cebb6d0abe787ef472a3ae31fb853e0823da96ab02c1452b4a463`
completed the empty-library API, restart and PostgreSQL outage/recovery smoke
(`/tmp/plex-candidate-schema-evidence/result.json`). The intentional
`workload-smoke-complete-certification-incomplete` exit remains `1`.
This is not evidence that the separate intermittent native SIGSEGV is fixed.

### Native media workload extensions

The new scan driver generates a two-second 320x240 uncompressed AVI using the
candidate's bundled Transcoder and Python, with no added dependencies. The run
in `/tmp/plex-candidate-scan-evidence` completed actual scanner analysis, API
readback, exactly one joined PG metadata/media/part row, zero matching shadow
parts, and the same assertions after restart, followed by outage/recovery smoke.

The next run in `/tmp/plex-candidate-art-evidence` additionally verified that
Plex's media-file route returns byte-identical fixture content and that item
thumbnail/art routes return bounded image responses with image signatures.
Both pre- and post-restart checks passed. This verifies delivery routes, not
decoded playback or transcoding. Both runs deliberately exited at incomplete
certification with promotion forbidden.

The driver then gained watched/unwatched API and PG assertions, with persistence
across restart and shadow-storage checks. **These checks have not passed:**
`/tmp/plex-candidate-watch-evidence/result.json` and
`/tmp/plex-candidate-watch-repeat-evidence/result.json` both failed at
`first-start` when the crash-report gate detected actual native PMS crash dumps.
All four runs used candidate
`sha256:8778456b654cebb6d0abe787ef472a3ae31fb853e0823da96ab02c1452b4a463`.
The two failures preserve evidence that successful scans do not resolve or
excuse the intermittent startup SIGSEGV. Owned fixtures were cleaned up;
unrelated services were left untouched.

The complete known-report acceptance ledger and release gates are in
[release-readiness.md](release-readiness.md), including closed issues and all
seven subreports in issue #10.

Follow-up review tightened scan assertions to bind exact fixture dimensions and
duration to its own Media/Part, and compare PG metadata/media/part identities
across restart so automatic rescanning cannot hide recreated rows. These newer
assertions pass syntax checks but have not completed a fresh native run; previous
smoke evidence must not be presented as execution of the tightened assertions.

### Scalar ownership and subsequent native checks

`LiveScalarState` previously stored a pointer into its inline buffer before the
struct was returned and moved. Live int/int64/double readers now borrow the
owned buffer at the point of parsing instead of retaining a self-pointer.
The existing numeric E2E case now exercises all three SQLite scalar ABI exports
repeatedly; no unit tests or dependencies were added.

The rebuilt macOS shared shim passed all 12 cases
(`/tmp/plex-scalar-macos.log`), and interpose-library/driver Clippy with
`-D warnings` passed (`/tmp/plex-scalar-clippy.log`). The Linux/musl rerun passed
all 12 on each of PostgreSQL 15 and 18 (`/tmp/plex-scalar-linux-matrix.log`).

Native candidate
`sha256:331f7fb35b99cf8b347db7de47feb7cae9753790919c33ecd091b50c39f5e460`
completed the tightened scan identity assertions, watched-state persistence
through restart, unwatched reset, file/artwork routes and outage/recovery smoke
on PostgreSQL 15 (`/tmp/plex-candidate-scalar-evidence/result.json`). It still
exits intentionally with incomplete certification; a single successful run is
not proof that all native startup crashes are fixed.

The native driver now accepts `PLEX_E2E_RESTART_CYCLES` (default 1), checks all
media/watch-state assertions each cycle, and records requested/completed counts.
CI requests 100 cycles in each of its eight native PostgreSQL 15/18 cells.
Missing native matrix execution and the 72-hour mixed-workload soak still block
promotion. Interrupt/termination traps preserve evidence and clean owned fixtures.

### Standalone RTree boundary

The PlexInc ARM64/PostgreSQL 18 candidate
`sha256:08a518eff1ec5192aeb1985d72303406d45aee9929df9875b1695dc708fb235d`
failed native startup. PostgreSQL logs retained attempts to execute SQLite's
internal `SELECT length(data) FROM "main"."locations_node" WHERE nodeno = 1`;
the nonexistent PG `main` relation exposed incorrect engine-internal routing.
The three RTree backing tables now stay on SQLite and the caller's thread.
A new real-shim RTree E2E case checks creation, a valid root node and absence of
PG backing-table leakage; macOS passed all 13 requested cases including reconnect
(`/tmp/plex-rtree-macos.log`).

The following candidate with only the routing correction still produced a native
crash (`/tmp/plex-plexinc-rtree-evidence/result.json`). Shadow construction also
precreated empty RTree backing tables, then accepted a failed virtual-table
constructor as harmless. The staged shadow builder now lets the RTree module
create its own backing tables and verifies their engine-produced schema, instead
of publishing that partially initialized virtual table. A real temporary SQLite
run of the production builder block passed root-node, insert/query and quick-check
validation. This repair affects only newly staged shadow files, not preserved
source libraries; it is not yet proof that the standalone native crash is fixed.

### Completed restart sample and further initialization findings

The rebuilt LinuxServer ARM64/PostgreSQL 15 candidate
`sha256:4c28e58b9d95867b3158b96a0a5f18eb6dd91f2bb6a445bf0b73aa3138aabee7`
completed **100/100 restart cycles**, each checking media identities, bytes,
artwork and watched-state persistence, then unwatched reset and outage/recovery.
Evidence: `/tmp/plex-native-100-current-evidence/result.json` and
`/tmp/plex-native-100-current-probe.log`. Promotion remains false; this sample
predates the subsequent RTree/operator fixes and cannot certify their artifacts.

The Linux RTree E2E exposed a second engine boundary: `PRAGMA 'main'.page_size`
was treated as a no-op, so RTree tried to initialize a negative-sized node.
Page-size reads now use real SQLite. Initial failure evidence is retained in
`/tmp/plex-rtree-pg15-failed-shim.log`; missing completion remains an error, not
silently translated to DONE.

Standalone PG18 logs also showed compatibility initialization failing before
default account seeding: the reverse equality operator was dropped after its
commutator shell had been created. Plex nevertheless started after the failed
s6 init step. Both operators are now dropped before either is created, with
schema-qualified commutators, and a fresh boot readiness marker prevents the
standalone service from starting unless all PostgreSQL initialization succeeded.
Repeated application of the actual compatibility SQL and symmetric bool/int
comparisons passed on isolated PostgreSQL 15 and 18. A failed intermediate SQL
candidate verified the service refuses to start without its readiness marker;
that failed probe is not counted as successful native startup.

The corrected standalone candidate
`sha256:0b8a805a0936ecdf1a19c03c5ae411f24f7e593001b5e0abc6edf6daa4902945`
then completed native PlexInc ARM64/PostgreSQL 18 startup, scan, file/art routes,
media identity and watch-state persistence across restart, unwatched reset and
fresh-start outage/recovery (`/tmp/plex-plexinc-operatorfix-evidence/result.json`).
Its deliberate incomplete-certification exit is not a failed exercised smoke,
but still does not authorize promotion.

The Linux RTree case also exposed incorrect `pzTail` behavior: a rewrite adding
`IF NOT EXISTS` changed the prepared statement length, so SQLite's internal
multi-statement executor resumed at the wrong position in the caller's SQL.
Engine-local passthrough prepares now use the original SQL and byte count,
without rewrite buffers. The RTree E2E exercises that real internal execution
path; failed intermediate runs are retained rather than skipped or masked.

The corrected RTree/SQL-tail driver passed all **13 cases** on macOS
(`/tmp/plex-rtree-final-macos.log`) and on each of Linux/musl PostgreSQL 15 and 18
(`/tmp/plex-rtree-tail-linux-matrix.log`). The Linux artifact run uses the tested
engine-local fast path; a subsequent equivalent missing-original-symbol guard
was also checked by the final macOS build and Clippy. No full eight-cell native
or 72-hour soak certification is inferred from these runtime results.

The user subsequently changed the planned soak window from 72 hours to **5h50
(21000 seconds)** to fit GitHub-hosted execution. Build and 100-cycle probes
remain separate from the 360-minute soak jobs; each soak requires the matching
prior candidate digest and completed smoke evidence. Historical references to
72 hours above describe the earlier plan, not the current acceptance duration.
No 5h50 observation has been completed merely by changing this configuration.

A short native PlexInc ARM64/PG18 run exercised the new soak loop for 10 measured
seconds with four concurrent media/art readers, scan and watch-state mutations,
identity/process checks and resource sampling. Its subsequent outage/recovery
smoke completed (`/tmp/plex-soak-short-evidence/result.json`); cleanup removed
owned fixtures. This validates short-run wiring, not the 21000-second soak gate.

### Running-server recovery and native fixture decoding

The native driver now stops/restarts only the fixture PostgreSQL container while
PMS remains running. It requires successful library/media reads and new watched/
unwatched writes independently visible in PostgreSQL after recovery, with the
exact same PMS PID set. `/tmp/plex-live-recovery-evidence/result.json` records
`live_postgres_recovery_verified: true` on PlexInc ARM64/PG18. A timed-out read
while PostgreSQL was down is retained as outage evidence, not counted as recovery.

The subsequent `/tmp/plex-decoded-recovery-evidence/result.json` also records
`decoded_fixture_verified: true`: the actual Plex media download is decoded by
that candidate's bundled Transcoder, with all 20 320x240 RGB frames checked against
the synthetic source. These checks passed before/after restart, with four concurrent
readers in the 10-second soak and after live PostgreSQL recovery. This is limited
fixture decoding, not universal client playback/transcoding certification.
Both runs used `sha256:0b8a805a0936ecdf1a19c03c5ae411f24f7e593001b5e0abc6edf6daa4902945`;
promotion remains forbidden. The soak prerequisite validator now rejects artifacts
without explicit live-recovery and decoded-fixture evidence.

The existing LF attributes were independently checked via a real disposable Git
checkout with `core.autocrlf=true`; both entrypoint scripts retained LF and passed
Bash syntax checks. No attributes or unit tests were added. This does not execute
Windows Docker Desktop itself. CLI checks of the soak prerequisite program accept
complete fixture evidence and reject insufficient cycle counts, missing live
recovery and architecture mismatch; those contract fixtures are not native runs.

### First GitHub matrix and corrections

The first remote run on `5573fc1`,
`https://github.com/cgnl/plex-postgresql/actions/runs/37928322012`, failed its
preconditions and did not start any 5h50 soak. Both GNU/Linux runtime jobs tried
to link the bundled-SQLite compatibility harness with interpose exports because
the Makefile static-library recipe built all binaries. The recipe now explicitly
builds only `--lib --features interpose`.

All eight native artifacts failed at fresh startup before completing any restart
cycle. Their logs identify internal SQLite FTS `_content` DDL incorrectly routed
to PostgreSQL. Backing tables with FTS3/FTS4 prefixes and engine suffixes now stay
on SQLite/the invoking thread; logical FTS base views still use PostgreSQL.
A new real-shim FTS E2E case verifies both sides of that boundary. The corrected
macOS run passed all 14 requested cases (`/tmp/plex-ci-fts-macos.log`), and Clippy
passed (`/tmp/plex-ci-fts-clippy.log`). Native remote results require a rerun;
these local checks do not convert the failed first matrix into passing evidence.

Repository-wide `cargo fmt --check` reports existing formatting differences
outside this continuation's driver edit. The edited driver passes its direct
`rustfmt --check`; unrelated formatting was not changed.

## Release decision and limits

**Release/promotion remains blocked.** Both native results explicitly require
the full native matrix, scan/playback/watch-state/artwork and sustained outage
workload. Passing unit tests and macOS shared-shim cases do not replace those
gates. Validate each supported pinned image/architecture with import/source
preservation, startup, scans, playback, watch state, playlists, artwork, restarts
and PostgreSQL interruption, including sustained mixed workloads.

This evidence supports no general Rust-versus-C stability conclusion: there is
no controlled comparison with the same Plex build, architecture, PostgreSQL,
configuration and disposable library snapshot. It also provides no guarantee
of compatibility with future Plex releases. Keep the last validated release
until a candidate passes the required native matrix and workloads; preserve
failure evidence without attributing unproven causes.

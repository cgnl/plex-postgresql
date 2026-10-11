# Stability assessment — 9 October 2026

Baseline: `main` at `c004b81`, version 1.3.20. `develop` at `e1e1b65`
is fully contained in main (85 commits behind, zero unique commits). It is
not a source of additional fixes and should not be the stabilization base.

## Pull requests

| PR | Recommendation |
| --- | --- |
| #27 | Keep removal of the Rust `vfork` wrapper and safe shell counters. Do not import the unverified charset assembly wholesale. |
| #28 | Make Linux exception interposition a compile-time opt-in, not an environment-only pass-through wrapper. |
| #29 | Synchronize all owned sequences, including CLI/blob paths; fail on errors. |
| #30 | Synchronize exact migration-marker sets with SQLite-safe encoding and every startup path. |
| #25 | Reuse the bootstrap-only destination detection idea; do not merge the entire closed patch. |
| #12 | Reject blanket INSERT-to-UPSERT rewriting: it can silently overwrite data. |
| #13 | Reuse only handle-local rowid/error accounting after checking isolation; it contains changes from #12. |
| #21 | Do not ship binary UUID-validator patching as a stability fix. |

## What the reports establish

Issues #17 and #26 show similar plugin-prefix context near native crashes,
but the logs do not contain a usable native backtrace proving a faulting
instruction. An empty-library report also rules out explaining every failure
as a large import. UUID validation, readonly errors, C++ exceptions and native
crashes must be investigated separately.

Reports that older C releases were more stable are valid regression leads,
not proof that Rust itself causes the crashes. The current implementation is
Rust with unsafe FFI, assembly and C++ ABI boundaries. A fair comparison pins
Plex build, architecture, PostgreSQL version, configuration and the same
disposable library snapshot for both artifacts. Never compare different Plex
versions and call the result a language comparison.

## Release evidence and limits

Existing unit tests and the SQL translation harness passed on the baseline;
they did not exercise real handle transactions or the published Plex image.
New coverage uses the real shared shim through a Rust E2E driver and isolated
PostgreSQL. No new unit tests are required for this stabilization work.

See `runtime-e2e.md` for the runtime gate and `plex-compatibility.md` for the
candidate-image gates. Only an executed passing matrix establishes support;
adding a workflow is not evidence that its Linux artifacts passed.
See `stability-validation-2026-10-09.md` for the concrete local results, including
failed native runs and the later completed-but-uncertified container smoke.

Before declaring a production release stable, verify import/source preservation,
native startup, scan, playback, watch state, playlists, artwork, restarts and
PostgreSQL interruption against each supported pinned image/architecture.
Use a sustained mixed-workload canary to investigate the reported native
crashes. Retain crash reports and record versions; do not erase diagnostics.
Future Plex releases cannot be guaranteed compatible in advance. Discovery
must create candidates, and failure must leave the last validated release alone.

## Findings from actual native startup

The isolated Linux ARM64 canary exposed SQLite FTS virtual-table/trigger DDL and
optimizer statistics incorrectly routed to PostgreSQL. These engine-local
operations now run against the real shadow SQLite instead of being ignored or
translated into invalid PostgreSQL statements. PostgreSQL search views/triggers
remain authoritative for library data; shadow FTS maintenance is not a library
data write and does not imply cross-engine transactional atomicity.

Repeated startup also exposed a forked plugin child waiting in a musl futex.
GDB identified the saved caller as `pms_child_env::adjusted_env_for_process` at a
Rust allocator call before `exec`. The exec wrappers now filter environment
entries with bounded stack buffers, without heap allocation, formatting or
logging. Scanner preload reinjection is prepared in the parent's constructor.
Process-hook symbol resolution is also performed before threads/forks, and the
fast child path no longer takes fake-value or exception-tracker mutexes.
The buffers fail with E2BIG rather than bypassing sanitization on overflow.

Follow-up review removed child-side ownership/deallocation of the clone context,
suppressed process-hook stdio logging in forked children, and kept only the first
`LD_PRELOAD` environment entry so duplicate entries cannot alias a rewritten
buffer. These changes simplify the pre-exec path; they are not a guarantee that
arbitrary Plex code executed after fork is async-signal-safe.

The Linux/musl shared-shim E2E later stalled after its first PostgreSQL read
had completed. Debug logging showed an internal SQLite schema prepare
(`SELECT*FROM"main".sqlite_master ORDER BY rowid`) delegated to the worker
while the caller waited. This is consistent with SQLite's caller-held mutex
being needed by the worker; stripped backtraces did not identify the exact
mutex. Schema queries (`sqlite_master`/`sqlite_schema`) now stay on their
calling thread, preserving real SQLite passthrough and leaving ordinary
PostgreSQL/FTS worker delegation unchanged. This targets the observed deadlock,
not the separate native SIGSEGV. Post-change macOS and Linux/musl PostgreSQL
15/18 runs each passed all 12 runtime E2E cases.

A later ARM64 canary still crashed. A diagnostic-only GDB image captured SIGSEGV
in the actual PMS `PMS GTP` thread at a native null-pointer dereference. Separately,
source inspection found the existing `create_simple_converter` pass-through
declared a plain-pointer return where Boost declares a nontrivial C++
`std::unique_ptr`. This redundant wrapper is removed rather than adding more
charset assembly. Docker build/probe export checks reject its reintroduction.
The faulting native instruction does not by itself prove that this wrapper was
the cause: the candidate must still pass actual startup/workload tests.

A later concurrent-workload core now identifies a concrete pool fault on
LinuxServer amd64/PostgreSQL 15. The matched candidate's musl `pthread_kill`
called its lock routine on an unmapped retained thread descriptor, reached
from `reclaim_zombies_and_reap` during column metadata lookup. The actual PMS
exited with signal 11 and zero observed OOM counters. The
[retained evidence](evidence/native-pool-owner-crash-20261010.json) records the
candidate digest, core hash and independently recovered call sites.

Pool ownership now uses unique logical TLS tokens, atomic exit retirement and
exact-owner reclamation instead of calling pthread functions on retained
handles. Live owners and active streams remain protected; abandoned transaction
cleanup stays in the existing connection reuse path. Calling `pthread_kill`
after a thread ID's lifetime ends is undefined and can itself fault, as the
[Linux pthread documentation](https://man7.org/linux/man-pages/man3/pthread_kill.3.html)
explains. This fixes the identified ownership mechanism; the new native matrix
and full 5h50 workload still determine runtime certification.

Native startup also reported a VACUUM step failure on the shadow SQLite path.
The cached/unregistered-statement route did not honor the existing maintenance
no-op policy; it now returns DONE for those statements after checking SQLite
passthrough exceptions. A real shared-shim E2E covers prepared VACUUM/REINDEX/
PRAGMA optimize and the `sqlite3_exec` VACUUM entry point, with unchanged PG data.

This is a concrete independently observed startup defect, not proof that every
native crash in issues #17/#26 shares the same cause. Earlier local probes
failed, and one passed initial startup/restart before another run exposed the
allocation deadlock. One successful startup is insufficient stability evidence.

The PostgreSQL-to-SQLite data export is not a certified native Plex rollback:
native extensions, schema/build matching and search/watch-state/artwork checks
remain separate acceptance requirements. Never substitute an export for the
preserved original library backup.

# Evidence-based stable release

## Scope and decision

The target is a release that resolves or explicitly dispositions every known
report for a declared support matrix. It is not proof that no bugs exist, that
every historical Plex binary is supported, or that future Plex binaries will
remain compatible. Closed GitHub issues are not automatically passing evidence.

The 9 October 2026 GitHub snapshot contains 14 issues excluding pull requests:
four open and ten closed after reopening #24. Bodies and all available comments were reviewed.
Issue #10 contains seven independently verifiable subreports. Issue #24 was
previously closed for perceived inactivity and has now been reopened, #6 was closed for missing feedback, and #8's closing
discussion does not establish a tested recovery fix.

**Publication decision (9 October 2026): v1.3.21 is published as a regular release at the maintainer's explicit request, before completion of the native matrix/soak. Binary publication does not certify those unresolved runtime gates; Docker production promotion remains gated.**

**Native certification status: incomplete.** The schema-delegation deadlock is fixed
and macOS plus Linux/musl PostgreSQL 15/18 runtime E2E pass. Native ARM64 scan,
file-byte delivery, thumbnail/art routes, restart and outage/recovery smoke have
passed in isolated runs. However, the same updated candidate subsequently
crashed during native startup. Subsequent fixes completed one LinuxServer ARM64
PG15 100-restart sample and one PlexInc ARM64 PG18 workload smoke, including
watch state. These are different intermediate artifacts; the final frozen
candidate's complete native matrix and soak remain pending. See
[validation evidence](stability-validation-2026-10-09.md).

## Issue ledger

Each row must acquire a sanitized reproducer, its observed failure signature,
a causal fix or explicit disposition, and post-fix artifact/version evidence.
One issue can require multiple scenarios; similar last-query logs alone do not
prove identical causes.

| Report | Required acceptance evidence | Current remaining gap |
| --- | --- | --- |
| [#1](https://github.com/cgnl/plex-postgresql/issues/1): Docker build | Both native architectures build cleanly from the pinned inputs and boot the resulting artifacts. | ARM64 build evidence exists; native amd64 acceptance remains. |
| [#2](https://github.com/cgnl/plex-postgresql/issues/2): shim/type exceptions | Real SQLite ABI storage classes, integer/datetime decltypes and native device queries agree; no swallowed native exception. | Runtime type E2E passes; exact historical native failure signature needs regression coverage. |
| [#4](https://github.com/cgnl/plex-postgresql/issues/4): export/rollback | Consistent library/blob snapshot, failure-safe export, then native Plex opens exported databases and preserves media/watch state/search/artwork. | Export E2E exists; exported data is not yet a certified native rollback. |
| [#5](https://github.com/cgnl/plex-postgresql/issues/5): inline/ABI loading | Both variants/architectures resolve their actual runtime dependencies; preload is limited to intended Plex processes; no OpenSSL/glibc/musl symbol mismatch. | Bundled libpq and child filtering reduce exposure; full native ABI matrix is required. |
| [#6](https://github.com/cgnl/plex-postgresql/issues/6): Windows quickstart | CRLF checkout/build and Docker Desktop startup follow the documented supported installation path. | Actual Git checkout with core.autocrlf=true preserves LF shell syntax using existing attributes; Windows Docker Desktop acceptance remains unvalidated. |
| [#8](https://github.com/cgnl/plex-postgresql/issues/8): PostgreSQL restart | An already running Plex serves its library again without restarting Plex; interrupted writes/transactions do not partially commit or replay. | Shim reconnect E2E and native PlexInc ARM64 PG18 read/write recovery with unchanged PMS PID pass; full native matrix still pending. |
| [#9](https://github.com/cgnl/plex-postgresql/issues/9): load/pool starvation | Concurrent scanning and 3–4 client streams with bounded pool pressure, no unexpected metadata failures, leaked sessions or transaction contamination. | Handle/transaction E2E passes; real sustained mixed load is unvalidated. |
| [#10](https://github.com/cgnl/plex-postgresql/issues/10): migration/startup | All seven subreports below independently pass. | Native startup remains a blocker; generic runtime passes are insufficient. |
| [#15](https://github.com/cgnl/plex-postgresql/issues/15): maintenance | Documented maintainer responsibility for failed canaries, triage and release approval. | Governance report, not a runtime bug; assigning responsibility is not a code fix. |
| [#17](https://github.com/cgnl/plex-postgresql/issues/17): native shim crash | Fresh and migrated startup, plugin initialization and preferences reads remain crash-free through repeated cycles on each native lane. | Real candidate still produces a startup crash report. |
| [#22](https://github.com/cgnl/plex-postgresql/issues/22): STRM/zurg question | Controlled local HTTP/STRM fixture proves the supported scan/stream behavior, or documentation explicitly declares it unsupported. | Synthetic AVI evidence does not establish STRM or remote VPS support. |
| [#23](https://github.com/cgnl/plex-postgresql/issues/23): PostgreSQL 18 question | PostgreSQL 18 migration plus the actual native workload matrix, not just translation tests. | Linux/musl PG 18 runtime E2E passes; native PG 18 matrix remains pending. |
| [#24](https://github.com/cgnl/plex-postgresql/issues/24): bootstrap migration | Bootstrap Administrator/preferences rows do not suppress real import; genuine populated destinations are preserved; required shadow extensions are checked. | The complete Administrator plus preferences seed-state regression and full imported-library native acceptance remain. #24 is reopened until all three reported failures have evidence. |
| [#26](https://github.com/cgnl/plex-postgresql/issues/26): plugin preferences crash | Native reproduction and causal fix for its signature, with repeated standalone startup and valid NULL/BLOB/preferences behavior. | Do not declare this solved by the unrelated schema-delegation fix. |

### Issue #10 subreports

| Subreport | Required proof |
| --- | --- |
| 1: post-import UUID/segfault startup | Native boot on a representative imported library; SQLite ABI/UUID semantics agree with the original. No binary hotpatch or blanket error suppression. |
| 2: values longer than 255 characters | Exact long-title/GUID/text roundtrip, row counts and FTS/view integrity; failed rows cannot be silently discarded. |
| 3: hidden migration errors | Injected import failure returns nonzero, preserves source/destination and emits actionable retained diagnostics. |
| 4: missing pg_trgm | Required extension is established or startup fails explicitly before schema import. |
| 5: schema destruction on restart | Populated destination survives repeated startup; bootstrap-only destination can import transactionally. |
| 6: unexpected Plex update | Actual running version equals the pinned official candidate; no in-container upgrade changes the tested binary. |
| 7: missing doctor tool | Each final image contains and successfully runs its read-only diagnostic entry point. |

## Ordered release gates

1. **Close the crash blocker first.** Retain every failed run and crash report.
   Compare the candidate with vanilla Plex and, when obtainable, the last known
   stable C artifact using the same Plex build, library, PostgreSQL and platform.
   An old C release on different inputs is not a controlled language comparison.
2. **Finish family-level Rust/native E2E coverage.** Cover migrations, ABI/type
   ownership, plugin startup, scanner subprocesses, watch-state changes,
   playlists, file routes, decoded playback/transcoding, artwork, interruption
   and mixed load. Reuse fixtures and existing helpers; do not add more unit
   tests or dependencies for this work. A byte-identical file download is not
   decoded playback certification. The driver now additionally decodes the
   retrieved synthetic AVI and checks its exact frames; client playback,
   seeking, audio and transcoding-session coverage remain separate requirements.
3. **Execute the declared native matrix.** Initially LinuxServer and PlexInc,
   arm64 and amd64, PostgreSQL 15 and 18: eight combinations. Test genuine native
   hardware, fresh configs and representative migrated libraries. The separate
   macOS/shared-shim and Linux/glibc checks cannot substitute for those lanes.
   Supporting other PostgreSQL majors requires adding their acceptance cells.
   GitHub-hosted `ubuntu-24.04` supplies native x64 and `ubuntu-24.04-arm`
   supplies native arm64. The candidate workflow pins these labels; its runtime
   architecture check still rejects emulation. GitHub-hosted jobs are limited
   to six hours. Build/100-cycle acceptance and the continuous 5h50 soak run in
   separate jobs, reusing the exact candidate digest. The soak job has a
   360-minute timeout, leaving ten minutes for setup, teardown and evidence.
4. **Exercise failure and restoration.** PostgreSQL restart, dropped sessions,
   full pool, invalid credentials and interrupted migration must fail safely.
   Running-server recovery must not require a PMS restart. Verify native restore
   from the preserved original database; certify export-based rollback separately.
5. **Run a release-candidate soak.** Minimum 100 clean startup/restart cycles per
   native lane and 5 hours 50 minutes of concurrent scan/playback/metadata updates on a
   representative canary. Require zero unexplained crashes, failed invariants,
   lost/duplicated writes or unbounded pool/memory growth. These sample sizes are
   operational gates, not a mathematical guarantee of zero future failures.
   This replaces the earlier 72-hour requirement at the user's request; its
   stability claim is limited to the shorter measured observation window.
6. **Promote exactly what passed.** Freeze source and Plex versions, retain all
   test reports, and promote the tested immutable image digest without rebuilding.
   A missing/skipped lane, unresolved blocking ledger row or failed canary forbids
   certified Docker promotion. Keep the last validated release and tested restoration path.

For every future public Plex version, discovery creates a pinned candidate;
the same gates establish support before promotion. Failure leaves the last
validated release unchanged. New issues extend the ledger and its regression
scenarios rather than relying on one successful startup or issue closure.

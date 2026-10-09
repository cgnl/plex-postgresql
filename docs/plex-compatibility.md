# Plex compatibility and fail-closed releases

## Current policy

The goal is compatibility with the newest **public Linux Plex release**, not a
promise of compatibility with unknown future binaries or Plex Pass betas. The
official `https://plex.tv/api/downloads/5.json` Linux version observed on
2026-10-09 is `1.43.4.10903-e5521bd8c`. Every candidate run fetches this API again;
that observed version is not hardcoded as a permanent supported version.

**Promotion is disabled.** Scheduled monitoring and the Docker publish workflow
build candidates only. They do not bump `VERSION`, rewrite `CHANGELOG.md`, update
the approved digest baseline, create Git tags, create GitHub releases, or update
production version/architecture/`latest` image tags. Digest changes indicate work
to investigate, never compatibility. Missing inputs, missing drivers, skipped
jobs, failed builds, unsupported architectures, and incomplete gates cannot
produce a successful release workflow. The terminal promotion-blocked job fails
even when every implemented probe passes. There is no environment-variable bypass.

## Candidate inputs and evidence

`scripts/check-upstream-updates.py` resolves the official Linux version and both
official Debian architecture records, then resolves Docker Hub manifest indexes
for LinuxServer Plex, Plex Inc Plex, Alpine 3.15, and PostgreSQL 15/16/18 Bookworm.
Registry response digests are checked against the returned manifest bytes; both
Linux amd64 and arm64 platform digests must exist. Any resolution failure fails
the run rather than retaining an old digest or skipping an architecture.

The workflow resolves these inputs once, records the exact source commit, and
builds each of the eight variant/architecture/PostgreSQL 15-or-18 candidates once using pinned
runtime and builder image refs. Candidate tags include the run ID, attempt, and
architecture. Tests pull the build action's immutable output digest, never a
mutable candidate tag; subsequent starts use the resulting immutable image ID.
Artifacts record the source SHA, upstream snapshot, candidate digest, build
metadata, installed/runtime Plex version, process/shim evidence, fixture logs,
and phase/result. Candidate artifacts and images are **not approved releases**.

Image pinning does not pin apt/apk repositories, Rust's stable channel, or the
Dockerfile syntax frontend. Exact rebuild reproducibility is not claimed. A
future promoter must reuse the tested output digests, not rebuild those inputs.

## Runtime downloads

Both Dockerfiles default to `VERSION=docker`. LinuxServer must use its bundled
Plex binary, not fetch a new one at startup. Plex Inc's independent
`50-plex-update` init hook is removed at build time because its version mechanism
does not rely on the LinuxServer `VERSION` convention. A missing expected hook
fails the build so upstream layout changes require review. Do not override
`VERSION` or reintroduce a startup updater in deployed candidates.

The container probe uses an internal Docker network with **no external egress**,
no production configuration, no Plex claim/token, unique labelled containers and
volumes, and container-loopback HTTP checks without published host ports. This also prevents
startup from quietly replacing the tested Plex binary. Cleanup targets only its
own labelled resources, including anonymous container volumes, and retains logs.

## Implemented probes

`scripts/plex-container-e2e.sh` requires immutable candidate and PostgreSQL refs
and native execution. It checks image architecture, `VERSION=docker`, and the
installed package against the observed official version. A lagging upstream
image fails; it is not upgraded during the test to disguise that lag.

It starts the actual candidate with isolated PostgreSQL, requires real Plex
`/identity` readiness and matching version, verifies the actual PMS process maps
contain `db_interpose_pg.so`, and observes remote PostgreSQL sessions. It creates
an empty movie library through the real Plex HTTP API, requires exactly one
matching PostgreSQL row and no matching row in read-only shadow SQLite, then
requires the real Plex API to return that library. It repeats the routing and
persistence checks after restarting the same candidate/configuration. It then stops
PostgreSQL, starts a fresh-config candidate, rejects HTTP readiness throughout an
80-second observation window, and requires recovery after PostgreSQL returns and
the candidate restarts. The driver also generates a two-second uncompressed AVI
using Python and the candidate's existing Plex Transcoder, requests an actual
library scan, and requires analyzed media through the Plex API plus exactly one
joined PostgreSQL metadata/media/part row and no matching shadow SQLite part.
It repeats the media assertions after restart, compares the native media-file
route's bytes with the fixture, decodes that download using the bundled Plex
Transcoder and compares all 20 RGB frames with the generated pixels, and checks
thumbnail/art image responses. The
watch-state gate marks the fixture watched, checks API/PG persistence across
restart and absence of watched shadow rows, then marks it unwatched and verifies
the reset. This tests one local personal-media workload, not general scan-format
coverage, client playback/seek/transcoding protocols, audio, multiple users or sustained mixed load.
Adding a gate is not evidence it passed; startup failure prevents later checks.
Even
their successful completion returns nonzero until full certification exists.
Readiness checks also reject native `.dmp` crash reports in the fresh fixture,
including reports left by a process that restarted and later became HTTP-ready.
Reports remain in the evidence directory during cleanup.

The separate eight-cell soak job reuses each digest only after its 100-cycle
native smoke evidence passes source/version/architecture/PostgreSQL matching.
`PLEX_E2E_SOAK_SECONDS=21000` requests 5h50 of monotonic elapsed time. The loop
mixes scan requests, four concurrent media/file/art readers, watched/unwatched
writes and PG/shadow identity checks, records resource samples, and rejects PMS
process replacement. Its 360-minute job limit includes setup and cleanup;
slow setup or interrupted/incomplete observations remain failures. This fixture
workload does not establish all playback protocols, media formats or multi-user
coverage. Promotion remains blocked for those outstanding gates.

The workflow additionally requires
`scripts/run-runtime-e2e.sh --reconnect` against a digest-pinned disposable
PostgreSQL service and the same source SHA. Missing driver or any driver failure
fails this job. Its host-built shared artifact is useful independent runtime
coverage, **not proof for the container-built artifact or actual Plex workload**.

Both Linux Docker builders run `nm -D --defined-only` on the actual final
`/libs/db_interpose_pg.so` and fail if `vfork`, `__cxa_throw` or
`create_simple_converter` is exported,
including versioned symbols. The symbol listing travels with the candidate image
and is retained/rechecked by the container probe. The existing builder invokes
Cargo with `--features interpose` only.

Default Linux artifacts must not export `__cxa_throw`: the Rust exception hook is
an opt-in `exception-hook` Cargo feature, not a default release feature. The
unsafe Rust vfork wrapper has been removed by the runtime owner. This pipeline
does not enable that feature or restore the wrapper. Passing symbol validation
alone is not an inferred compatibility claim.

The unnecessary `create_simple_converter` pass-through wrapper is removed:
Boost returns a nontrivial C++ `std::unique_ptr`, not the plain pointer declared
by that Rust wrapper. The original C++ implementation now receives the call
directly, avoiding a redundant and incorrectly typed ABI boundary.

## Remaining release blockers

- Execute the implemented real API mutation/persistence gate on all eight native
  candidate lanes; initialization and an open session alone are insufficient.
- Exercise scanning, playback, watch state, playlists and artwork in addition to
  the empty-library mutation, with database-side persistence evidence.
- Execute the implemented running-server PostgreSQL restart/read/write recovery
  gate on every native cell, without restarting or replacing the PMS process.
- Exercise the exact candidate shared artifact with runtime routing/reconnect
  coverage on every supported architecture and variant.
- Execute and review all eight native container lanes in external CI. A local
  smoke attempt is not release certification or a guarantee of future support.
- Implement one promotion barrier requiring explicit successful results and
  complete digest-bound evidence from **all** runtime/container gates. Only then
  compose versioned and `latest` manifests from the **same tested digests**.
  No rebuild, new upstream lookup, mutable tag resolution, or digest-only release.

The Linux/macOS artifact workflows still build and retain candidate bundles,
but now have explicit failing certification jobs before GitHub Release uploads.
Manually creating a `v*` tag cannot bypass this block. Replace these blockers only
with passing workload gates tied to the exact packaged artifacts; do not remove
them just because the separate host-built runtime driver passes.

## Local probe

Resolve to a temporary snapshot, build one native candidate with its pinned
`BUILDER_IMAGE` and `PLEX_BASE_IMAGE`, then supply `CANDIDATE_IMAGE` as its local
`sha256:` image ID (or a registry `@sha256:` ref), `POSTGRES_IMAGE` from the snapshot,
`EXPECTED_PLEX_VERSION`, `EXPECTED_ARCH`, `VARIANT`, and a disposable `EVIDENCE_DIR`:

```sh
python3 scripts/check-upstream-updates.py --output /tmp/upstream-candidate.json
bash scripts/plex-container-e2e.sh
```

The probe deliberately fails at `workload-smoke-complete-certification-incomplete` even if all
implemented checks pass. Inspect `result.json` and logs, not merely the exit code.
Never use a production database/configuration or treat HTTP readiness alone as
compatibility approval.

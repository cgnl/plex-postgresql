# Automated Plex compatibility and releases

## Current configuration

The monitor checks the newest **public Linux Plex release** every six hours.
Plex Pass betas are outside this policy. The schedule starts discovery; it does
not promise a completed certification or release every six hours.

Automatic publication is currently disabled because no reviewed
`.github/release-trust.json` has been installed. This trust file is the
authoritative publication gate. The historical profile ledger is empty, so coverage starts
with latest only. There is no configured `OPENAI_API_KEY` repository secret;
automated fix generation reports `fix-generation-unconfigured` until one is
provided. These are configuration gaps, not evidence of compatibility.

## Discovery and historical coverage

`check-updates.yml` resolves the official Plex download API and immutable image
manifests for Plex Inc, LinuxServer, the builder and PostgreSQL. Fingerprints
include the source, policy, fixtures and pinned profile inputs. Unchanged
attempts, including recorded failures, are deduplicated. The ledger keeps one
active attempt and one replaceable pending fingerprint; superseded inputs remain
visible. Infrastructure retries are bounded; assertion failures require
investigation or an explicit retry.

Native tests must measure the actual Plex binary version in both variants on
amd64 and arm64. Images that lag the official version produce
`waiting-for-image`, not a compatibility regression or approval. Digest changes
alone never prove support. Missing historical artifacts are reported rather than
silently replaced with latest.

Maintainers can add up to two distinct older public releases to
`.github/plex-compatibility-profiles.json`. Each profile must pass validation and
contain immutable inputs available for both architectures and variants.
Historical records do not certify new code: every release must test every
required profile against the **same current frozen source**.

## Probe versus full certification

The reusable `docker-publish.yml` accepts a pinned profile and source SHA.
`probe` mode exercises a shorter workload and cannot issue a release
certificate. For each profile, `full` mode requires:

- Eight native container lanes: Plex Inc and LinuxServer, amd64 and arm64,
  PostgreSQL 15 and 18.
- 100 restart cycles per lane and a separate **21,000-second (5h50) soak** using
  the same immutable candidate image.
- Four native ZIP installation lanes covering both variants and architectures,
  including a real SQLite-to-PostgreSQL migration.
- Matching source, actual Plex version, workload and artifact evidence across
  all required lanes.

Profiles run sequentially with bounded matrix concurrency. Setup, builds,
restarts and ZIP checks add time beyond the soak. Missing, failed, cancelled or
skipped required evidence prevents certification.

The fixture libraries contain Big Buck Bunny and two episodes of The Beverly
Hillbillies. Tests exercise scanning, server file delivery, range seeking,
video/audio decoding, watch-state persistence, import/export and database outage
recovery. See [runtime E2E](runtime-e2e.md) for attribution and assertions. This
does not establish browser/client playback, every media format, server-side
transcoding or macOS compatibility.

Candidates use isolated configuration and disposable PostgreSQL, with no
production library or Plex claim token. Runtime updater hooks are disabled;
tests cannot quietly replace the pinned Plex binary. Crash reports remain
evidence even if the process subsequently becomes HTTP-ready.

## Failure reports and separate fix PRs

Reporting distinguishes infrastructure failures, image lag, demonstrated
regressions and insufficient evidence. A failed job alone does not justify
calling an upstream release a breaking change. Reports and changelog entries
link to observed evidence and avoid inventing a root cause.

A demonstrated regression can request a **separate draft code-fix PR** through
`compatibility-fix.yml`. Codex generation uses a pinned action, a trusted prompt,
read-only repository permissions, no persisted checkout credentials and a
restricted sandbox. A separate posting job validates the structured proposal
against its exact base and fingerprint. Only allowed runtime and regression-test
paths can change; workflows, publication helpers, dependencies, symlinks and
binary patches are rejected.

Generated fixes are never automatically merged. They need review and normal
unprivileged CI; a merged fix then requires fresh full certification. Without an
API secret, reporting continues but no generated fix is claimed. Configure
`OPENAI_API_KEY` as a GitHub repository secret using a dedicated API project with
a limited budget and appropriate billing. A sandbox and patch allowlist reduce
risk but do not make untrusted generation risk-free; use a scoped API proxy
where available.

## Exact tested releases

Staging freezes `VERSION` and a factual `CHANGELOG.md` entry **before** full
certification. The certificate binds that source leaf, required profiles,
completed native evidence, final ZIP hashes and candidate image digests. A green
probe or ordinary PR check cannot substitute for it.

`publish-certified-release.yml` independently authenticates the issuing
repository, trusted workflow and revision, run attempt, successful conclusion,
source and artifact ownership through GitHub. It starts automatically when
certification completes successfully, or accepts a manual certificate run ID;
both paths require the same trust validation. It reuses certified bytes and
immutable image digests instead of rebuilding. Existing tags or assets must
match exactly, otherwise publication fails. Concurrent main changes or rewritten
staging commits require fresh staging and certification. Preserve the tested
leaf with a fast-forward or a merge retaining its ancestry and tree. Publication
also rechecks current upstream inputs. A newer upstream release supersedes a
stale candidate and blocks its publication rather than calling it latest.

The publisher creates a **regular release**, not a prerelease. Automated assets
are **Linux-certified only**. No new macOS asset is published until a separate
native macOS gate and exact-byte certificate exist. Previously published macOS
assets remain available. Legacy tag workflows no longer independently upload
release assets, so creating a tag cannot bypass certificate validation.

## Bootstrap and operation

Before enabling automatic publication:

1. Merge and validate the trusted automation workflows and policy.
2. Resolve an immutable latest profile and stage the versioned candidate.
3. Run full `release-certification.yml` on the default branch with the frozen
   `source_sha`, profile JSON and profile ID.
4. Review genuine native evidence and artifact retention. Use the bootstrap
   helper below to prepare trust configuration from the successful run.
5. Review and merge that configuration as `.github/release-trust.json`. Configure
   the `certified-release` environment according to repository governance.

Run the helper from a reviewed, committed default-branch checkout. Its committed
workflow and helper blobs must match the files that executed the issuing run;
uncommitted local edits do not count as reviewed execution bytes. Supply the
actual successful certification run ID and its immutable latest profile:

```sh
export GH_TOKEN="$(gh auth token)"
python3 scripts/bootstrap_release_trust.py \
  --repository cgnl/plex-postgresql \
  --run-id ACTUAL_CERTIFICATION_RUN_ID \
  --latest-profile /tmp/profile-latest.json \
  --output /tmp/release-trust.json
unset GH_TOKEN
```

The helper performs read-only GitHub API checks and writes a local file for a
configuration PR. It authenticates the completed default-branch run, certificate
bytes, reviewed execution surface and required coverage. It refuses to overwrite
an existing output file and never enables publication remotely. Do not replace
the run ID with ordinary PR checks or construct a certificate manually. Adding
or changing trusted execution files requires review and appropriate trust
refresh; missing matching evidence remains a publication blocker.

Certificates and CI artifacts have retention limits. Missing or expired assets
cannot be rebuilt as substitutes; retain certified bytes durably or recertify.
An explicit monitor dispatch with `retry: true` requests a retry of a recorded
fingerprint. Job summaries and the ledger expose waiting, failed, superseded and
unconfigured states. Implemented workflows and unit tests are not a claim that
a live generated fix, complete certificate or automatic release has succeeded.

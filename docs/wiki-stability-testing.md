# Stability Testing

As of 9 October 2026, `test/stability-native-e2e-20261009` is a prerelease test
branch. Production promotion remains blocked. Native testing is in progress;
the full GitHub matrix and continuous 5h50 soak have not completed for the final
candidate. Passing results from intermediate images are not release evidence
for a different image digest.

## Native acceptance matrix

The candidate must run on genuine native hardware for all eight combinations:
LinuxServer and PlexInc, amd64 and arm64, PostgreSQL 15 and 18. Emulation does not
count as native acceptance. Each lane requires 100 startup/restart cycles and a
separate 5-hour-50-minute soak of concurrent library, media and metadata work.
Both jobs reuse the exact candidate digest. Missing or failed lanes prevent
promotion; the workflow currently publishes candidate tags only.

## Real movie and TV libraries

The isolated container runner scans these checksum-pinned fixtures:

- **Big Buck Bunny (2008):** 640x360 H.264 with audio, licensed CC BY 3.0.
  Attribution: (c) copyright 2008, Blender Foundation / www.bigbuckbunny.org.
- **The Beverly Hillbillies:** S01E01, *The Clampetts Strike Oil*, and S01E02,
  *Getting Settled*. The Internet Archive uploaders mark these copies Public
  Domain; this is the source designation, not a rights claim for the entire series.

Candidate smoke checks movie and show/season/episode identities, PostgreSQL
hierarchy, server file delivery, byte-range seeking, delivery hashes, and full
video/audio decoding with Plex's bundled decoder. The soak decodes 20-second
HTTP samples during setup, recovery and each iteration. Full decode and sample
results are recorded separately. Media binaries are cached outside the repository
and are not bundled into evidence artifacts.

These checks cover native file delivery and decoding. Browser player behavior
and server-side transcoding remain unverified.

## Evidence and release decision

Consult the repository documents for current results and the complete gates:

- [Runtime E2E and fixture sources](https://github.com/cgnl/plex-postgresql/blob/test/stability-native-e2e-20261009/docs/runtime-e2e.md)
- [Release readiness and issue ledger](https://github.com/cgnl/plex-postgresql/blob/test/stability-native-e2e-20261009/docs/release-readiness.md)
- [Recorded validation evidence](https://github.com/cgnl/plex-postgresql/blob/test/stability-native-e2e-20261009/docs/stability-validation-2026-10-09.md)

Release approval requires a frozen candidate, passing native evidence, and all
blocking release gates resolved. A successful download or one clean startup is
insufficient to approve a production release.

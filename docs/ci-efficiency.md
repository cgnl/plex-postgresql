# Native CI cost and timing

The native matrix covers LinuxServer and PlexInc, AMD64 and ARM64, and
PostgreSQL 15 and 18. PostgreSQL runs in a separate test container; its version
does not change the Plex image.

Build each Plex image once, then test its immutable digest with both PostgreSQL
versions. This reduces image builds from eight to four while retaining all eight
test combinations. Each test still checks that the installed libraries match the
release ZIP. Tests can read packages; only build jobs can push candidates.

Normal runs use INFO shim logs and PostgreSQL error logging. Docker keeps three
20 MB log files per test container. Recent errors are collected with the test
results; older rotated output is discarded. For a focused diagnosis, set:

```sh
PLEX_E2E_SHIM_LOG_LEVEL=DEBUG
PLEX_E2E_POSTGRES_LOG_STATEMENT=all
```

A short probe checks changed Plex inputs. The full release run requires 100
restarts and 350 minutes of sustained work per combination, plus four native
ZIP installation tests. Reducing duplicate builds saves runner time and download
traffic, but the full load phase still takes at least 5h50.

Retry a lost runner separately after the other jobs finish. Assertion failures
need investigation. A retry must still provide valid source, digest and artifact
provenance before publication; a green badge alone is insufficient.

These changes should be merged after the currently frozen release has finished
testing. They do not alter that running workflow or its evidence.

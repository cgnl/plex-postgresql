#!/bin/bash
export DYLD_INSERT_LIBRARIES=/Users/sander/plex-postgresql/db_interpose_pg.dylib
export PLEX_PG_HOST=localhost
export PLEX_PG_PORT=5432
export PLEX_PG_DATABASE=plex
export PLEX_PG_USER=plex
export PLEX_PG_PASSWORD=plex
export PLEX_PG_SCHEMA=plex

# Run with lldb catching all exceptions
lldb -o "breakpoint set -E C++" -o "run" -o "bt" -o "quit" -- "/Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server" 2>&1

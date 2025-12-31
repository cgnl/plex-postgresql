#!/bin/bash

export DYLD_INSERT_LIBRARIES=/Users/sander/plex-postgresql/db_interpose_pg.dylib
export PLEX_NO_SHADOW_SCAN=1
export PLEX_PG_HOST=localhost
export PLEX_PG_PORT=5432
export PLEX_PG_DATABASE=plex
export PLEX_PG_USER=plex
export PLEX_PG_PASSWORD=plex
export PLEX_PG_SCHEMA=plex
export ENV_PG_LOG_LEVEL=DEBUG

# Run lldb with commands
lldb -o run -o "thread backtrace" -o "register read" -o "frame info" "/Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server"

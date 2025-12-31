#!/bin/bash

# Kill any running Plex
pkill -9 -f "Plex" 2>/dev/null

# Set up environment and start lldb
export DYLD_INSERT_LIBRARIES=/Users/sander/plex-postgresql/db_interpose_pg.dylib
export PLEX_NO_SHADOW_SCAN=1
export PLEX_PG_HOST=localhost
export PLEX_PG_PORT=5432
export PLEX_PG_DATABASE=plex
export PLEX_PG_USER=plex
export PLEX_PG_PASSWORD=plex
export PLEX_PG_SCHEMA=plex
export ENV_PG_LOG_LEVEL=INFO

lldb "/Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server"

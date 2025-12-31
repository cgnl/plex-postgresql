#!/bin/bash
# Plex Media Scanner wrapper for PostgreSQL shim
# Replaces the binary wrapper to ensure ALL database operations go through PostgreSQL

SCRIPT_DIR="$(dirname "$0")"
SCANNER_ORIGINAL="/Applications/Plex Media Server.app/Contents/MacOS/Plex Media Scanner.original"

# Ensure our PostgreSQL shim is loaded (should already be inherited, but be explicit)
export DYLD_INSERT_LIBRARIES="${DYLD_INSERT_LIBRARIES:-/Users/sander/plex-postgresql/db_interpose_pg.dylib}"

# Disable any shadow database logic
export PLEX_NO_SHADOW_SCAN=1

# Log that our wrapper is being used
echo "[pg-wrapper] Running scanner with PostgreSQL shim: $@" >> /tmp/plex_redirect_pg.log

# Execute the original scanner with all arguments passed through
exec "$SCANNER_ORIGINAL" "$@"

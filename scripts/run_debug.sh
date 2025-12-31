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

echo "Starting Plex under lldb..."
echo "Once Plex starts, run this in another terminal:"
echo "  curl 'http://127.0.0.1:32400/playQueues?type=video&uri=server%3A%2F%2F354c2de30499608177061a89e65d298851277bcb%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F50844&X-Plex-Token=REDACTED_TOKEN' -X POST"
echo ""

exec lldb "/Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server"

#!/bin/bash
# Uninstall Plex wrapper scripts - restore original binaries

set -e

PLEX_APP="/Applications/Plex Media Server.app/Contents/MacOS"

echo "=== Plex PostgreSQL Wrapper Uninstaller ==="
echo ""

# Check if Plex is running
if pgrep -x "Plex Media Server" >/dev/null 2>&1 || pgrep -x "Plex Media Server.original" >/dev/null 2>&1; then
    echo "WARNING: Plex is running. Stop it first:"
    echo "  pkill -x 'Plex Media Server' 'Plex Media Server.original'"
    exit 1
fi

# Restore Server
if [[ -f "$PLEX_APP/Plex Media Server.original" ]]; then
    echo "Restoring Plex Media Server..."
    rm -f "$PLEX_APP/Plex Media Server"
    mv "$PLEX_APP/Plex Media Server.original" "$PLEX_APP/Plex Media Server"
    echo "  Done"
else
    echo "Plex Media Server.original not found - nothing to restore"
fi

# Restore Scanner
if [[ -f "$PLEX_APP/Plex Media Scanner.original" ]]; then
    echo "Restoring Plex Media Scanner..."
    rm -f "$PLEX_APP/Plex Media Scanner"
    mv "$PLEX_APP/Plex Media Scanner.original" "$PLEX_APP/Plex Media Scanner"
    echo "  Done"
else
    echo "Plex Media Scanner.original not found - nothing to restore"
fi

echo ""
echo "=== Uninstall complete ==="
echo "Plex will now use SQLite (original behavior)."

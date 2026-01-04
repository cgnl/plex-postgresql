#!/bin/bash
# Uninstall Plex wrapper scripts - restore original binaries (Linux)

set -e

PLEX_DIR="${PLEX_DIR:-/usr/lib/plexmediaserver}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

echo "=== Plex PostgreSQL Wrapper Uninstaller (Linux) ==="
echo ""

# Check if running as root
if [[ $EUID -ne 0 ]]; then
    echo -e "${RED}ERROR: This script must be run as root${NC}"
    echo "  sudo $0"
    exit 1
fi

# Check if Plex is running
if pgrep -f "Plex Media Server" >/dev/null 2>&1; then
    echo -e "${RED}WARNING: Plex is running. Stop it first:${NC}"
    echo "  sudo systemctl stop plexmediaserver"
    exit 1
fi

# Restore Server
if [[ -f "$PLEX_DIR/Plex Media Server.original" ]]; then
    echo "Restoring Plex Media Server..."
    rm -f "$PLEX_DIR/Plex Media Server"
    mv "$PLEX_DIR/Plex Media Server.original" "$PLEX_DIR/Plex Media Server"
    echo -e "${GREEN}  Done${NC}"
else
    echo "Plex Media Server.original not found - nothing to restore"
fi

# Restore Scanner
if [[ -f "$PLEX_DIR/Plex Media Scanner.original" ]]; then
    echo "Restoring Plex Media Scanner..."
    rm -f "$PLEX_DIR/Plex Media Scanner"
    mv "$PLEX_DIR/Plex Media Scanner.original" "$PLEX_DIR/Plex Media Scanner"
    echo -e "${GREEN}  Done${NC}"
else
    echo "Plex Media Scanner.original not found - nothing to restore"
fi

echo ""
echo -e "${GREEN}=== Uninstall complete ===${NC}"
echo "Plex will now use SQLite (original behavior)."
echo ""
echo "Start Plex:"
echo "  sudo systemctl start plexmediaserver"

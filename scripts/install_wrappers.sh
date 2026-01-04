#!/bin/bash
# Install Plex wrapper scripts for PostgreSQL shim
# This replaces the Plex binaries with wrapper scripts that inject the shim

set -e

PLEX_APP="/Applications/Plex Media Server.app/Contents/MacOS"
SHIM_PATH="/Users/sander/plex-postgresql/db_interpose_pg.dylib"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=== Plex PostgreSQL Wrapper Installer ==="
echo ""

# Check if shim exists
if [[ ! -f "$SHIM_PATH" ]]; then
    echo -e "${RED}ERROR: Shim not found at $SHIM_PATH${NC}"
    echo "Run 'make' first to build the shim."
    exit 1
fi

# Check if Plex is running
if pgrep -x "Plex Media Server" >/dev/null 2>&1 || pgrep -x "Plex Media Server.original" >/dev/null 2>&1; then
    echo -e "${YELLOW}WARNING: Plex is running. Stop it first:${NC}"
    echo "  pkill -x 'Plex Media Server' 'Plex Media Server.original'"
    exit 1
fi

# Backup and install Server wrapper
echo "Installing Plex Media Server wrapper..."
if [[ -f "$PLEX_APP/Plex Media Server" && ! -f "$PLEX_APP/Plex Media Server.original" ]]; then
    # First time - backup original binary
    if file "$PLEX_APP/Plex Media Server" | grep -q "Mach-O"; then
        echo "  Backing up original binary..."
        mv "$PLEX_APP/Plex Media Server" "$PLEX_APP/Plex Media Server.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not a Mach-O binary)${NC}"
    fi
fi

if [[ -f "$PLEX_APP/Plex Media Server.original" ]]; then
    cat > "$PLEX_APP/Plex Media Server" << 'WRAPPER'
#!/bin/bash
# Plex Media Server wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SERVER_BINARY="$SCRIPT_DIR/Plex Media Server.original"

# PostgreSQL shim
export DYLD_INSERT_LIBRARIES="/Users/sander/plex-postgresql/db_interpose_pg.dylib"
export PLEX_PG_HOST="${PLEX_PG_HOST:-localhost}"
export PLEX_PG_PORT="${PLEX_PG_PORT:-5432}"
export PLEX_PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
export PLEX_PG_USER="${PLEX_PG_USER:-plex}"
export PLEX_PG_PASSWORD="${PLEX_PG_PASSWORD:-plex}"
export PLEX_PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
export PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR="${PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR:-/Users/sander/Library/Application Support}"

# Execute the original server
exec "$SERVER_BINARY" "$@"
WRAPPER
    chmod +x "$PLEX_APP/Plex Media Server"
    echo -e "${GREEN}  Server wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original binary not found${NC}"
    exit 1
fi

# Backup and install Scanner wrapper
echo "Installing Plex Media Scanner wrapper..."
if [[ -f "$PLEX_APP/Plex Media Scanner" && ! -f "$PLEX_APP/Plex Media Scanner.original" ]]; then
    if file "$PLEX_APP/Plex Media Scanner" | grep -q "Mach-O"; then
        echo "  Backing up original binary..."
        mv "$PLEX_APP/Plex Media Scanner" "$PLEX_APP/Plex Media Scanner.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not a Mach-O binary)${NC}"
    fi
fi

if [[ -f "$PLEX_APP/Plex Media Scanner.original" ]]; then
    cat > "$PLEX_APP/Plex Media Scanner" << 'WRAPPER'
#!/bin/bash
# Plex Media Scanner wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SCANNER_ORIGINAL="$SCRIPT_DIR/Plex Media Scanner.original"

# Ensure PostgreSQL shim is loaded
export DYLD_INSERT_LIBRARIES="${DYLD_INSERT_LIBRARIES:-/Users/sander/plex-postgresql/db_interpose_pg.dylib}"

# Disable shadow database logic
export PLEX_NO_SHADOW_SCAN=1

# Execute the original scanner
exec "$SCANNER_ORIGINAL" "$@"
WRAPPER
    chmod +x "$PLEX_APP/Plex Media Scanner"
    echo -e "${GREEN}  Scanner wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original scanner binary not found${NC}"
    exit 1
fi

echo ""
echo -e "${GREEN}=== Installation complete ===${NC}"
echo ""
echo "Wrappers installed. Start Plex normally - the shim will be auto-injected."
echo ""
echo "To uninstall:"
echo "  ./scripts/uninstall_wrappers.sh"

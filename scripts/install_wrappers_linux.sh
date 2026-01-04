#!/bin/bash
# Install Plex wrapper scripts for PostgreSQL shim (Linux)
# This replaces the Plex binaries with wrapper scripts that inject the shim

set -e

PLEX_DIR="${PLEX_DIR:-/usr/lib/plexmediaserver}"
SHIM_PATH="${SHIM_PATH:-/usr/local/lib/plex-postgresql/db_interpose_pg.so}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=== Plex PostgreSQL Wrapper Installer (Linux) ==="
echo ""

# Check if running as root
if [[ $EUID -ne 0 ]]; then
    echo -e "${RED}ERROR: This script must be run as root${NC}"
    echo "  sudo $0"
    exit 1
fi

# Check if shim exists
if [[ ! -f "$SHIM_PATH" ]]; then
    echo -e "${RED}ERROR: Shim not found at $SHIM_PATH${NC}"
    echo "Build and install the shim first:"
    echo "  make linux"
    echo "  sudo make install"
    exit 1
fi

# Check if Plex is running
if pgrep -f "Plex Media Server" >/dev/null 2>&1; then
    echo -e "${YELLOW}WARNING: Plex is running. Stop it first:${NC}"
    echo "  sudo systemctl stop plexmediaserver"
    exit 1
fi

# Backup and install Server wrapper
echo "Installing Plex Media Server wrapper..."
if [[ -f "$PLEX_DIR/Plex Media Server" && ! -f "$PLEX_DIR/Plex Media Server.original" ]]; then
    if file "$PLEX_DIR/Plex Media Server" | grep -q "ELF"; then
        echo "  Backing up original binary..."
        mv "$PLEX_DIR/Plex Media Server" "$PLEX_DIR/Plex Media Server.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not an ELF binary)${NC}"
    fi
fi

if [[ -f "$PLEX_DIR/Plex Media Server.original" ]]; then
    cat > "$PLEX_DIR/Plex Media Server" << 'WRAPPER'
#!/bin/bash
# Plex Media Server wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SERVER_BINARY="$SCRIPT_DIR/Plex Media Server.original"

# PostgreSQL shim
export LD_PRELOAD="/usr/local/lib/plex-postgresql/db_interpose_pg.so"
export PLEX_PG_HOST="${PLEX_PG_HOST:-localhost}"
export PLEX_PG_PORT="${PLEX_PG_PORT:-5432}"
export PLEX_PG_DATABASE="${PLEX_PG_DATABASE:-plex}"
export PLEX_PG_USER="${PLEX_PG_USER:-plex}"
export PLEX_PG_PASSWORD="${PLEX_PG_PASSWORD:-}"
export PLEX_PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
export PLEX_PG_POOL_SIZE="${PLEX_PG_POOL_SIZE:-50}"

# Execute the original server
exec "$SERVER_BINARY" "$@"
WRAPPER
    chmod +x "$PLEX_DIR/Plex Media Server"
    echo -e "${GREEN}  Server wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original binary not found${NC}"
    exit 1
fi

# Backup and install Scanner wrapper
echo "Installing Plex Media Scanner wrapper..."
if [[ -f "$PLEX_DIR/Plex Media Scanner" && ! -f "$PLEX_DIR/Plex Media Scanner.original" ]]; then
    if file "$PLEX_DIR/Plex Media Scanner" | grep -q "ELF"; then
        echo "  Backing up original binary..."
        mv "$PLEX_DIR/Plex Media Scanner" "$PLEX_DIR/Plex Media Scanner.original"
    else
        echo -e "${YELLOW}  Wrapper already installed (not an ELF binary)${NC}"
    fi
fi

if [[ -f "$PLEX_DIR/Plex Media Scanner.original" ]]; then
    cat > "$PLEX_DIR/Plex Media Scanner" << 'WRAPPER'
#!/bin/bash
# Plex Media Scanner wrapper for PostgreSQL shim

SCRIPT_DIR="$(dirname "$0")"
SCANNER_ORIGINAL="$SCRIPT_DIR/Plex Media Scanner.original"

# Ensure PostgreSQL shim is loaded
export LD_PRELOAD="${LD_PRELOAD:-/usr/local/lib/plex-postgresql/db_interpose_pg.so}"

# Execute the original scanner
exec "$SCANNER_ORIGINAL" "$@"
WRAPPER
    chmod +x "$PLEX_DIR/Plex Media Scanner"
    echo -e "${GREEN}  Scanner wrapper installed${NC}"
else
    echo -e "${RED}  ERROR: Original scanner binary not found${NC}"
    exit 1
fi

echo ""
echo -e "${GREEN}=== Installation complete ===${NC}"
echo ""
echo "Configure PostgreSQL connection in /etc/default/plexmediaserver:"
echo "  PLEX_PG_HOST=localhost"
echo "  PLEX_PG_DATABASE=plex"
echo "  PLEX_PG_USER=plex"
echo "  PLEX_PG_PASSWORD=yourpassword"
echo ""
echo "Then start Plex:"
echo "  sudo systemctl start plexmediaserver"
echo ""
echo "To uninstall:"
echo "  sudo ./scripts/uninstall_wrappers_linux.sh"

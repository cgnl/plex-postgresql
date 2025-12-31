#!/bin/bash
#
# Plex Metadata Refresh Script (Bash versie)
#
# Een eenvoudig Bash script voor het refreshen van Plex metadata items.
# Dit script gebruikt curl om de Plex HTTP API aan te roepen.
#
# Gebruik:
#   ./plex_refresh.sh <item_id>
#
# Voorbeeld:
#   export PLEX_TOKEN="your-token-here"
#   ./plex_refresh.sh 7082350
#

set -euo pipefail

# Configuratie
PLEX_URL="${PLEX_URL:-http://localhost:32400}"
PLEX_TOKEN="${PLEX_TOKEN:-}"

# Kleuren voor output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Functie: Print error en exit
error() {
    echo -e "${RED}✗ Error: $1${NC}" >&2
    exit 1
}

# Functie: Print success
success() {
    echo -e "${GREEN}✓ $1${NC}"
}

# Functie: Print warning
warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

# Functie: Toon help
show_help() {
    cat <<EOF
Gebruik: $(basename "$0") <item_id> [options]

Refresh een Plex metadata item via de HTTP API.

Argumenten:
    item_id          ID van het metadata item om te refreshen

Opties:
    -h, --help       Toon deze help
    -t, --token      Plex authentication token (kan ook via PLEX_TOKEN env var)
    -u, --url        Plex server URL (default: http://localhost:32400)
    -s, --section    Refresh een hele library sectie i.p.v. een enkel item
    --help-token     Toon instructies voor het verkrijgen van een token

Environment Variables:
    PLEX_TOKEN       Plex authentication token
    PLEX_URL         Plex server base URL

Voorbeelden:
    # Refresh een enkel item
    export PLEX_TOKEN="your-token-here"
    $(basename "$0") 7082350

    # Met expliciete token
    $(basename "$0") --token "YOUR_TOKEN" 7082350

    # Met custom Plex URL
    $(basename "$0") --url "http://192.168.1.100:32400" 7082350

    # Refresh een hele library sectie
    $(basename "$0") --section 1

EOF
}

# Functie: Toon token instructies
show_token_help() {
    cat <<EOF

Hoe verkrijg je een Plex authentication token:

Methode 1 - Via Browser Developer Tools:
1. Log in op je Plex Web App (${PLEX_URL}/web)
2. Open Developer Tools (F12)
3. Ga naar de Network tab
4. Refresh de pagina
5. Zoek een request naar de Plex server
6. In de request headers, zoek naar 'X-Plex-Token'
7. Kopieer de token waarde

Methode 2 - Via XML Request:
    curl -u "username:password" "https://plex.tv/users/sign_in.xml" \\
        -X POST -H "X-Plex-Product: Plex Media Server" \\
        -H "X-Plex-Version: 1.0" \\
        -H "X-Plex-Client-Identifier: unique-id" \\
        | grep -oP '(?<=authToken=")[^"]*'

Methode 3 - Via PlexAPI (Python):
    pip install plexapi
    python3 -c "from plexapi.myplex import MyPlexAccount; \\
                account = MyPlexAccount('username', 'password'); \\
                print(account.authenticationToken)"

Zet daarna de token als environment variable:
    export PLEX_TOKEN="your-token-here"

Of voeg toe aan ~/.bashrc of ~/.zshrc voor permanent gebruik:
    echo 'export PLEX_TOKEN="your-token-here"' >> ~/.bashrc

EOF
}

# Functie: Refresh een metadata item
refresh_item() {
    local item_id="$1"
    local endpoint="${PLEX_URL}/library/metadata/${item_id}/refresh"

    echo "Refreshing item ${item_id}..."

    local response
    local http_code

    # Voer de request uit en capture HTTP status code
    http_code=$(curl -s -w "%{http_code}" -o /dev/null -X PUT \
        "${endpoint}?X-Plex-Token=${PLEX_TOKEN}")

    case "$http_code" in
        200|204)
            success "Item ${item_id} successfully refreshed"
            return 0
            ;;
        401)
            error "Authentication failed - check your Plex token"
            ;;
        404)
            error "Item ${item_id} not found"
            ;;
        000)
            error "Could not connect to Plex at ${PLEX_URL}"
            ;;
        *)
            error "HTTP error ${http_code}"
            ;;
    esac
}

# Functie: Refresh een library sectie
refresh_section() {
    local section_id="$1"
    local endpoint="${PLEX_URL}/library/sections/${section_id}/refresh"

    echo "Refreshing library section ${section_id}..."

    local http_code
    http_code=$(curl -s -w "%{http_code}" -o /dev/null -X GET \
        "${endpoint}?X-Plex-Token=${PLEX_TOKEN}")

    case "$http_code" in
        200|204)
            success "Library section ${section_id} refresh initiated"
            return 0
            ;;
        401)
            error "Authentication failed - check your Plex token"
            ;;
        404)
            error "Library section ${section_id} not found"
            ;;
        000)
            error "Could not connect to Plex at ${PLEX_URL}"
            ;;
        *)
            error "HTTP error ${http_code}"
            ;;
    esac
}

# Parse command line argumenten
ITEM_ID=""
SECTION_ID=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            show_help
            exit 0
            ;;
        --help-token)
            show_token_help
            exit 0
            ;;
        -t|--token)
            PLEX_TOKEN="$2"
            shift 2
            ;;
        -u|--url)
            PLEX_URL="$2"
            shift 2
            ;;
        -s|--section)
            SECTION_ID="$2"
            shift 2
            ;;
        -*)
            error "Unknown option: $1"
            ;;
        *)
            ITEM_ID="$1"
            shift
            ;;
    esac
done

# Valideer dat we een token hebben
if [[ -z "$PLEX_TOKEN" ]]; then
    error "Plex token is required. Set PLEX_TOKEN environment variable or use --token option.
Run with --help-token for instructions."
fi

# Valideer dat we een item_id of section_id hebben
if [[ -z "$ITEM_ID" && -z "$SECTION_ID" ]]; then
    show_help
    error "Either item_id or --section is required"
fi

# Voer de refresh uit
if [[ -n "$SECTION_ID" ]]; then
    refresh_section "$SECTION_ID"
else
    refresh_item "$ITEM_ID"
fi

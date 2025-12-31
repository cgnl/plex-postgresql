#!/usr/bin/env python3
"""
Plex Metadata Item Refresh Script

Dit script gebruikt de Plex HTTP API om een specifiek metadata item
te refreshen nadat de database is geüpdatet.

Gebruik:
    python3 plex_refresh_item.py <item_id>

Voorbeeld:
    python3 plex_refresh_item.py 7082350
"""

import sys
import os
import requests
import argparse
from typing import Optional


class PlexRefresher:
    """Helper class voor het refreshen van Plex metadata items."""

    def __init__(self, base_url: str = "http://localhost:32400",
                 token: Optional[str] = None):
        """
        Initialiseer de PlexRefresher.

        Args:
            base_url: De base URL van de Plex server (default: localhost:32400)
            token: Plex authentication token (kan ook via env var PLEX_TOKEN)
        """
        self.base_url = base_url.rstrip('/')
        self.token = token or os.environ.get('PLEX_TOKEN')

        if not self.token:
            raise ValueError(
                "Plex token is vereist. Zet deze via PLEX_TOKEN environment "
                "variable of geef deze mee als argument."
            )

    def refresh_item(self, item_id: int) -> bool:
        """
        Refresh een specifiek metadata item.

        Args:
            item_id: De ID van het metadata item

        Returns:
            True als de refresh succesvol was, False anders
        """
        endpoint = f"{self.base_url}/library/metadata/{item_id}/refresh"
        params = {"X-Plex-Token": self.token}

        try:
            print(f"Refreshing item {item_id}...")
            response = requests.put(endpoint, params=params, timeout=10)
            response.raise_for_status()

            print(f"✓ Item {item_id} successfully refreshed")
            return True

        except requests.exceptions.HTTPError as e:
            if e.response.status_code == 404:
                print(f"✗ Item {item_id} not found")
            elif e.response.status_code == 401:
                print(f"✗ Authentication failed - check your Plex token")
            else:
                print(f"✗ HTTP error: {e}")
            return False

        except requests.exceptions.ConnectionError:
            print(f"✗ Could not connect to Plex at {self.base_url}")
            print("   Is Plex Media Server running?")
            return False

        except requests.exceptions.Timeout:
            print(f"✗ Request timed out")
            return False

        except Exception as e:
            print(f"✗ Unexpected error: {e}")
            return False

    def get_item_info(self, item_id: int) -> Optional[dict]:
        """
        Haal informatie op over een metadata item.

        Args:
            item_id: De ID van het metadata item

        Returns:
            Dictionary met item info, of None als het item niet bestaat
        """
        endpoint = f"{self.base_url}/library/metadata/{item_id}"
        params = {"X-Plex-Token": self.token}

        try:
            response = requests.get(endpoint, params=params, timeout=10)
            response.raise_for_status()

            # Parse XML response (Plex gebruikt XML)
            # Voor een volledige implementatie zou je een XML parser gebruiken
            return {"status": "found", "id": item_id}

        except requests.exceptions.HTTPError as e:
            if e.response.status_code == 404:
                return None
            raise

    def refresh_library_section(self, section_id: int) -> bool:
        """
        Refresh een hele library sectie.

        Args:
            section_id: De ID van de library sectie

        Returns:
            True als de refresh succesvol was, False anders
        """
        endpoint = f"{self.base_url}/library/sections/{section_id}/refresh"
        params = {"X-Plex-Token": self.token}

        try:
            print(f"Refreshing library section {section_id}...")
            response = requests.get(endpoint, params=params, timeout=10)
            response.raise_for_status()

            print(f"✓ Library section {section_id} refresh initiated")
            return True

        except Exception as e:
            print(f"✗ Failed to refresh library section: {e}")
            return False


def get_plex_token_instructions():
    """Print instructies voor het verkrijgen van een Plex token."""
    print("""
Hoe verkrijg je een Plex authentication token:

1. Log in op je Plex Web App (http://localhost:32400/web)
2. Open je browser's Developer Tools (F12)
3. Ga naar de Network tab
4. Refresh de pagina
5. Zoek naar een request naar de Plex server
6. In de request headers, zoek naar 'X-Plex-Token'
7. Kopieer de token waarde

Alternatief (via PlexAPI Python library):
    pip install plexapi
    python3 -c "from plexapi.myplex import MyPlexAccount; \
                account = MyPlexAccount('username', 'password'); \
                print(account.authenticationToken)"

Zet daarna de token als environment variable:
    export PLEX_TOKEN="your-token-here"

Of geef deze mee via command line argument:
    python3 plex_refresh_item.py --token "your-token-here" <item_id>
""")


def main():
    """Main entry point voor het script."""
    parser = argparse.ArgumentParser(
        description="Refresh Plex metadata items via HTTP API",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
    # Refresh een enkel item
    python3 plex_refresh_item.py 7082350

    # Met expliciete token
    python3 plex_refresh_item.py --token "YOUR_TOKEN" 7082350

    # Met custom Plex URL
    python3 plex_refresh_item.py --url "http://192.168.1.100:32400" 7082350

    # Refresh een hele library sectie
    python3 plex_refresh_item.py --section 1

    # Toon token instructies
    python3 plex_refresh_item.py --help-token
        """
    )

    parser.add_argument('item_id', type=int, nargs='?',
                        help='ID van het metadata item om te refreshen')
    parser.add_argument('--token', '-t',
                        help='Plex authentication token')
    parser.add_argument('--url', '-u', default='http://localhost:32400',
                        help='Plex server URL (default: http://localhost:32400)')
    parser.add_argument('--section', '-s', type=int,
                        help='Refresh een hele library sectie (ID)')
    parser.add_argument('--help-token', action='store_true',
                        help='Toon instructies voor het verkrijgen van een Plex token')

    args = parser.parse_args()

    # Toon token instructies indien gevraagd
    if args.help_token:
        get_plex_token_instructions()
        return 0

    # Valideer argumenten
    if not args.item_id and not args.section:
        parser.print_help()
        print("\nError: Either item_id or --section is required")
        return 1

    try:
        # Initialiseer refresher
        refresher = PlexRefresher(base_url=args.url, token=args.token)

        # Refresh library sectie indien gevraagd
        if args.section:
            success = refresher.refresh_library_section(args.section)
            return 0 if success else 1

        # Anders, refresh specifiek item
        success = refresher.refresh_item(args.item_id)
        return 0 if success else 1

    except ValueError as e:
        print(f"Error: {e}")
        print("\nRun with --help-token for instructions on getting a Plex token")
        return 1

    except KeyboardInterrupt:
        print("\nInterrupted by user")
        return 130


if __name__ == '__main__':
    sys.exit(main())

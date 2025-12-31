# Plex Metadata Refresh - Praktische Handleiding

## Probleem
Wanneer je de PostgreSQL database direct wijzigt (bijvoorbeeld `deleted_at = NULL` zetten), ziet Plex deze wijziging niet door in-memory caching. Een herstart van Plex is onpraktisch en tijdrovend.

## Oplossing
Gebruik Plex's ingebouwde HTTP API om metadata items te refreshen.

## Beschikbare Tools

### 1. Python Script (Aanbevolen)
Het meest uitgebreide script met error handling en meerdere opties.

```bash
# Installeer dependencies (eenmalig)
pip3 install requests

# Basis gebruik
export PLEX_TOKEN="your-token-here"
./plex_refresh_item.py 7082350

# Met opties
./plex_refresh_item.py --token "YOUR_TOKEN" --url "http://192.168.1.100:32400" 7082350

# Refresh hele library sectie
./plex_refresh_item.py --section 1

# Toon help
./plex_refresh_item.py --help

# Toon token instructies
./plex_refresh_item.py --help-token
```

### 2. Bash Script
Eenvoudiger alternatief dat alleen curl gebruikt (geen Python vereist).

```bash
# Basis gebruik
export PLEX_TOKEN="your-token-here"
./plex_refresh.sh 7082350

# Met opties
./plex_refresh.sh --token "YOUR_TOKEN" --url "http://192.168.1.100:32400" 7082350

# Refresh hele library sectie
./plex_refresh.sh --section 1

# Toon help
./plex_refresh.sh --help
```

### 3. Direct via curl
Voor snelle one-liners:

```bash
# Refresh een enkel item
curl -X PUT "http://localhost:32400/library/metadata/7082350/refresh?X-Plex-Token=YOUR_TOKEN"

# Refresh hele library sectie
curl -X GET "http://localhost:32400/library/sections/1/refresh?X-Plex-Token=YOUR_TOKEN"
```

## Plex Token Verkrijgen

### Methode 1: Via Browser (Makkelijkst)
1. Open Plex Web App: `http://localhost:32400/web`
2. Open Developer Tools (F12)
3. Ga naar Network tab
4. Refresh de pagina
5. Zoek een request naar Plex
6. In de headers, vind `X-Plex-Token`
7. Kopieer de waarde

### Methode 2: Via PlexAPI Python Library
```bash
pip3 install plexapi
python3 << EOF
from plexapi.myplex import MyPlexAccount
account = MyPlexAccount('username', 'password')
print(f"Token: {account.authenticationToken}")
EOF
```

### Methode 3: Via Plex.tv API
```bash
curl -u "username:password" "https://plex.tv/users/sign_in.xml" \
    -X POST \
    -H "X-Plex-Product: Plex Media Server" \
    -H "X-Plex-Version: 1.0" \
    -H "X-Plex-Client-Identifier: $(uuidgen)" \
    | grep -oP '(?<=authToken=")[^"]*'
```

### Token Permanent Opslaan
Voeg toe aan `~/.bashrc` of `~/.zshrc`:

```bash
echo 'export PLEX_TOKEN="your-token-here"' >> ~/.bashrc
source ~/.bashrc
```

## Complete Workflow: Verwijderd Item Terughalen

### Stap 1: Database Wijziging
```sql
-- Maak item weer zichtbaar
UPDATE metadata_items
SET deleted_at = NULL
WHERE id = 7082350;
```

### Stap 2: Refresh in Plex
```bash
# Optie A: Python script
./plex_refresh_item.py 7082350

# Optie B: Bash script
./plex_refresh.sh 7082350

# Optie C: Direct curl
curl -X PUT "http://localhost:32400/library/metadata/7082350/refresh?X-Plex-Token=$PLEX_TOKEN"
```

### Stap 3: Verificatie
Het item zou nu weer zichtbaar moeten zijn in Plex.

## Geautomatiseerde Workflow

### Gecombineerd Script
Maak `/Users/sander/plex-postgresql/restore_item.sh`:

```bash
#!/bin/bash
ITEM_ID="$1"

if [[ -z "$ITEM_ID" ]]; then
    echo "Usage: $0 <item_id>"
    exit 1
fi

echo "Restoring item $ITEM_ID..."

# 1. Database update
psql -U plex_user -d plex_db -c "
UPDATE metadata_items
SET deleted_at = NULL
WHERE id = $ITEM_ID;
"

# 2. Refresh in Plex
./plex_refresh.sh "$ITEM_ID"

echo "✓ Item $ITEM_ID restored and refreshed"
```

Gebruik:
```bash
chmod +x restore_item.sh
./restore_item.sh 7082350
```

### PostgreSQL Trigger + Daemon
Voor volledig geautomatiseerde synchronisatie, zie `LLDB_INVESTIGATION_RESULTS.md` sectie "Optie 3".

## Troubleshooting

### Error: "Authentication failed"
- Controleer of je Plex token correct is
- Token kan verlopen zijn, haal een nieuwe op
- Check of token correct is geëxporteerd: `echo $PLEX_TOKEN`

### Error: "Item not found"
- Controleer of het item ID correct is
- Item bestaat mogelijk niet in de database
- Gebruik SQL query om te verifiëren: `SELECT * FROM metadata_items WHERE id = 7082350;`

### Error: "Could not connect to Plex"
- Controleer of Plex Media Server draait: `pgrep -x "Plex Media Server"`
- Check Plex URL: standaard is `http://localhost:32400`
- Test met browser: `http://localhost:32400/web`

### Item wordt niet getoond na refresh
- Controleer of `deleted_at` echt NULL is in database
- Mogelijk zijn er andere filters actief (bijv. `hidden = 1`)
- Probeer hele library sectie te refreshen i.p.v. enkel item
- Check Plex logs: `~/Library/Logs/Plex Media Server/Plex Media Server.log`

## Advanced: LLDB Debugging

Voor diepgaand onderzoek naar Plex's interne werking, zie:
- `LLDB_INVESTIGATION_RESULTS.md` - Complete debugging analyse
- `lldb_cache_flush.py` - Experimenteel LLDB script

**Conclusie**: LLDB manipulatie is niet praktisch. HTTP API is de betrouwbare oplossing.

## API Referenties

### Belangrijke Endpoints

```
# Item refresh
PUT /library/metadata/{id}/refresh

# Sectie refresh
GET /library/sections/{id}/refresh

# Item info ophalen
GET /library/metadata/{id}

# Alle secties tonen
GET /library/sections

# Scan library
GET /library/sections/{id}/scan
```

### Response Codes
- `200/204`: Success
- `401`: Unauthorized (bad token)
- `404`: Not found
- `500`: Server error

## Bestand Overzicht

```
plex-postgresql/
├── plex_refresh_item.py           # Python script (uitgebreid)
├── plex_refresh.sh                # Bash script (eenvoudig)
├── LLDB_INVESTIGATION_RESULTS.md  # Technische analyse
├── README_METADATA_REFRESH.md     # Deze file
└── lldb_cache_flush.py            # Experimenteel (niet aanbevolen)
```

## Best Practices

1. **Gebruik altijd de HTTP API** voor metadata updates
2. **Test eerst met één item** voordat je bulk updates doet
3. **Bewaar je Plex token veilig** - dit is gelijkwaardig aan je wachtwoord
4. **Log database wijzigingen** voor audit trail
5. **Refresh specifieke items** i.p.v. hele library (sneller)

## Veiligheid

- **NOOIT** delen van Plex token in scripts of version control
- Gebruik environment variables voor tokens
- Overweeg een dedicated Plex user voor API toegang
- Beperk netwerk toegang tot Plex API indien mogelijk

## Performance

- Item refresh: ~1-2 seconden
- Library sectie refresh: Afhankelijk van grootte
- Batch refreshes: Voeg pauzes toe tussen requests

```bash
# Voorbeeld: Refresh meerdere items met pauze
for id in 7082350 7082351 7082352; do
    ./plex_refresh.sh "$id"
    sleep 1
done
```

## Verder Lezen

- Plex API Docs: https://plexapi.dev/
- PlexAPI Python: https://python-plexapi.readthedocs.io/
- Plex Forums: https://forums.plex.tv/

## Ondersteuning

Voor vragen of problemen:
1. Check eerst de Troubleshooting sectie
2. Controleer Plex logs
3. Raadpleeg `LLDB_INVESTIGATION_RESULTS.md` voor technische details
4. Test met curl voor isolatie van problemen

---

**Laatste update**: 24 december 2025
**Versie**: 1.0
**Status**: Productie-ready

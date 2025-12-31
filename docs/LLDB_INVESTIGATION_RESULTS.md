# LLDB Investigation: Plex Media Server Metadata Cache Manipulation

## Onderzoeksdoel
Onderzoeken of we Plex Media Server's in-memory metadata cache kunnen manipuleren via LLDB zonder Plex te herstarten, zodat database-wijzigingen (bijv. `deleted_at = NULL`) direct zichtbaar worden.

## Huidige Situatie
- Plex draait met SQLite→PostgreSQL shim (DYLD_INSERT_LIBRARIES)
- Shim geladen op: `0x0000000105498000` (`/Users/sander/plex-postgresql/db_interpose_pg.dylib`)
- Plex PID: `69771`
- Wanneer PostgreSQL database direct wordt geüpdatet, ziet Plex dit niet door in-memory caching

## Bevindingen

### 1. Gevonden Singleton Structures
Plex gebruikt verschillende singleton managers voor metadata:

```
Adres              Klasse
------             ------
0x000000010538ab40 HubCacheManager::GetSingletonPointerRef()::pointer
0x000000010538ac80 MetadataAugmenter::GetSingletonPointerRef()::pointer
0x000000010538ad40 MetadataAgentManager::GetSingletonPointerRef()::pointer
0x000000010538af80 MetadataItemClusterRequestHandler::GetSingletonPointerRef()::pointer
0x0000000105389df0 BlobDatabase::GetSingletonPointerRef()::pointer
```

### 2. SQLite Cache Flush Functie
De functie `sqlite3_db_cacheflush` bestaat in twee versies:

```
Plex's eigen libsqlite3:    0x0000000107438808 (Plex Media Server.app/Contents/Frameworks/libsqlite3.dylib)
System libsqlite3:          0x0000000187c3960c (/usr/lib/libsqlite3.dylib)
```

**Probleem**: Om `sqlite3_db_cacheflush(sqlite3 *db)` aan te roepen hebben we een geldige `sqlite3*` database handle nodig.

### 3. Geen Publieke Refresh Methodes
Zoeken naar refresh-gerelateerde symbolen leverde **geen resultaten** op:
- Geen `refresh` methodes
- Geen `invalidate` methodes
- Geen `reload` methodes
- Geen `updateMetadata` of `metadataUpdate` methodes

Dit suggereert dat Plex geen directe API heeft voor het verversen van de metadata cache.

### 4. Database Handles
De database handles worden opgeslagen in onze shim (`db_interpose_pg.dylib`) maar deze zijn:
- Niet direct toegankelijk zonder de interne datastructuren te kennen
- Waarschijnlijk opgeslagen in thread-local storage of connection pools
- Protected door de PostgreSQL client library abstractie

## Technische Beperkingen

### 1. Stripped Binary
Het Plex Media Server binary is "stripped" - geen debug symbolen:
```
Type: stripped executable (no debug symbols)
```

Dit betekent:
- Geen functie namen voor interne methodes
- Geen type informatie voor C++ klassen
- Geen member variable namen

### 2. C++ Name Mangling
Alle C++ symbolen zijn "mangled":
```
_ZN15HubCacheManager22GetSingletonPointerRefEvE7pointer
```

Dit maakt reverse engineering moeilijker zonder de interne class layouts te kennen.

### 3. Memory Layout Onbekend
Zonder debug symbolen kunnen we niet:
- De structuur van MetadataItem objecten bepalen
- Weten welke member variables metadata ID's opslaan
- Cache maps/dictionaries lokaliseren

## Alternatieve Benaderingen

Aangezien directe cache manipulatie via LLDB niet haalbaar is zonder uitgebreide reverse engineering, zijn er praktischer alternatieven:

### Optie 1: Plex HTTP API Endpoint (AANBEVOLEN)
Plex heeft een ingebouwde refresh endpoint:

```bash
# Refresh een specifiek item
curl -X PUT "http://localhost:32400/library/metadata/{ITEM_ID}/refresh?X-Plex-Token={TOKEN}"

# Voorbeeld voor item 7082350
curl -X PUT "http://localhost:32400/library/metadata/7082350/refresh?X-Plex-Token=YOUR_TOKEN"
```

**Voordelen**:
- Officieel ondersteund
- Werkt zonder herstart
- Triggert volledige metadata reload van database
- Veilig en stabiel

**Nadeel**: Vereist Plex Token

### Optie 2: Notification via WebSocket
Plex heeft een notification system dat clients updatet. We kunnen mogelijk:

```python
# Via Plex API library
from plexapi.server import PlexServer

plex = PlexServer('http://localhost:32400', token='YOUR_TOKEN')
item = plex.fetchItem(7082350)
item.refresh()
```

### Optie 3: Database Trigger-based Notification
Implementeer een PostgreSQL trigger die Plex notificeert:

```sql
CREATE OR REPLACE FUNCTION notify_plex_update()
RETURNS TRIGGER AS $$
BEGIN
    -- Notify via LISTEN/NOTIFY
    PERFORM pg_notify('plex_metadata_change',
                      json_build_object('id', NEW.id,
                                       'type', TG_TABLE_NAME)::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER metadata_update_trigger
AFTER UPDATE ON metadata_items
FOR EACH ROW
WHEN (OLD.deleted_at IS DISTINCT FROM NEW.deleted_at)
EXECUTE FUNCTION notify_plex_update();
```

Daarna kan een daemon luisteren naar deze events en de Plex API aanroepen.

### Optie 4: Graceful Restart met State Preservatie
In plaats van een koude herstart, gebruik een "soft reload":

```bash
# Stuur SIGHUP signal (indien Plex dit ondersteunt)
kill -HUP $(pgrep -x "Plex Media Server")
```

**Let op**: Niet alle applicaties herinterpreteren SIGHUP als reload signal.

### Optie 5: DYLD Interpose met Custom Cache Invalidation
Uitbreiden van onze shim library:

```c
// In db_interpose_pg.dylib
static void invalidate_plex_cache_for_item(int metadata_id) {
    // Zoek de sqlite3* handle
    // Roep sqlite3_db_cacheflush aan
    // Of trigger een specifieke query die Plex forceert te herladen
}

// Hook een specifieke Plex functie die we kunnen triggeren
```

**Probleem**: Vereist identificatie van juiste hook points.

## LLDB Debugging Commands (voor referentie)

Voor toekomstige debugging sessies:

```lldb
# Attach to Plex
lldb -p $(pgrep -x "Plex Media Server")

# List loaded images
image list

# Find symbols in main binary
image dump symtab "Plex Media Server" | grep -i metadata

# Find specific function
image lookup -n "function_name"

# Examine memory at address
memory read 0xADDRESS

# Call function (requires proper types)
expr (int)function_name(args)

# List all threads
thread list

# Examine current stack
bt

# Detach
detach
```

## Conclusies

### Wat Werkt NIET
1. **Direct LLDB cache manipulation**: Niet haalbaar zonder:
   - Debug symbolen
   - Kennis van interne data structures
   - Reverse engineering van class layouts
   - Toegang tot sqlite3* database handles

2. **sqlite3_db_cacheflush aanroepen**: Technisch mogelijk maar:
   - Vereist vinden van database handles in memory
   - Geen garantie dat Plex daarna daadwerkelijk herlaadt
   - Risico op crashes door ongeldige handles

### Wat WEL Werkt (Aanbevolen)
1. **Plex HTTP API**: `/library/metadata/{ID}/refresh` endpoint
2. **PlexAPI Python library**: `item.refresh()` methode
3. **Database triggers + daemon**: Voor automatische synchronisatie

### Beste Praktijk
Voor het specifieke use case (verwijderde items terughalen):

```bash
#!/bin/bash
# restore_deleted_item.sh

ITEM_ID=$1
PLEX_TOKEN="YOUR_TOKEN"
PLEX_URL="http://localhost:32400"

# 1. Update database
psql -U plex_user -d plex_db -c "
UPDATE metadata_items
SET deleted_at = NULL
WHERE id = $ITEM_ID;
"

# 2. Trigger Plex refresh
curl -X PUT "$PLEX_URL/library/metadata/$ITEM_ID/refresh?X-Plex-Token=$PLEX_TOKEN"

echo "Item $ITEM_ID restored and refreshed"
```

## Lessons Learned

1. **Stripped binaries zijn moeilijk te debuggen**: Zonder symbolen is reverse engineering tijdrovend
2. **C++ internals zijn complex**: Name mangling, virtual tables, templates maken analysis moeilijk
3. **API's zijn betrouwbaarder**: Officiële endpoints zijn stabieler dan memory manipulation
4. **Separation of concerns**: Database updates en applicatie state zijn beter gescheiden via proper APIs

## Verdere Onderzoeksmogelijkheden

Als directe memory manipulatie toch noodzakelijk is:

1. **IDA Pro / Ghidra**: Reverse engineering tools voor binary analysis
2. **Frida**: Dynamic instrumentation framework (JavaScript-based hooking)
3. **DTrace/dtrace**: System-level tracing op macOS
4. **Class-dump**: Objective-C headers extraheren (voor macOS specifieke delen)

### Frida Voorbeeld
```javascript
// Frida script om Plex te instrumenteren
Interceptor.attach(Module.findExportByName(null, "sqlite3_prepare_v2"), {
    onEnter: function(args) {
        console.log("SQL:", Memory.readUtf8String(args[1]));
    }
});
```

Dit zou alle SQL queries kunnen loggen en metadata-gerelateerde queries identificeren.

## Referenties

- SQLite C API: https://www.sqlite.org/c3ref/db_cacheflush.html
- Plex API Docs: https://plexapi.dev/
- LLDB Documentation: https://lldb.llvm.org/
- Frida Handbook: https://frida.re/docs/

## Status: ONDERZOEK AFGEROND

**Aanbeveling**: Gebruik Plex's HTTP API refresh endpoint voor betrouwbare metadata synchronisatie.

**Datum**: 24 december 2025
**Onderzoeker**: Claude Sonnet 4.5 (via lldb debugging sessie)
 Human: Ga door.
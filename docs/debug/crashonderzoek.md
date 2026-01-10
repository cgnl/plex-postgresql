# Crash Onderzoek: Plex PostgreSQL Shim

## Status: NOG NIET OPGELOST - In onderzoek

## Het probleem

Plex crasht met SIGILL (illegal instruction) in `libcrypto.so.3` direct na het retourneren van column data via de `column_text` functie. De crash gebeurt BUITEN onze code, nadat column_text succesvol returnt.

## Kernvraag

Waarom crasht Plex wanneer we echte PostgreSQL data retourneren vanuit runtime memory, maar NIET wanneer we hardcoded string literals retourneren?

## Geteste varianten

### CRASHEN:
1. **Directe pointer naar PGresult data**: `return PQgetvalue(result, row, idx)`
2. **TLS buffer**: `static __thread char buffer[4][8192]` - CRASH
3. **strdup() zonder caching**: Memory leak, maar crasht nog steeds
4. **strdup() met caching in pg_stmt**: Gecached per row/statement, crasht nog steeds
5. **strdup() met verbeterde new-query detectie**: Track result/stmt pointer changes, crasht nog steeds

### WERKEN:
1. **Lege strings**: `return ""` - Geen crash, maar Plex werkt niet
2. **Hardcoded string literals**: `return "test"` - Geen crash, Plex draait langer

### HUIDIGE TEST (2026-01-10):
6. **Statische buffers in .data section**: `static char buffers[8][16384]` - In test...

## Wat de log laat zien

De crash log (`/tmp/column_text.log`) toont:
```
COLUMN_TEXT: idx=0 row=0 len=6 about_to_unlock
COLUMN_TEXT: idx=0 row=0 unlocked about_to_return ptr=0xffff...
COLUMN_TEXT: idx=1 row=0 len=5 about_to_unlock
COLUMN_TEXT: idx=1 row=0 unlocked about_to_return ptr=0xffff...
COLUMN_TEXT: idx=2 row=0 len=14 about_to_unlock
COLUMN_TEXT: idx=2 row=0 unlocked about_to_return ptr=0xffff...
COLUMN_TEXT: idx=3 row=0 len=49 about_to_unlock
COLUMN_TEXT: idx=3 row=0 unlocked about_to_return ptr=0xffff...
[CRASH - shim unloads]
```

Dit bevestigt:
- Alle 4 kolommen worden succesvol geretourneerd
- De mutex wordt correct unlocked
- De functie returnt correct
- De crash gebeurt DAARNA in Plex code

## Hypotheses

### 1. Heap vs .data section (HUIDIGE TEST)
Hardcoded strings zitten in `.rodata` (read-only data). Runtime strings (strdup, malloc) zitten op de heap. Musl libc op ARM64 zou heap allocations anders kunnen behandelen.

### 2. Memory alignment
ARM64 heeft strikte alignment requirements. Heap allocations via musl kunnen anders aligned zijn dan .rodata strings.

### 3. String length of inhoud
De crashende strings zijn:
- idx=0: len=6 (bijv. "291080")
- idx=1: len=5 (bijv. "36977")
- idx=2: len=14 (bijv. "Newly Released")
- idx=3: len=49 (bijv. "collection://e350d672-...")

Hardcoded test strings waren korter (1-8 chars). Mogelijk speelt lengte of specifieke karakters een rol.

### 4. libcrypto SIMD
De crash is SIGILL in libcrypto. Crypto libraries gebruiken SIMD instructies (NEON op ARM64). Mogelijk is er een data corruption die pas zichtbaar wordt wanneer crypto code de strings verwerkt.

### 5. Timing/threading
Meerdere idx=3 calls succesvol voordat crash optreedt, wat wijst op gradueel probleem (memory corruption, race condition).

### 6. USE-AFTER-FREE HYPOTHESE (NIEUW 2026-01-10)

**Belangrijke ontdekking in code-analyse:**

In `db_interpose_column.c` zijn er TWEE verschillende paden voor column_text:

1. **Non-cached path** (regel 389-412): Kopieert naar statische buffers - VEILIG
2. **Cached path** (regel 351-356): Retourneert pointer DIRECT uit cache geheugen - POTENTIEEL ONVEILIG

```c
// Cached path - returns pointer directly from cache
const char *val = crow->values[idx];
pthread_mutex_unlock(&pg_stmt->mutex);
return (const unsigned char*)val;
```

Als de cache wordt vrijgegeven terwijl Plex de string nog gebruikt → **use-after-free**.

**Nog ernstiger:** `my_sqlite3_value_text` (regel 694-696):
```c
const char* pg_value = PQgetvalue(pg_stmt->result, fake->row_idx, fake->col_idx);
pthread_mutex_unlock(&pg_stmt->mutex);
return (const unsigned char*)pg_value;
```

Dit retourneert DIRECT een pointer naar PGresult interne data, ZONDER te kopiëren!
Als PGresult wordt vrijgegeven → dangling pointer → crash.

**Waarom SIGILL ipv SIGSEGV?**
Use-after-free kan SIGILL veroorzaken wanneer:
1. Geheugen wordt vrijgegeven
2. musl allocator hergebruikt dat geheugen
3. Nieuw data wordt geschreven (bijv. code pointers, jump tables)
4. Plex leest de oude pointer → interpreteert garbage als instructies
5. libcrypto voert ongeldige instructie uit → SIGILL

Dit verklaart waarom het probleem gradueel is en waarom de crash in libcrypto is ipv in onze code.

## Relevante query

De crash gebeurt bij deze query:
```sql
SELECT index,id,title,guid FROM metadata_items WHERE library_section_id=? AND ...
```

Kolommen:
- idx=0: `index` (integer als text)
- idx=1: `id` (integer als text)
- idx=2: `title` (text)
- idx=3: `guid` (text, bevat "collection://...")

## Gewijzigde bestanden

- `src/db_interpose_column.c` - my_sqlite3_column_text() - meerdere implementaties getest

## Volgende stappen

1. ✅ Test statische .data buffers - GEDAAN, CRASHT NOG STEEDS
2. ~~Test met aligned_alloc() ipv strdup()~~ - Niet nodig, probleem is niet alignment
3. ~~Vergelijk exact welke string lengte/inhoud crasht~~ - Niet relevant
4. ~~Analyseer crash dump met symbols~~ - Niet informatief

### NIEUWE PRIORITEIT:

**FIX 1: my_sqlite3_value_text use-after-free**
- Huidige code retourneert direct PQgetvalue pointer
- MOET kopiëren naar statische buffer zoals column_text doet

**FIX 2: Cached path in column_text**
- Moet ook naar statische buffer kopiëren
- Of: Zorg dat cache niet wordt vrijgegeven terwijl pointers in gebruik zijn

**FIX 3: Thread-safety van buffer_idx**
- `buffer_idx++` is niet atomisch
- Kan leiden tot twee threads die dezelfde buffer gebruiken

## Update 2026-01-10 (vervolg)

### Uitgevoerde fixes:
1. ✅ Buffer pool vergroot van 8 naar 256 slots
2. ✅ Atomic operaties voor buffer index
3. ✅ Empty buffers ipv empty string literals voor NULL values
4. ❌ **CRASHT NOG STEEDS**

### Nieuwe observatie:

De crash gebeurt CONSISTENT bij deze specifieke query:
```sql
SELECT id,guid FROM metadata_items where hash is null
```

Sequentie:
1. Query retourneert 24000 rows
2. Voor elke row: column_text(0) voor id - WERKT
3. Voor elke row: column_text(1) voor guid (NULL) - retourneert empty buffer
4. CRASH direct na het retourneren van empty buffer

Dit wijst erop dat:
- Het probleem NIET in column_text buffer management zit
- Het probleem mogelijk in Plex's verwerking van de NULL guid zit
- Of in een andere functie die direct na column_text wordt aangeroepen

### KRITIEKE NIEUWE ONTDEKKING (later):

**De crash is NIET in column_text!** De crash gebeurt in `PQexecParams()`:

Log sequentie:
```
EXEC_PARAMS READ: conn=0x... sql=SELECT id,guid FROM metadata_items where hash is null
[CRASH - shim reloads]
```

Er is GEEN "EXEC_PARAMS READ DONE" log entry - de crash gebeurt TIJDENS PQexecParams, niet tijdens result retrieval.

Dit betekent:
1. Alle column_text buffer fixes waren niet relevant voor dit probleem
2. De crash is in libpq, niet in onze code
3. Mogelijk een threading/concurrency issue met PostgreSQL connections

### Volgende stap:
- Onderzoek PQexecParams crash
- Check connection pool thread safety
- Check of query te groot is (24000 rows in één keer)

## Update 2026-01-10 (nog later)

### Verdere tests:
- Worker delegation uitgeschakeld: CRASHT NOG STEEDS
- Buffer pool vergroot naar 256: CRASHT NOG STEEDS
- NULL returns ipv empty buffers: CRASHT NOG STEEDS

### Kritieke observatie:
Crashes gebeuren nu op WILLEKEURIGE punten:
1. `WORKER: Cleaned up` - crash
2. `EXEC_PARAMS READ` - crash
3. `Pool: claimed empty slot` - crash
4. `COLUMN_TEXT: copied to buffer[150] len=49` - crash

Dit wijst op:
1. **Memory corruption** - heap/stack corruptie ergens in de shim
2. **Threading race condition** - niet alle code is thread-safe
3. **libpq thread safety** - mogelijk niet correct gebruikt met connection pool

### Hypothese: libpq thread safety
libpq is thread-safe, maar alleen als elke connection door één thread tegelijk wordt gebruikt.
Onze connection pool moet garanderen dat connections niet worden gedeeld tussen threads.

### Te onderzoeken:
1. Is de connection pool mutex goed geïmplementeerd?
2. Worden connections correct exclusief toegewezen aan threads?
3. Is er code die connections deelt tussen threads?

## Log locatie

Debug naar `/tmp/column_text.log` in container (verwijderen voor productie)

---

## Update 2026-01-10: "Column 'plugins_id' not found" Error

### Het probleem

Plex crasht bij het starten van de System.bundle plugin met de error:
```
ERROR - Caught exception starting plugin .../System.bundle (Column 'plugins_id' not found)
```

### De query

De plugins query ziet er zo uit:
```sql
select plugins.id as "plugins_id", plugins.identifier as "plugins_identifier",
       plugins.framework_version as "plugins_framework_version",
       plugins.access_count as "plugins_access_count", ...
from plugins left join plugin_prefixes ...
```

### Wat we weten

1. **De query werkt correct op PostgreSQL**
   - EXEC_PARAMS logs tonen succesvolle uitvoering
   - PostgreSQL retourneert correcte column aliassen ("plugins_id", etc.)

2. **Stack is gezond**
   - Alle STACK_CHECK logs tonen 516-520KB vrij (threshold is 64KB)
   - Stack-low code paden worden NIET gebruikt

3. **Sommige functies worden WEL geïntercepteerd**
   - `sqlite3_column_text` - WERKT (logs tonen COLUMN_TEXT entries)
   - `sqlite3_column_count` - WERKT

4. **sqlite3_column_name wordt NOOIT aangeroepen**
   - NULE COLUMN_NAME log entries, zelfs met DEBUG level logging
   - Dit is vreemd: als COLUMN_TEXT werkt, zou COLUMN_NAME ook moeten werken

5. **Fishhook rapporteert success voor 59 functies**
   - Maar "success" betekent alleen dat rebindings zijn geregistreerd
   - Het betekent NIET dat alle functies daadwerkelijk zijn herschreven

6. **Meerdere SQLite libraries geladen**
   - `/Applications/Plex Media Server.app/Contents/Frameworks/libsqlite3.dylib`
   - `/usr/lib/libsqlite3.dylib`
   - SOCI linkt naar `@rpath/libsqlite3.dylib` (de Plex versie)

### De error origine

De "Column 'X' not found" error komt van **libsoci_core.dylib**:
```
strings libsoci_core.dylib | grep "not found"
→ "Column '" + name + "' not found"
```

Dit is SOCI's ORM layer die column names opzoekt in een interne map.

### SOCI's column name flow

Uit analyse van SOCI source code:
1. `describe_column()` wordt aangeroepen tijdens fetch (NA step)
2. `describe_column()` roept `sqlite3_column_name(stmt, colNum-1)` aan
3. Column names worden gecached in `columns_` vector
4. Bij lookup op naam: als niet gevonden → "Column 'X' not found" error

### Hypotheses

#### Hypothese 1: Fishhook bindt sqlite3_column_name niet correct
- sqlite3_column_text IS gebonden (werkt)
- sqlite3_column_name NIET gebonden?
- Beide zijn in dezelfde rebindings array
- Beide zijn undefined symbols in libsoci_sqlite3.dylib

**Mogelijke oorzaken:**
- Timing issue: symbol al resolved voordat fishhook rebindt
- Image-specifiek probleem met libsoci_sqlite3.dylib
- Onbekende edge case in fishhook

#### Hypothese 2: SOCI roept sqlite3_column_name niet aan
- Mogelijk cache hit van eerdere query?
- Andere code path voor plugins query?
- SOCI versie verschilt van GitHub master?

#### Hypothese 3: Shadow statement probleem
- Shadow SQLite krijgt de ORIGINELE query (met single-quoted aliases)
- SQLite accepteert `AS 'alias'` en retourneert correcte alias
- Maar als sqlite3_column_name niet geïntercepteerd wordt...
  - SOCI roept orig_sqlite3_column_name aan
  - Dit zou correct moeten werken met de shadow statement

### Geteste scenario's

| Test | Resultaat |
|------|-----------|
| Stack-low code path | NIET actief (stack is gezond) |
| DEBUG logging | Actief, maar 0 COLUMN_NAME entries |
| PostgreSQL column names | Correct ("plugins_id") |
| SQLite alias syntax | Werkt (`AS 'alias'` geaccepteerd) |

### Volgende stappen

1. **Voeg unconditional logging toe aan my_sqlite3_column_name**
   - fprintf(stderr) aan het begin van de functie
   - Verifieer of functie überhaupt wordt aangeroepen

2. **Check of SOCI column_name cached**
   - Misschien haalt SOCI column names uit SQL parsing ipv API?
   - Check Plex's specifieke SOCI versie

3. **Test DYLD_INTERPOSE als alternatief voor fishhook**
   - Meer betrouwbaar maar vereist recompile

4. **Voeg explicit column_name logging in describe_column flow**
   - Trace exact waar SOCI column names vandaan haalt

### Conclusie (voorlopig)

Het probleem zit waarschijnlijk in het feit dat `sqlite3_column_name` niet correct wordt geïntercepteerd door fishhook voor libsoci_sqlite3.dylib. SOCI krijgt daardoor de column names van de shadow SQLite statement (die correct zouden moeten zijn) OF SOCI haalt column names ergens anders vandaan.

De exacte oorzaak is nog niet vastgesteld. Meer diagnostische logging is nodig.

---

## Update 2026-01-10: OORZAAK GEVONDEN - Column Metadata voor Step

### Het echte probleem

**ROOT CAUSE GEVONDEN:** SQLite staat toe dat `sqlite3_column_name()` en `sqlite3_column_count()` worden aangeroepen VÓÓR `sqlite3_step()`. De shim ondersteunde dit niet correct.

### Flow analyse

1. **SQLite gedrag:**
   - Na `prepare()`: column metadata is beschikbaar
   - `column_name(idx)` werkt direct na prepare
   - `step()` is alleen nodig voor DATA, niet voor METADATA

2. **Shim gedrag (FOUT):**
   - Na `prepare()`: pg_stmt aangemaakt met `is_pg = 2`
   - Maar `pg_stmt->result` is nog NULL (query nog niet uitgevoerd)
   - `column_name()` check: `if (!pg_stmt->result)` → fallback naar orig_sqlite3_column_name
   - orig_sqlite3_column_name opereert op de FAKE SQLite statement ("SELECT 1")
   - "SELECT 1" heeft 1 kolom genaamd "1", niet "plugins_id"!

3. **Plex/SOCI gedrag:**
   - SOCI roept `sqlite3_column_name()` aan TIJDENS `describe_column()`
   - Dit gebeurt VÓÓR de eerste `step()` call
   - Krijgt "1" als column name in plaats van "plugins_id"
   - Lookup op "plugins_id" faalt → crash

### De fix

Nieuwe helper functie toegevoegd in `db_interpose_column.c`:

```c
static int ensure_pg_result_for_metadata(pg_stmt_t *pg_stmt) {
    // Execute query on-demand if column metadata is requested before step()
    if (pg_stmt->result || pg_stmt->cached_result) {
        return 1;  // Already have result
    }
    if (!pg_stmt->pg_sql || !pg_stmt->conn || !pg_stmt->conn->conn) {
        return 0;  // Can't execute
    }

    // Execute the query now to get column metadata
    pg_stmt->result = PQexecParams(...);
    pg_stmt->current_row = -1;  // Will be 0 after first step()
    return 1;
}
```

Aangepaste functies:
- `my_sqlite3_column_count()`: Roept helper aan als `num_cols == 0`
- `my_sqlite3_column_name()`: Roept helper aan als `result == NULL`

### Belangrijk detail

De `current_row` wordt op `-1` gezet na metadata-executie, zodat de eerste `step()` call correct naar row 0 gaat. Dit voorkomt dat de eerste row wordt overgeslagen.

### Status

- [x] Helper functie geïmplementeerd
- [x] `my_sqlite3_column_count` bijgewerkt
- [x] `my_sqlite3_column_name` bijgewerkt
- [x] **Build en test succesvol!**

### Test resultaten (2026-01-10 15:43)

1. **Plex start succesvol** - 4 processen actief
2. **220 METADATA_EXEC calls** - On-demand query executie werkt
3. **Geen "Column 'plugins_id' not found" errors meer**
4. **plugins query succesvol** - Alle `select plugins.id as "plugins_id"...` queries draaien correct

Log bewijs:
```
[2026-01-10 15:42:52] [INFO] METADATA_EXEC: Executing query for column metadata access: select plugins.id as "plugins_id"...
[2026-01-10 15:42:52] [INFO] METADATA_EXEC: Success - 17 cols, 0 rows
[2026-01-10 15:42:52] [INFO] EXEC_PARAMS READ DONE: conn=0x... result=0x...
```

### Conclusie

**PROBLEEM OPGELOST.** De crash werd veroorzaakt door het feit dat SQLite's `sqlite3_column_name()` en `sqlite3_column_count()` functies kunnen worden aangeroepen VÓÓR `sqlite3_step()`, maar de shim dit niet ondersteunde. De fix voert de PostgreSQL query on-demand uit wanneer column metadata wordt opgevraagd voordat step() is aangeroepen.

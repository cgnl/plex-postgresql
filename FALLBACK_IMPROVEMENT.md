# PostgreSQL Fallback Improvement Guide

Dit document beschrijft hoe je systematisch de SQL translator kunt verbeteren om steeds meer queries naar PostgreSQL te migreren.

## Hoe het werkt

### 1. Automatische Fallback Logging

De shim logt automatisch alle queries die **niet** naar PostgreSQL kunnen:
- **Locatie**: `/tmp/plex_pg_fallbacks.log`
- **Bevat**: Originele SQL, vertaalde SQL, PostgreSQL error
- **Format**: Timestamped entries met context (CACHED READ, PREPARED READ, etc.)

### 2. Analyse van Fallbacks

Run het analyze script om statistieken te zien:

```bash
./scripts/analyze_fallbacks.sh
```

**Output geeft:**
- Totaal aantal fallbacks
- Fallbacks per error type
- Meest voorkomende failing queries
- Suggesties voor verbetering

### 3. Iteratief Verbeterproces

```
┌──────────────────────────────────────────┐
│ 1. Start Plex met shim                  │
│    → Fallbacks worden automatisch gelogd│
└────────────┬─────────────────────────────┘
             │
             ▼
┌──────────────────────────────────────────┐
│ 2. Analyze fallback log                 │
│    ./scripts/analyze_fallbacks.sh       │
└────────────┬─────────────────────────────┘
             │
             ▼
┌──────────────────────────────────────────┐
│ 3. Kies meest voorkomende error         │
│    → Focus op hoogste impact             │
└────────────┬─────────────────────────────┘
             │
             ▼
┌──────────────────────────────────────────┐
│ 4. Fix SQL translator                   │
│    → src/sql_translator.c aanpassen     │
└────────────┬─────────────────────────────┘
             │
             ▼
┌──────────────────────────────────────────┐
│ 5. Test fix                              │
│    → Rebuild shim                        │
│    → Restart Plex                        │
│    → Check of error verdwenen is         │
└────────────┬─────────────────────────────┘
             │
             ▼
┌──────────────────────────────────────────┐
│ 6. Repeat tot alle fallbacks weg zijn   │
└──────────────────────────────────────────┘
```

## Huidige Fallbacks (voorbeelden)

### 1. JSON operator issue

**Probleem:**
```sql
SELECT di.`queue_id`,di.`id`,di.`order`
FROM download_queue_items di
WHERE di.`status` IN (SELECT value FROM json_each('[1,2,3]'))
```

**Error:**
```
operator does not exist: integer = json
```

**Oorzaak:**
- SQLite's `json_each()` heeft andere syntax dan PostgreSQL
- SQLite: `SELECT value FROM json_each('[1,2,3]')`
- PostgreSQL: `SELECT value::text FROM json_array_elements('[1,2,3]'::json)`

**Fix locatie:** `src/sql_translator.c` - add `translate_json_each()` function

**Fix voorbeeld:**
```c
static char* translate_json_each(const char *sql) {
    // Replace: SELECT value FROM json_each('[...]')
    // With:    SELECT value::text FROM json_array_elements('[...]'::json)

    // Implementation here...
}
```

### 2. GROUP BY strict mode issue

**Probleem:**
```sql
SELECT metadata_item_id, metadata_item_cluster_id
FROM metadata_item_clusterings
GROUP BY metadata_item_id
```

**Error:**
```
column "metadata_item_cluster_id" must appear in the GROUP BY clause
```

**Oorzaak:**
- PostgreSQL strict mode: alle non-aggregate columns moeten in GROUP BY
- SQLite is minder strict

**Fix locatie:** `src/sql_translator.c` - enhance `GROUP BY` handling

**Fix voorbeeld:**
```c
// Auto-detect all selected columns and add to GROUP BY if not aggregate
static char* fix_group_by_strict(const char *sql) {
    // Parse SELECT columns
    // Check which are in GROUP BY
    // Add missing non-aggregate columns to GROUP BY

    // Implementation here...
}
```

## SQL Translator Verbeteren

### File Structure

```
src/sql_translator.c
├── translate_autoincrement()  // Already implemented
├── translate_datetime()        // Already implemented
├── translate_strftime()        // Already implemented
├── translate_unixepoch()       // Already implemented
├── translate_iif()             // Already implemented
├── translate_typeof()          // Already implemented
│
├── translate_json_each()       // TODO: Add this
├── fix_group_by_strict()       // TODO: Add this
└── sql_translate()             // Main orchestrator
```

### Toevoegen van nieuwe translator

**Stap 1:** Schrijf translator functie in `sql_translator.c`:

```c
static char* translate_new_feature(const char *sql) {
    char *result = malloc(MAX_SQL_LEN);
    if (!result) return NULL;

    // Your translation logic here
    // Pattern matching
    // Replacement

    return result;
}
```

**Stap 2:** Voeg toe aan `sql_translate()` pipeline:

```c
sql_translation_t sql_translate(const char *sql) {
    // ... existing translations ...

    // Your new translation
    temp = translate_new_feature(current);
    free(current);
    if (!temp) return NULL;
    current = temp;

    // ... rest of pipeline ...
}
```

**Stap 3:** Test:

```bash
make clean && make
make stop && make run
./scripts/analyze_fallbacks.sh
```

## Metrics

Track je progress:

```bash
# Before fix
./scripts/analyze_fallbacks.sh
# Total fallbacks: 10

# After fix
./scripts/analyze_fallbacks.sh
# Total fallbacks: 8  ✓ 2 queries fixed!
```

## Doel

**Target: 0 fallbacks** = 100% PostgreSQL-only

Op dit moment:
- ✅ SQLite gebruikt alleen voor internal state
- ✅ Alle data in PostgreSQL
- 🔄 Werk aan progress: bijna alle queries naar PostgreSQL
- 🎯 Doel: Elimineer alle fallbacks door SQL translator te verbeteren

## Tips

1. **Start met meest voorkomende errors** - hoogste impact
2. **Test incrementeel** - één fix per keer
3. **Gebruik logging** - zie direct resultaat
4. **Edge cases** - SQLite heeft veel variaties, test grondig
5. **PostgreSQL docs** - Check PostgreSQL equivalent syntax

## Resources

- PostgreSQL JSON functions: https://www.postgresql.org/docs/current/functions-json.html
- PostgreSQL GROUP BY: https://www.postgresql.org/docs/current/sql-select.html#SQL-GROUPBY
- SQLite → PostgreSQL migration: https://wiki.postgresql.org/wiki/Converting_from_other_Databases_to_PostgreSQL#SQLite

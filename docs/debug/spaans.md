# Plan: Spaanse Content Libraries voor Plex

## Overzicht

Dit plan beschrijft hoe je aparte Spaanse audio libraries kunt toevoegen aan je Plex setup via CLI Debrid (DMB). De oplossing maakt gebruik van CLI Debrid's version profiles en symlink organization om dezelfde content in meerdere talen te bewaren.

---

## Huidige Situatie

### CLI Debrid Configuratie
- **Config locatie:** `/Users/sander/docker/DMB/cli_debrid/data/config/config.json`
- **Database:** `/Users/sander/docker/DMB/cli_debrid/data/db_content/media_items.db`
- **Symlink base path:** `/mnt`
- **Huidige versies:** 2160p REMUX, 1080p REMUX, 2160p WEB, 1080p WEB, 2160p BEST, 1080p BEST
- **Taal filter:** `language_code: "en,nl"` (alle versies)

### Huidige Plex Libraries
| Library | Pad |
|---------|-----|
| Cloud Movies | `/mnt/Movies` |
| Cloud TV | `/mnt/Shows` |

### Bewijs dat CLI Debrid meerdere versies ondersteunt
Query op database toont films met meerdere versies:
```sql
SELECT imdb_id, title, version, state FROM media_items WHERE imdb_id='tt0021799';
-- Result: "Dirigible" heeft zowel "2160p BEST" als "1080p BEST" collected
```

---

## Stap 1: Nieuwe Version Profiles Toevoegen

### Locatie
`/Users/sander/docker/DMB/cli_debrid/data/config/config.json`

### Sectie
`Scraping.versions`

### Toe te voegen profiles

```json
"ES 2160p BEST": {
  "bitrate_weight": "3",
  "enable_hdr": true,
  "filter_in": [
    "\\b(Spanish|Español|ESP|Latino|Castellano|SPANISH|SPA)\\b"
  ],
  "filter_out": [
    "\\b(MP4|mp4|3D|SDR)\\b",
    "www",
    "RGzsRutracker"
  ],
  "hdr_weight": "3",
  "max_resolution": "2160p",
  "max_size_gb": null,
  "min_size_gb": 0.5,
  "min_bitrate_mbps": 0.01,
  "max_bitrate_mbps": null,
  "preferred_filter_in": [
    ["\\b(DV|DoVi|Dolby.Vision)\\b", 1000],
    ["\\b(REMUX|Remux)\\b", 800],
    ["\\b(Latino)\\b", 500],
    ["\\b(Castellano)\\b", 400],
    ["\\b(WEB-DL|WEB)\\b", 200]
  ],
  "preferred_filter_out": [
    ["[^\\x00-\\x7FÅÄÖåäö]", 5000]
  ],
  "require_physical_release": false,
  "resolution_wanted": "==",
  "resolution_weight": "3",
  "similarity_threshold": "0.8",
  "similarity_threshold_anime": "0.35",
  "similarity_weight": "3",
  "size_weight": "3",
  "wake_count": 0,
  "language_code": "es",
  "fallback_version": "ES 1080p BEST",
  "year_match_weight": 3,
  "anime_filter_mode": "None"
},
"ES 1080p BEST": {
  "bitrate_weight": "3",
  "enable_hdr": false,
  "filter_in": [
    "\\b(Spanish|Español|ESP|Latino|Castellano|SPANISH|SPA)\\b"
  ],
  "filter_out": [
    "\\b(x265|HEVC|MP4|mp4|HDR|H265|h265|3D|10bit)\\b",
    "RGzsRutracker"
  ],
  "hdr_weight": "3",
  "max_resolution": "1080p",
  "max_size_gb": null,
  "min_size_gb": 0.5,
  "min_bitrate_mbps": 0.01,
  "max_bitrate_mbps": null,
  "preferred_filter_in": [
    ["\\b(REMUX|Remux)\\b", 800],
    ["\\b(Latino)\\b", 500],
    ["\\b(Castellano)\\b", 400],
    ["\\b(WEB-DL|WEB)\\b", 200],
    ["\\b(BluRay|BLURAY)\\b", 300]
  ],
  "preferred_filter_out": [
    ["[^\\x00-\\x7FÅÄÖåäö]", 5000],
    ["\\b(CAM|TS|TC|HDTS|HDTC)\\b", 10000]
  ],
  "require_physical_release": false,
  "resolution_wanted": "==",
  "resolution_weight": "3",
  "similarity_threshold": "0.8",
  "similarity_threshold_anime": "0.35",
  "similarity_weight": "3",
  "size_weight": "3",
  "wake_count": 0,
  "language_code": "es",
  "fallback_version": "None",
  "year_match_weight": 3,
  "anime_filter_mode": "None"
}
```

---

## Stap 2: Symlink Organization Aanpassen

### Locatie
`/Users/sander/docker/DMB/cli_debrid/data/config/config.json`

### Sectie
`File Management`

### Wijzigingen

```json
{
  "symlink_organize_by_version": true,
  "symlink_folder_order": "type,version"
}
```

### Resulterende folder structuur

```
/mnt/
├── Movies/
│   ├── 2160p BEST/
│   │   └── Movie Name (2024)/
│   ├── 1080p BEST/
│   │   └── Movie Name (2024)/
│   ├── ES 2160p BEST/
│   │   └── Película Nombre (2024)/
│   └── ES 1080p BEST/
│       └── Película Nombre (2024)/
└── Shows/
    ├── 2160p BEST/
    │   └── Show Name/
    ├── 1080p BEST/
    │   └── Show Name/
    ├── ES 2160p BEST/
    │   └── Serie Nombre/
    └── ES 1080p BEST/
        └── Serie Nombre/
```

---

## Stap 3: Content Sources Updaten

### Optie A: Spaanse versies toevoegen aan bestaande sources

Voeg de ES versies toe aan de `versions` array van bestaande Content Sources:

```json
"Collected_1": {
  "versions": [
    "2160p REMUX",
    "1080p REMUX",
    "2160p BEST",
    "1080p BEST",
    "ES 2160p BEST",
    "ES 1080p BEST"
  ]
}
```

### Optie B: Aparte Spaanse Content Source (aanbevolen)

Maak een nieuwe MDBList source voor Spaanse content:

```json
"MDBList_Spanish": {
  "enabled": true,
  "urls": "https://mdblist.com/lists/YOUR_SPANISH_LIST",
  "versions": [
    "ES 2160p BEST",
    "ES 1080p BEST"
  ],
  "media_type": "All",
  "display_name": "Spanish Content",
  "allow_specials": false,
  "custom_symlink_subfolder": "",
  "cutoff_date": "",
  "exclude_genres": [],
  "type": "MDBList",
  "list_length_limit": "0"
}
```

### Aanbevolen Spaanse MDBLists

- `https://mdblist.com/lists/hdlists/spanish-movies` (als bestaat)
- Trakt lijsten met Spaanse content
- Custom MDBList aanmaken met populaire Spaanse/Latino films

---

## Stap 4: Plex Libraries Aanmaken

### Nieuwe Libraries

| Library Naam | Type | Pad | Taal |
|--------------|------|-----|------|
| Películas | Movies | `/mnt/Movies/ES 2160p BEST` + `/mnt/Movies/ES 1080p BEST` | Spanish |
| Series Español | TV Shows | `/mnt/Shows/ES 2160p BEST` + `/mnt/Shows/ES 1080p BEST` | Spanish |

### Stappen in Plex

1. **Settings → Libraries → Add Library**
2. **Type:** Movies / TV Shows
3. **Name:** Películas / Series Español
4. **Add folders:**
   - `/mnt/Movies/ES 2160p BEST`
   - `/mnt/Movies/ES 1080p BEST`
5. **Advanced:**
   - Language: Spanish
   - Scanner: Plex Movie / Plex TV Series
   - Agent: Plex Movie / Plex TV Series

---

## Stap 5: Bestaande Content Opnieuw Symlinken

Na het wijzigen van `symlink_organize_by_version`, moet je de symlinks opnieuw laten genereren:

### CLI Debrid UI
1. Ga naar CLI Debrid web interface
2. Navigeer naar Settings → File Management
3. Klik op "Recreate All Symlinks" of vergelijkbare optie

### Of via database reset (voorzichtig!)
```bash
# Backup eerst!
cp /Users/sander/docker/DMB/cli_debrid/data/db_content/media_items.db \
   /Users/sander/docker/DMB/cli_debrid/data/db_content/media_items.db.backup

# Reset symlink status (CLI Debrid zal ze opnieuw aanmaken)
sqlite3 /Users/sander/docker/DMB/cli_debrid/data/db_content/media_items.db \
  "UPDATE media_items SET symlinked = 0 WHERE symlinked = 1;"
```

---

## Volledige Config Diff

### File: `/Users/sander/docker/DMB/cli_debrid/data/config/config.json`

```diff
{
  "File Management": {
    "file_collection_management": "Symlinked/Local",
    "original_files_path": "/data/rclone_RD/__all__",
    "symlinked_files_path": "/mnt",
    "symlink_organize_by_type": true,
-   "symlink_organize_by_version": false,
+   "symlink_organize_by_version": true,
-   "symlink_folder_order": "type,version,resolution"
+   "symlink_folder_order": "type,version"
  },
  "Scraping": {
    "versions": {
      // ... bestaande versies ...
+     "ES 2160p BEST": { ... zie hierboven ... },
+     "ES 1080p BEST": { ... zie hierboven ... }
    }
  },
  "Content Sources": {
    "Collected_1": {
      "versions": [
        "2160p REMUX",
        "1080p REMUX",
        "2160p BEST",
        "1080p BEST",
+       "ES 2160p BEST",
+       "ES 1080p BEST"
      ]
    }
  }
}
```

---

## Impact Analyse

### Voordelen
- Spaanse content automatisch gescraped en georganiseerd
- Aparte Plex libraries voor verschillende talen
- Geen conflict met bestaande Engelse content
- CLI Debrid bewaart beide versies (bewezen door database analyse)

### Nadelen / Overwegingen
- **Dubbele opslag:** Elke film/serie kan in 2 talen worden opgeslagen
- **Scraping tijd:** Meer versies = langere scraping tijd
- **Real-Debrid ruimte:** Controleer beschikbare ruimte
- **Symlink reorganisatie:** Eenmalige actie, kan even duren

### Bestaande Libraries
Na `symlink_organize_by_version: true` veranderen de paden:
- **Oud:** `/mnt/Movies/Movie Name (2024)/`
- **Nieuw:** `/mnt/Movies/2160p BEST/Movie Name (2024)/`

**Actie vereist:** Update Plex library paden naar de nieuwe structuur!

---

## Checklist

- [ ] Backup maken van config.json
- [ ] Version profiles toevoegen (ES 2160p BEST, ES 1080p BEST)
- [ ] File Management settings aanpassen (symlink_organize_by_version: true)
- [ ] Content Sources updaten met ES versies
- [ ] DMB/CLI Debrid container herstarten
- [ ] Symlinks laten regenereren
- [ ] Plex library paden updaten voor bestaande libraries
- [ ] Nieuwe Spaanse Plex libraries aanmaken
- [ ] Testen: voeg een Spaanse film toe en controleer of beide versies worden gescraped

---

## Rollback Plan

Als iets misgaat:

```bash
# Restore backup
cp /Users/sander/docker/DMB/cli_debrid/data/config/config.json.backup \
   /Users/sander/docker/DMB/cli_debrid/data/config/config.json

# Restart container
docker restart dmb  # of de juiste container naam
```

---

## Alternatieve Aanpak: Dual Audio Releases

In plaats van aparte Spaanse releases, kun je ook zoeken naar **dual audio** releases die zowel Engels als Spaans bevatten:

```json
"Dual Audio": {
  "filter_in": [
    "\\b(Dual|DUAL|DualAudio|Dual.Audio|Multi)\\b",
    "\\b(ENG.SPA|SPA.ENG|English.Spanish)\\b"
  ],
  "language_code": "en,es"
}
```

Dit vermindert opslagruimte maar is afhankelijk van beschikbaarheid van dual audio releases.

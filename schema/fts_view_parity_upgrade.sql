-- Preserve existing view column order and types while adding SQLite FTS columns.
-- Build metadata vectors from source text with distinct field weights. Existing
-- search_vector uses overlapping weights, which can join phrases across fields.
-- Idempotent; no library rows are changed.
BEGIN;

CREATE OR REPLACE VIEW plex.fts4_metadata_titles AS
 SELECT metadata_items.id AS rowid,
    metadata_items.title,
    setweight(to_tsvector('simple', COALESCE(metadata_items.title, '')), 'A') ||
        setweight(to_tsvector('simple', COALESCE(metadata_items.title_sort, '')), 'B') ||
        setweight(to_tsvector('simple', COALESCE(metadata_items.original_title, '')), 'C') AS title_fts,
    metadata_items.title_sort,
    metadata_items.original_title
   FROM plex.metadata_items;

CREATE OR REPLACE VIEW plex.fts4_metadata_titles_icu AS
 SELECT metadata_items.id AS rowid,
    metadata_items.title,
    setweight(to_tsvector('simple', COALESCE(metadata_items.title, '')), 'A') ||
        setweight(to_tsvector('simple', COALESCE(metadata_items.title_sort, '')), 'B') ||
        setweight(to_tsvector('simple', COALESCE(metadata_items.original_title, '')), 'C') AS title_fts,
    metadata_items.title_sort,
    metadata_items.original_title
   FROM plex.metadata_items;

CREATE OR REPLACE VIEW plex.fts4_tag_titles AS
 SELECT tags.id AS rowid,
    tags.tag AS title,
    COALESCE(tags.search_vector, to_tsvector('simple', COALESCE(tags.tag, ''))) AS title_fts,
    tags.tag
   FROM plex.tags;

CREATE OR REPLACE VIEW plex.fts4_tag_titles_icu AS
 SELECT tags.id AS rowid,
    tags.tag AS title,
    COALESCE(tags.search_vector, to_tsvector('simple', COALESCE(tags.tag, ''))) AS title_fts,
    tags.tag
   FROM plex.tags;

COMMIT;

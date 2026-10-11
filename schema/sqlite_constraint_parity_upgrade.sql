-- Run with psql -X -v ON_ERROR_STOP=1 -v PG_SCHEMA=plex -f this-file.
-- All rows are preserved. Missing uniqueness is added atomically or the complete
-- upgrade fails with the table/key that needs administrator review.
BEGIN;
SELECT set_config('plex.sqlite_parity_schema', :'PG_SCHEMA', true);
DO $parity$
DECLARE
    target_schema text := current_setting('plex.sqlite_parity_schema');
    candidate record;
    existing record;
    table_oid oid;
    quoted_columns text;
    nonnull_predicate text;
    duplicates boolean;
BEGIN
    IF target_schema !~ '^[a-z_][a-z0-9_]*$' OR length(target_schema) > 63 THEN
        RAISE EXCEPTION 'Invalid PG_SCHEMA: use a lowercase SQL identifier';
    END IF;
    FOR candidate IN
        SELECT * FROM (VALUES
            ('play_queues', 'idx_play_queues_client_account_type', ARRAY['client_identifier', 'account_id', 'metadata_type']::text[]),
            ('media_provider_resources', 'idx_media_provider_resources_uuid', ARRAY['uuid']::text[]),
            ('media_grabs', 'idx_media_grabs_uuid', ARRAY['uuid']::text[]),
            ('external_metadata_sources', 'idx_external_metadata_sources_uri', ARRAY['uri']::text[]),
            ('blobs', 'idx_blobs_linked_type_id_blob_type', ARRAY['linked_type', 'linked_id', 'blob_type']::text[]),
            ('blobs', 'idx_blobs_linked_type_guid_blob_type', ARRAY['linked_type', 'linked_guid', 'blob_type']::text[]),
            ('metadata_agent_providers', 'idx_metadata_agent_providers_identifier', ARRAY['identifier']::text[]),
            ('metadata_agent_provider_group_items', 'idx_metadata_agent_provider_group_items_pair', ARRAY['metadata_agent_provider_group_id', 'metadata_agent_provider_id']::text[])
        ) AS source(table_name, index_name, columns)
    LOOP
        table_oid := to_regclass(format('%I.%I', target_schema, candidate.table_name));
        IF table_oid IS NULL THEN
            RAISE EXCEPTION 'Missing SQLite parity table %.%', target_schema, candidate.table_name;
        END IF;
        -- Serialize upgrades and block writes while checking and creating keys.
        EXECUTE format('LOCK TABLE %I.%I IN SHARE ROW EXCLUSIVE MODE', target_schema, candidate.table_name);
        SELECT i.indrelid, i.indisunique, i.indisvalid, i.indisready,
               i.indnkeyatts, i.indnatts, i.indnullsnotdistinct,
               i.indpred IS NULL AS unfiltered, i.indexprs IS NULL AS unexpressed,
               am.amname,
               ARRAY(SELECT a.attname::text FROM unnest(i.indkey) WITH ORDINALITY key(attnum, position)
                     JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=key.attnum
                     ORDER BY key.position) AS columns,
               NOT EXISTS(SELECT 1 FROM unnest(i.indoption) option WHERE option <> 0) AS ordinary_order,
               NOT EXISTS(SELECT 1 FROM unnest(i.indclass) class_oid JOIN pg_opclass op ON op.oid=class_oid WHERE NOT op.opcdefault) AS default_classes,
               NOT EXISTS(SELECT 1 FROM unnest(i.indkey, i.indcollation) key(attnum, collation_oid)
                          JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=key.attnum
                          WHERE key.collation_oid <> a.attcollation) AS default_collations
        INTO existing
        FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
        LEFT JOIN pg_index i ON i.indexrelid=c.oid LEFT JOIN pg_am am ON am.oid=c.relam
        WHERE n.nspname=target_schema AND c.relname=candidate.index_name;
        IF FOUND THEN
            IF existing.indrelid IS DISTINCT FROM table_oid
               OR existing.indisunique IS DISTINCT FROM true
               OR existing.indisvalid IS DISTINCT FROM true
               OR existing.indisready IS DISTINCT FROM true
               OR existing.columns IS DISTINCT FROM candidate.columns
               OR existing.indnkeyatts IS DISTINCT FROM cardinality(candidate.columns)
               OR existing.indnatts IS DISTINCT FROM cardinality(candidate.columns)
               OR existing.indnullsnotdistinct IS DISTINCT FROM false
               OR existing.unfiltered IS DISTINCT FROM true OR existing.unexpressed IS DISTINCT FROM true
               OR existing.amname IS DISTINCT FROM 'btree'
               OR existing.ordinary_order IS DISTINCT FROM true
               OR existing.default_classes IS DISTINCT FROM true OR existing.default_collations IS DISTINCT FROM true THEN
                RAISE EXCEPTION 'Refusing modified SQLite parity index %.%; inspect its definition before upgrading', target_schema, candidate.index_name;
            END IF;
            CONTINUE;
        END IF;
        SELECT string_agg(format('%I', column_name), ', ' ORDER BY position),
               string_agg(format('%I IS NOT NULL', column_name), ' AND ' ORDER BY position)
        INTO quoted_columns, nonnull_predicate
        FROM unnest(candidate.columns) WITH ORDINALITY columns(column_name, position);
        -- SQLite and PostgreSQL default uniqueness both permit repeated NULLs.
        EXECUTE format('SELECT EXISTS (SELECT 1 FROM %I.%I WHERE %s GROUP BY %s HAVING count(*) > 1)',
                       target_schema, candidate.table_name, nonnull_predicate, quoted_columns) INTO duplicates;
        IF duplicates THEN
            RAISE EXCEPTION 'Cannot restore SQLite uniqueness on %.% (%): duplicate non-NULL keys; preserve a backup and resolve conflicting rows explicitly', target_schema, candidate.table_name, quoted_columns;
        END IF;
        EXECUTE format('CREATE UNIQUE INDEX %I ON %I.%I USING btree (%s)', candidate.index_name, target_schema, candidate.table_name, quoted_columns);
    END LOOP;
END $parity$;
COMMIT;

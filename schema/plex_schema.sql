--
-- PostgreSQL database dump
--

\restrict k1FTTexA8Xe4WPWhJLvkzFfK77e8SGscLqfdbjfaYwEAOAK1wzzj90sahsatJMU

-- Dumped from database version 15.15 (Homebrew)
-- Dumped by pg_dump version 15.15 (Homebrew)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: plex; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA plex;


--
-- Name: clean_old_statistics_bandwidth(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.clean_old_statistics_bandwidth() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    cutoff_time BIGINT;
    deleted_count INTEGER;
BEGIN
    -- Only run cleanup every 1000 inserts (check using random)
    -- This prevents running on every single insert
    IF random() > 0.001 THEN
        RETURN NEW;
    END IF;
    
    -- Calculate cutoff: 7 days ago in unix timestamp
    cutoff_time := EXTRACT(EPOCH FROM (NOW() - INTERVAL '7 days'))::BIGINT;
    
    -- Delete old rows (limit to prevent long locks)
    DELETE FROM statistics_bandwidth 
    WHERE id IN (
        SELECT id FROM statistics_bandwidth 
        WHERE at < cutoff_time 
        LIMIT 10000
    );
    
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    
    IF deleted_count > 0 THEN
        RAISE NOTICE 'Cleaned % old statistics_bandwidth rows', deleted_count;
    END IF;
    
    RETURN NEW;
END;
$$;


--
-- Name: clean_old_statistics_resources(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.clean_old_statistics_resources() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    cutoff_time BIGINT;
BEGIN
    IF random() > 0.001 THEN
        RETURN NEW;
    END IF;
    
    cutoff_time := EXTRACT(EPOCH FROM (NOW() - INTERVAL '7 days'))::BIGINT;
    
    DELETE FROM statistics_resources 
    WHERE id IN (
        SELECT id FROM statistics_resources 
        WHERE at < cutoff_time 
        LIMIT 10000
    );
    
    RETURN NEW;
END;
$$;


--
-- Name: cleanup_statistics_bandwidth(integer); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.cleanup_statistics_bandwidth(days_to_keep integer DEFAULT 7) RETURNS integer
    LANGUAGE plpgsql
    AS $$
DECLARE
    cutoff_time BIGINT;
    total_deleted INTEGER := 0;
    batch_deleted INTEGER;
BEGIN
    cutoff_time := EXTRACT(EPOCH FROM (NOW() - (days_to_keep || ' days')::INTERVAL))::BIGINT;
    
    -- Delete in batches to prevent long locks
    LOOP
        DELETE FROM statistics_bandwidth 
        WHERE id IN (
            SELECT id FROM statistics_bandwidth 
            WHERE at < cutoff_time 
            LIMIT 50000
        );
        
        GET DIAGNOSTICS batch_deleted = ROW_COUNT;
        total_deleted := total_deleted + batch_deleted;
        
        EXIT WHEN batch_deleted = 0;
        
        -- Small pause between batches
        PERFORM pg_sleep(0.1);
    END LOOP;
    
    RETURN total_deleted;
END;
$$;


--
-- Name: fix_orphan_season_on_episode_insert(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.fix_orphan_season_on_episode_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    season_parent_id INTEGER;
    episode_file TEXT;
    show_name TEXT;
    found_show_id INTEGER;
    season_library_id INTEGER;
BEGIN
    IF NEW.metadata_type != 4 OR NEW.parent_id IS NULL THEN
        RETURN NEW;
    END IF;
    
    SELECT parent_id, library_section_id 
    INTO season_parent_id, season_library_id
    FROM plex.metadata_items 
    WHERE id = NEW.parent_id AND metadata_type = 3;
    
    IF season_parent_id IS NOT NULL THEN
        RETURN NEW;
    END IF;
    
    SELECT mp.file INTO episode_file
    FROM plex.media_items med
    JOIN plex.media_parts mp ON mp.media_item_id = med.id
    WHERE med.metadata_item_id = NEW.id
    LIMIT 1;
    
    IF episode_file IS NULL THEN
        RETURN NEW;
    END IF;
    
    show_name := TRIM((regexp_match(episode_file, '/([^/]+)\s*\(\d{4}\)'))[1]);
    
    IF show_name IS NULL THEN
        RETURN NEW;
    END IF;
    
    -- ALLEEN dezelfde library
    SELECT id INTO found_show_id
    FROM plex.metadata_items
    WHERE metadata_type = 2
      AND library_section_id = season_library_id
      AND title ILIKE show_name
    LIMIT 1;
    
    IF found_show_id IS NOT NULL THEN
        UPDATE plex.metadata_items 
        SET parent_id = found_show_id 
        WHERE id = NEW.parent_id AND parent_id IS NULL;
        
        RAISE NOTICE 'Fixed orphan season % -> show %', NEW.parent_id, found_show_id;
    END IF;
    
    RETURN NEW;
END;
$$;


--
-- Name: fix_orphan_season_on_media_part_insert(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.fix_orphan_season_on_media_part_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    episode_id INTEGER;
    season_id INTEGER;
    season_parent_id INTEGER;
    season_library_id INTEGER;
    show_name TEXT;
    found_show_id INTEGER;
BEGIN
    SELECT med.metadata_item_id INTO episode_id
    FROM plex.media_items med
    WHERE med.id = NEW.media_item_id;
    
    IF episode_id IS NULL THEN
        RETURN NEW;
    END IF;
    
    SELECT mi.parent_id INTO season_id
    FROM plex.metadata_items mi
    WHERE mi.id = episode_id AND mi.metadata_type = 4;
    
    IF season_id IS NULL THEN
        RETURN NEW;
    END IF;
    
    SELECT parent_id, library_section_id 
    INTO season_parent_id, season_library_id
    FROM plex.metadata_items 
    WHERE id = season_id AND metadata_type = 3;
    
    IF season_parent_id IS NOT NULL THEN
        RETURN NEW;
    END IF;
    
    show_name := TRIM((regexp_match(NEW.file, '/([^/]+)\s*\(\d{4}\)'))[1]);
    
    IF show_name IS NULL THEN
        RETURN NEW;
    END IF;
    
    -- ALLEEN dezelfde library
    SELECT id INTO found_show_id
    FROM plex.metadata_items
    WHERE metadata_type = 2
      AND library_section_id = season_library_id
      AND title ILIKE show_name
    LIMIT 1;
    
    IF found_show_id IS NOT NULL THEN
        UPDATE plex.metadata_items 
        SET parent_id = found_show_id 
        WHERE id = season_id AND parent_id IS NULL;
        
        RAISE NOTICE 'Fixed orphan season % -> show %', season_id, found_show_id;
    END IF;
    
    RETURN NEW;
END;
$$;


--
-- Name: group_concat(text, text); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.group_concat(text, text) RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $_$ 
  SELECT string_agg($1, $2) 
$_$;


--
-- Name: group_concat_bigint(text, bigint); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.group_concat_bigint(state text, val bigint) RETURNS text
    LANGUAGE plpgsql IMMUTABLE
    AS $$
BEGIN
    IF state IS NULL THEN
        RETURN val::text;
    ELSE
        RETURN state || ',' || val::text;
    END IF;
END;
$$;


--
-- Name: group_concat_int(text, integer); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.group_concat_int(state text, val integer) RETURNS text
    LANGUAGE plpgsql IMMUTABLE
    AS $$
BEGIN
    IF state IS NULL THEN
        RETURN val::text;
    ELSE
        RETURN state || ',' || val::text;
    END IF;
END;
$$;


--
-- Name: group_concat_text(text, text); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.group_concat_text(state text, val text) RETURNS text
    LANGUAGE plpgsql IMMUTABLE
    AS $$
BEGIN
    IF state IS NULL THEN
        RETURN val;
    ELSE
        RETURN state || ',' || val;
    END IF;
END;
$$;


--
-- Name: iif(boolean, anyelement, anyelement); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.iif(condition boolean, true_val anyelement, false_val anyelement) RETURNS anyelement
    LANGUAGE plpgsql IMMUTABLE
    AS $$
BEGIN
    IF condition THEN
        RETURN true_val;
    ELSE
        RETURN false_val;
    END IF;
END;
$$;


--
-- Name: integer_equals_text(integer, text); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.integer_equals_text(integer, text) RETURNS boolean
    LANGUAGE sql IMMUTABLE
    AS $_$
    SELECT $1 = $2::integer;
$_$;


--
-- Name: maybe_cleanup_statistics(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.maybe_cleanup_statistics() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    ctrl maintenance_control%ROWTYPE;
    cutoff_time BIGINT;
    deleted_count INTEGER;
BEGIN
    -- Get control record for this table
    SELECT * INTO ctrl FROM maintenance_control WHERE table_name = TG_TABLE_NAME;
    
    -- Skip if no control record or cleanup not due yet
    IF ctrl IS NULL OR ctrl.last_cleanup + ctrl.cleanup_interval > NOW() THEN
        RETURN NEW;
    END IF;
    
    -- Calculate cutoff
    cutoff_time := EXTRACT(EPOCH FROM (NOW() - (ctrl.retention_days || ' days')::INTERVAL))::BIGINT;
    
    -- Perform cleanup
    IF TG_TABLE_NAME = 'statistics_bandwidth' THEN
        DELETE FROM statistics_bandwidth WHERE at < cutoff_time;
    ELSIF TG_TABLE_NAME = 'statistics_resources' THEN
        DELETE FROM statistics_resources WHERE at < cutoff_time;
    END IF;
    
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    
    -- Update last cleanup time
    UPDATE maintenance_control SET last_cleanup = NOW() WHERE table_name = TG_TABLE_NAME;
    
    IF deleted_count > 0 THEN
        RAISE LOG 'Cleaned % rows from %', deleted_count, TG_TABLE_NAME;
    END IF;
    
    RETURN NEW;
END;
$$;


--
-- Name: metadata_items_search_trigger(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.metadata_items_search_trigger() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.search_vector :=
        setweight(to_tsvector('simple', COALESCE(NEW.title, '')), 'A') ||
        setweight(to_tsvector('simple', COALESCE(NEW.title_sort, '')), 'B') ||
        setweight(to_tsvector('simple', COALESCE(NEW.original_title, '')), 'B');
    RETURN NEW;
END;
$$;


--
-- Name: prevent_cross_section_parent(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.prevent_cross_section_parent() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    parent_section_id INTEGER;
BEGIN
    -- If parent_id is NULL, allow it
    IF NEW.parent_id IS NULL THEN
        RETURN NEW;
    END IF;
    
    -- Get parent's library_section_id
    SELECT library_section_id INTO parent_section_id
    FROM plex.metadata_items
    WHERE id = NEW.parent_id;
    
    -- If parent not found, allow (will fail on FK constraint anyway)
    IF parent_section_id IS NULL THEN
        RETURN NEW;
    END IF;
    
    -- Check if sections match
    IF NEW.library_section_id != parent_section_id THEN
        RAISE EXCEPTION 'Cross-section parent link prevented: child section % cannot have parent in section %', 
            NEW.library_section_id, parent_section_id
            USING HINT = 'Parent and child must be in the same library section',
                  ERRCODE = '23514';  -- check_violation
    END IF;
    
    RETURN NEW;
END;
$$;


--
-- Name: prevent_self_referential_parent(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.prevent_self_referential_parent() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    current_id INTEGER;
    parent_type INTEGER;
    depth INTEGER := 0;
    max_depth INTEGER := 20;
BEGIN
    -- Skip if no parent_id
    IF NEW.parent_id IS NULL THEN
        RETURN NEW;
    END IF;

    -- Rule 1: Item cannot be its own parent
    IF NEW.parent_id = NEW.id THEN
        NEW.parent_id := NULL;
        RAISE WARNING 'Prevented self-referential parent for item %', NEW.id;
        RETURN NEW;
    END IF;
    
    -- Get parent's metadata_type
    SELECT metadata_type INTO parent_type FROM metadata_items WHERE id = NEW.parent_id;
    
    -- Rule 2: Validate parent-child type relationships
    -- Valid: season(3)->show(2), episode(4)->season(3), episode(4)->episode(4), any->collection(18)
    IF parent_type IS NOT NULL AND parent_type != 18 THEN  -- 18 = collection, always allowed as parent
        CASE NEW.metadata_type
            WHEN 2 THEN  -- show: should NOT have a parent (except collection)
                RAISE WARNING 'Prevented: Show % cannot have non-collection parent (type %)', NEW.id, parent_type;
                NEW.parent_id := NULL;
                RETURN NEW;
            WHEN 3 THEN  -- season: parent must be show(2)
                IF parent_type != 2 THEN
                    RAISE WARNING 'Prevented: Season % must have show as parent, not type %', NEW.id, parent_type;
                    NEW.parent_id := NULL;
                    RETURN NEW;
                END IF;
            WHEN 4 THEN  -- episode: parent must be season(3) or episode(4)
                IF parent_type NOT IN (3, 4) THEN
                    RAISE WARNING 'Prevented: Episode % must have season/episode as parent, not type %', NEW.id, parent_type;
                    NEW.parent_id := NULL;
                    RETURN NEW;
                END IF;
            ELSE
                NULL;  -- Other types: no restriction
        END CASE;
    END IF;
    
    -- Rule 3: Prevent direct circular reference (A -> B -> A)
    IF EXISTS (SELECT 1 FROM metadata_items WHERE id = NEW.parent_id AND parent_id = NEW.id) THEN
        RAISE WARNING 'Prevented circular reference: % <-> %', NEW.id, NEW.parent_id;
        NEW.parent_id := NULL;
        RETURN NEW;
    END IF;
    
    -- Rule 4: Prevent deeper circular references
    current_id := NEW.parent_id;
    WHILE current_id IS NOT NULL AND depth < max_depth LOOP
        IF current_id = NEW.id THEN
            RAISE WARNING 'Prevented circular chain at depth %: id=%', depth, NEW.id;
            NEW.parent_id := NULL;
            RETURN NEW;
        END IF;
        SELECT parent_id INTO current_id FROM metadata_items WHERE id = current_id;
        depth := depth + 1;
    END LOOP;
    
    RETURN NEW;
END;
$$;


--
-- Name: reject_empty_statistics(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.reject_empty_statistics() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
  IF NEW.count = 0 AND NEW.duration = 0 THEN
    RETURN NULL;
  END IF;
  RETURN NEW;
END;
$$;


--
-- Name: run_statistics_cleanup(integer); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.run_statistics_cleanup(p_days_to_keep integer DEFAULT NULL::integer) RETURNS TABLE(table_name text, rows_deleted bigint)
    LANGUAGE plpgsql
    AS $$
DECLARE
    cutoff_time BIGINT;
    ctrl maintenance_control%ROWTYPE;
    del_count BIGINT;
BEGIN
    FOR ctrl IN SELECT * FROM maintenance_control LOOP
        -- Use parameter if provided, otherwise use table's retention setting
        cutoff_time := EXTRACT(EPOCH FROM (NOW() - (COALESCE(p_days_to_keep, ctrl.retention_days) || ' days')::INTERVAL))::BIGINT;
        
        IF ctrl.table_name = 'statistics_bandwidth' THEN
            DELETE FROM statistics_bandwidth WHERE at < cutoff_time;
        ELSIF ctrl.table_name = 'statistics_resources' THEN
            DELETE FROM statistics_resources WHERE at < cutoff_time;
        END IF;
        
        GET DIAGNOSTICS del_count = ROW_COUNT;
        
        -- Update last cleanup time
        UPDATE maintenance_control SET last_cleanup = NOW() WHERE maintenance_control.table_name = ctrl.table_name;
        
        table_name := ctrl.table_name;
        rows_deleted := del_count;
        RETURN NEXT;
    END LOOP;
END;
$$;


--
-- Name: set_available_at(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.set_available_at() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.available_at IS NULL AND NEW.added_at IS NOT NULL THEN
        NEW.available_at := NEW.added_at;
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: sqlite_typeof(anyelement); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.sqlite_typeof(val anyelement) RETURNS text
    LANGUAGE plpgsql IMMUTABLE
    AS $$
BEGIN
    IF val IS NULL THEN
        RETURN 'null';
    END IF;
    CASE pg_typeof(val)::text
        WHEN 'integer', 'bigint', 'smallint' THEN RETURN 'integer';
        WHEN 'real', 'double precision', 'numeric' THEN RETURN 'real';
        WHEN 'text', 'character varying', 'character' THEN RETURN 'text';
        WHEN 'bytea' THEN RETURN 'blob';
        ELSE RETURN 'text';
    END CASE;
END;
$$;


--
-- Name: tags_search_trigger(); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.tags_search_trigger() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.search_vector := to_tsvector('simple', COALESCE(NEW.tag, ''));
    RETURN NEW;
END;
$$;


--
-- Name: text_equals_integer(text, integer); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.text_equals_integer(text, integer) RETURNS boolean
    LANGUAGE sql IMMUTABLE
    AS $_$
    SELECT $1::integer = $2;
$_$;


--
-- Name: unixepoch(text, text); Type: FUNCTION; Schema: plex; Owner: -
--

CREATE FUNCTION plex.unixepoch(ts text DEFAULT 'now'::text, modifier text DEFAULT NULL::text) RETURNS bigint
    LANGUAGE plpgsql STABLE
    AS $$
DECLARE
    result TIMESTAMP;
BEGIN
    IF ts = 'now' THEN
        result := NOW();
    ELSE
        result := ts::timestamp;
    END IF;

    IF modifier IS NOT NULL THEN
        result := result + modifier::interval;
    END IF;

    RETURN EXTRACT(EPOCH FROM result)::bigint;
END;
$$;


--
-- Name: group_concat(integer); Type: AGGREGATE; Schema: plex; Owner: -
--

CREATE AGGREGATE plex.group_concat(integer) (
    SFUNC = plex.group_concat_int,
    STYPE = text
);


--
-- Name: group_concat(bigint); Type: AGGREGATE; Schema: plex; Owner: -
--

CREATE AGGREGATE plex.group_concat(bigint) (
    SFUNC = plex.group_concat_bigint,
    STYPE = text
);


--
-- Name: group_concat(text); Type: AGGREGATE; Schema: plex; Owner: -
--

CREATE AGGREGATE plex.group_concat(text) (
    SFUNC = plex.group_concat_text,
    STYPE = text
);


--
-- Name: =; Type: OPERATOR; Schema: plex; Owner: -
--

CREATE OPERATOR plex.= (
    FUNCTION = plex.integer_equals_text,
    LEFTARG = integer,
    RIGHTARG = text,
    COMMUTATOR = OPERATOR(plex.=)
);


--
-- Name: =; Type: OPERATOR; Schema: plex; Owner: -
--

CREATE OPERATOR plex.= (
    FUNCTION = plex.text_equals_integer,
    LEFTARG = text,
    RIGHTARG = integer,
    COMMUTATOR = OPERATOR(plex.=)
);


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: accounts; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.accounts (
    id integer NOT NULL,
    name text,
    created_at bigint,
    updated_at bigint,
    default_audio_language text,
    default_subtitle_language text,
    auto_select_subtitle integer DEFAULT 1,
    auto_select_audio integer DEFAULT 1
);


--
-- Name: accounts_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.accounts_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: accounts_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.accounts_id_seq OWNED BY plex.accounts.id;


--
-- Name: activities; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.activities (
    id integer NOT NULL,
    parent_id integer,
    type text,
    title text,
    subtitle text,
    scheduled_at bigint,
    started_at bigint,
    finished_at bigint,
    cancelled integer
);


--
-- Name: activities_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.activities_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: activities_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.activities_id_seq OWNED BY plex.activities.id;


--
-- Name: blobs; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.blobs (
    id integer NOT NULL,
    blob bytea,
    linked_type text,
    linked_id integer,
    linked_guid text,
    created_at bigint,
    blob_type integer
);


--
-- Name: blobs_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.blobs_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: blobs_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.blobs_id_seq OWNED BY plex.blobs.id;




--
-- Name: custom_channels; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.custom_channels (
    id integer NOT NULL,
    name text,
    description text,
    playlist_id integer,
    start_time bigint,
    ordering integer,
    visibility integer,
    displayed_on integer,
    content_rating text
);


--
-- Name: custom_channels_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.custom_channels_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: custom_channels_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.custom_channels_id_seq OWNED BY plex.custom_channels.id;


--
-- Name: devices; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.devices (
    id integer NOT NULL,
    identifier text,
    name text,
    created_at bigint,
    updated_at bigint,
    platform text
);


--
-- Name: devices_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.devices_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: devices_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.devices_id_seq OWNED BY plex.devices.id;


--
-- Name: directories; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.directories (
    id integer NOT NULL,
    library_section_id integer,
    parent_directory_id integer,
    path text,
    created_at bigint,
    updated_at bigint,
    deleted_at bigint
);


--
-- Name: directories_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.directories_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: directories_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.directories_id_seq OWNED BY plex.directories.id;


--
-- Name: download_queue_items; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.download_queue_items (
    id integer NOT NULL,
    queue_id integer,
    key text,
    "order" integer,
    status integer,
    decision_params text,
    error text,
    decision_result text,
    metadata_item_id integer,
    media_part_id integer,
    expiration bigint,
    extra_data text
);


--
-- Name: download_queue_items_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.download_queue_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: download_queue_items_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.download_queue_items_id_seq OWNED BY plex.download_queue_items.id;


--
-- Name: download_queues; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.download_queues (
    id integer NOT NULL,
    owner integer,
    client_identifier text,
    extra_data text
);


--
-- Name: download_queues_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.download_queues_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: download_queues_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.download_queues_id_seq OWNED BY plex.download_queues.id;


--
-- Name: external_metadata_items; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.external_metadata_items (
    id integer,
    external_metadata_source_id integer,
    library_section_id integer,
    metadata_type integer,
    guid text,
    title text,
    parent_title text,
    year integer,
    added_at integer,
    updated_at integer,
    extra_data text
);


--
-- Name: external_metadata_sources; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.external_metadata_sources (
    id integer NOT NULL,
    uri text,
    source_title text,
    user_title text,
    online integer
);


--
-- Name: external_metadata_sources_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.external_metadata_sources_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: external_metadata_sources_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.external_metadata_sources_id_seq OWNED BY plex.external_metadata_sources.id;


--
-- Name: metadata_items; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_items (
    id integer NOT NULL,
    library_section_id integer,
    parent_id integer,
    metadata_type integer,
    guid text,
    media_item_count integer,
    title text,
    title_sort text,
    original_title text,
    studio text,
    rating double precision,
    rating_count integer,
    tagline text,
    summary text,
    trivia text,
    quotes text,
    content_rating text,
    content_rating_age integer,
    index integer,
    absolute_index integer,
    duration integer,
    user_thumb_url text,
    user_art_url text,
    user_banner_url text,
    user_music_url text,
    user_fields text,
    tags_genre text,
    tags_collection text,
    tags_director text,
    tags_writer text,
    tags_star text,
    originally_available_at bigint,
    available_at bigint,
    expires_at bigint,
    refreshed_at bigint,
    year integer,
    added_at bigint,
    created_at bigint,
    updated_at bigint,
    deleted_at bigint,
    tags_country text,
    extra_data text,
    hash text,
    audience_rating double precision,
    changed_at bigint DEFAULT 0,
    resources_changed_at bigint DEFAULT 0,
    remote integer,
    edition_title text,
    slug text,
    user_clear_logo_url text,
    user_square_art_url text,
    is_adult integer,
    metadata_agent_provider_group_id integer,
    search_vector tsvector,
    subtype integer GENERATED ALWAYS AS ((rating_count - ((rating_count / 100) * 100))) STORED,
    title_fts tsvector
);
ALTER TABLE ONLY plex.metadata_items ALTER COLUMN library_section_id SET STATISTICS 200;
ALTER TABLE ONLY plex.metadata_items ALTER COLUMN metadata_type SET STATISTICS 200;


--
-- Name: fts4_metadata_titles; Type: VIEW; Schema: plex; Owner: -
--

CREATE VIEW plex.fts4_metadata_titles AS
 SELECT metadata_items.id AS rowid,
    metadata_items.title,
    setweight(to_tsvector('simple', COALESCE(metadata_items.title, '')), 'A') ||
        setweight(to_tsvector('simple', COALESCE(metadata_items.title_sort, '')), 'B') ||
        setweight(to_tsvector('simple', COALESCE(metadata_items.original_title, '')), 'C') AS title_fts,
    metadata_items.title_sort,
    metadata_items.original_title
   FROM plex.metadata_items;


--
-- Name: fts4_metadata_titles_icu; Type: VIEW; Schema: plex; Owner: -
--

CREATE VIEW plex.fts4_metadata_titles_icu AS
 SELECT metadata_items.id AS rowid,
    metadata_items.title,
    setweight(to_tsvector('simple', COALESCE(metadata_items.title, '')), 'A') ||
        setweight(to_tsvector('simple', COALESCE(metadata_items.title_sort, '')), 'B') ||
        setweight(to_tsvector('simple', COALESCE(metadata_items.original_title, '')), 'C') AS title_fts,
    metadata_items.title_sort,
    metadata_items.original_title
   FROM plex.metadata_items;


--
-- Name: tags; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.tags (
    id integer NOT NULL,
    metadata_item_id integer,
    tag text,
    tag_type integer,
    user_thumb_url text,
    user_art_url text,
    user_music_url text,
    created_at bigint,
    updated_at bigint,
    tag_value integer,
    extra_data text,
    key text,
    parent_id integer,
    search_vector tsvector
);


--
-- Name: fts4_tag_titles; Type: VIEW; Schema: plex; Owner: -
--

CREATE VIEW plex.fts4_tag_titles AS
 SELECT tags.id AS rowid,
    tags.tag AS title,
    COALESCE(tags.search_vector, to_tsvector('simple', COALESCE(tags.tag, ''))) AS title_fts,
    tags.tag
   FROM plex.tags;


--
-- Name: fts4_tag_titles_icu; Type: VIEW; Schema: plex; Owner: -
--

CREATE VIEW plex.fts4_tag_titles_icu AS
 SELECT tags.id AS rowid,
    tags.tag AS title,
    COALESCE(tags.search_vector, to_tsvector('simple', COALESCE(tags.tag, ''))) AS title_fts,
    tags.tag
   FROM plex.tags;


--
-- Name: hub_templates; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.hub_templates (
    id integer NOT NULL,
    section text,
    identifier text,
    title text,
    home_visibility integer,
    recommended_visibility integer,
    "order" double precision,
    extra_data text
);


--
-- Name: hub_templates_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.hub_templates_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: hub_templates_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.hub_templates_id_seq OWNED BY plex.hub_templates.id;


--
-- Name: library_section_permissions; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.library_section_permissions (
    id integer NOT NULL,
    library_section_id integer,
    account_id integer,
    permission integer
);


--
-- Name: library_section_permissions_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.library_section_permissions_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: library_section_permissions_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.library_section_permissions_id_seq OWNED BY plex.library_section_permissions.id;


--
-- Name: library_sections; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.library_sections (
    id integer NOT NULL,
    library_id bigint,
    name text,
    name_sort text,
    section_type integer,
    language text,
    agent text,
    scanner text,
    user_thumb_url text,
    user_art_url text,
    user_theme_music_url text,
    public integer,
    created_at bigint,
    updated_at bigint,
    scanned_at bigint,
    display_secondary_level integer,
    user_fields text,
    query_xml text,
    query_type integer,
    uuid text,
    changed_at bigint DEFAULT 0,
    content_changed_at bigint DEFAULT 0,
    metadata_agent_provider_group_id integer
);


--
-- Name: library_sections_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.library_sections_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: library_sections_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.library_sections_id_seq OWNED BY plex.library_sections.id;


--
-- Name: locatables; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.locatables (
    id integer NOT NULL,
    location_id integer NOT NULL,
    locatable_id integer NOT NULL,
    locatable_type text NOT NULL,
    created_at bigint,
    updated_at bigint,
    extra_data text,
    geocoding_version integer
);


--
-- Name: locatables_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.locatables_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: locatables_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.locatables_id_seq OWNED BY plex.locatables.id;


--
-- Name: location_places; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.location_places (
    id integer NOT NULL,
    location_id integer,
    guid text NOT NULL
);


--
-- Name: location_places_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.location_places_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: location_places_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.location_places_id_seq OWNED BY plex.location_places.id;


--
-- Name: locations; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.locations (
    id integer NOT NULL,
    lat_min double precision,
    lat_max double precision,
    lon_min double precision,
    lon_max double precision
);


--
-- Name: locations_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.locations_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: locations_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.locations_id_seq OWNED BY plex.locations.id;


--
-- Name: locations_node; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.locations_node (
    nodeno integer,
    data bytea
);


--
-- Name: locations_parent; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.locations_parent (
    nodeno integer,
    parentnode integer
);


--
-- Name: locations_rowid; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.locations_rowid (
    rowid integer,
    nodeno integer
);


--
-- Name: maintenance_control; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.maintenance_control (
    table_name text NOT NULL,
    last_cleanup timestamp without time zone DEFAULT '1970-01-01 00:00:00'::timestamp without time zone,
    cleanup_interval interval DEFAULT '01:00:00'::interval,
    retention_days integer DEFAULT 7
);


--
-- Name: media_grabs; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_grabs (
    id integer NOT NULL,
    uuid text,
    status integer,
    error integer,
    metadata_item_id integer,
    media_subscription_id integer,
    extra_data text,
    created_at bigint,
    updated_at bigint
);


--
-- Name: media_grabs_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_grabs_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_grabs_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_grabs_id_seq OWNED BY plex.media_grabs.id;


--
-- Name: media_item_settings; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_item_settings (
    id integer NOT NULL,
    account_id integer,
    media_item_id integer,
    settings text,
    created_at bigint,
    updated_at bigint
);


--
-- Name: media_item_settings_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_item_settings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_item_settings_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_item_settings_id_seq OWNED BY plex.media_item_settings.id;


--
-- Name: media_items; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_items (
    id integer NOT NULL,
    library_section_id integer,
    section_location_id integer,
    metadata_item_id integer,
    type_id integer,
    width integer,
    height integer,
    size bigint,
    duration integer,
    bitrate integer,
    container text,
    video_codec text,
    audio_codec text,
    display_aspect_ratio double precision,
    frames_per_second double precision,
    audio_channels integer,
    interlaced integer,
    source text,
    hints text,
    display_offset integer,
    settings text,
    created_at bigint,
    updated_at bigint,
    optimized_for_streaming integer,
    deleted_at bigint,
    media_analysis_version integer DEFAULT 0,
    sample_aspect_ratio double precision,
    extra_data text,
    proxy_type integer,
    channel_id integer,
    begins_at bigint,
    ends_at bigint,
    color_trc text
);


--
-- Name: media_items_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_items_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_items_id_seq OWNED BY plex.media_items.id;


--
-- Name: media_part_settings; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_part_settings (
    id integer NOT NULL,
    account_id integer,
    media_part_id integer,
    selected_audio_stream_id integer,
    selected_subtitle_stream_id integer,
    settings text,
    created_at bigint,
    updated_at bigint,
    changed_at bigint DEFAULT 0
);


--
-- Name: media_part_settings_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_part_settings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_part_settings_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_part_settings_id_seq OWNED BY plex.media_part_settings.id;


--
-- Name: media_parts; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_parts (
    id integer NOT NULL,
    media_item_id integer,
    directory_id integer,
    hash text,
    open_subtitle_hash text,
    file text,
    index integer,
    size bigint,
    duration integer,
    created_at bigint,
    updated_at bigint,
    deleted_at bigint,
    extra_data text
);


--
-- Name: media_parts_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_parts_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_parts_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_parts_id_seq OWNED BY plex.media_parts.id;


--
-- Name: media_provider_resources; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_provider_resources (
    id integer NOT NULL,
    parent_id integer,
    type integer,
    status integer,
    state integer,
    identifier text,
    protocol text,
    uri text,
    uuid text,
    extra_data text,
    last_seen_at bigint,
    created_at bigint,
    updated_at bigint,
    data bytea
);


--
-- Name: media_provider_resources_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_provider_resources_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_provider_resources_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_provider_resources_id_seq OWNED BY plex.media_provider_resources.id;


--
-- Name: media_stream_settings; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_stream_settings (
    id integer NOT NULL,
    account_id integer,
    media_stream_id integer,
    extra_data text,
    created_at bigint,
    updated_at bigint
);


--
-- Name: media_stream_settings_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_stream_settings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_stream_settings_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_stream_settings_id_seq OWNED BY plex.media_stream_settings.id;


--
-- Name: media_streams; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_streams (
    id integer NOT NULL,
    stream_type_id integer,
    media_item_id integer,
    url text,
    codec text,
    language text,
    created_at bigint,
    updated_at bigint,
    index integer,
    media_part_id integer,
    channels integer,
    bitrate integer,
    url_index integer,
    "default" integer DEFAULT 0,
    forced integer DEFAULT 0,
    extra_data text
);


--
-- Name: media_streams_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_streams_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_streams_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_streams_id_seq OWNED BY plex.media_streams.id;


--
-- Name: media_subscriptions; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.media_subscriptions (
    id integer NOT NULL,
    "order" double precision,
    metadata_type integer,
    target_metadata_item_id integer,
    target_library_section_id integer,
    target_section_location_id integer,
    extra_data text,
    created_at bigint,
    updated_at bigint
);


--
-- Name: media_subscriptions_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.media_subscriptions_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: media_subscriptions_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.media_subscriptions_id_seq OWNED BY plex.media_subscriptions.id;


--
-- Name: metadata_agent_provider_group_items; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_agent_provider_group_items (
    id integer NOT NULL,
    metadata_agent_provider_group_id integer NOT NULL,
    metadata_agent_provider_id integer NOT NULL,
    "order" double precision
);


--
-- Name: metadata_agent_provider_group_items_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_agent_provider_group_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_agent_provider_group_items_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_agent_provider_group_items_id_seq OWNED BY plex.metadata_agent_provider_group_items.id;


--
-- Name: metadata_agent_provider_groups; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_agent_provider_groups (
    id integer NOT NULL,
    title text,
    primary_identifier text,
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    extra_data text
);


--
-- Name: metadata_agent_provider_groups_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_agent_provider_groups_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_agent_provider_groups_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_agent_provider_groups_id_seq OWNED BY plex.metadata_agent_provider_groups.id;


--
-- Name: metadata_agent_providers; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_agent_providers (
    id integer NOT NULL,
    identifier text,
    title text,
    uri text,
    agent_type integer,
    metadata_types text,
    online integer,
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    extra_data text
);


--
-- Name: metadata_agent_providers_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_agent_providers_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_agent_providers_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_agent_providers_id_seq OWNED BY plex.metadata_agent_providers.id;


--
-- Name: metadata_item_accounts; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_item_accounts (
    id integer NOT NULL,
    account_id integer,
    metadata_item_id integer
);


--
-- Name: metadata_item_accounts_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_item_accounts_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_item_accounts_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_item_accounts_id_seq OWNED BY plex.metadata_item_accounts.id;


--
-- Name: metadata_item_clusterings; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_item_clusterings (
    id integer NOT NULL,
    metadata_item_id integer,
    metadata_item_cluster_id integer,
    index integer,
    version integer
);


--
-- Name: metadata_item_clusterings_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_item_clusterings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_item_clusterings_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_item_clusterings_id_seq OWNED BY plex.metadata_item_clusterings.id;


--
-- Name: metadata_item_clusters; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_item_clusters (
    id integer NOT NULL,
    zoom_level integer,
    library_section_id integer,
    title text,
    count integer,
    starts_at bigint,
    ends_at bigint,
    extra_data text
);


--
-- Name: metadata_item_clusters_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_item_clusters_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_item_clusters_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_item_clusters_id_seq OWNED BY plex.metadata_item_clusters.id;


--
-- Name: metadata_item_setting_markers; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_item_setting_markers (
    id integer NOT NULL,
    marker_type integer NOT NULL,
    metadata_item_setting_id integer NOT NULL,
    start_time_offset integer NOT NULL,
    end_time_offset integer,
    title text,
    created_at bigint,
    updated_at bigint,
    extra_data text
);


--
-- Name: metadata_item_setting_markers_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_item_setting_markers_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_item_setting_markers_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_item_setting_markers_id_seq OWNED BY plex.metadata_item_setting_markers.id;


--
-- Name: metadata_item_settings; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_item_settings (
    id integer NOT NULL,
    account_id integer,
    guid text,
    rating double precision,
    view_offset integer,
    view_count integer,
    last_viewed_at bigint,
    created_at bigint,
    updated_at bigint,
    skip_count integer DEFAULT 0,
    last_skipped_at bigint,
    changed_at bigint DEFAULT 0,
    extra_data text,
    last_rated_at bigint
);


--
-- Name: metadata_item_settings_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_item_settings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_item_settings_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_item_settings_id_seq OWNED BY plex.metadata_item_settings.id;


--
-- Name: metadata_item_views; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_item_views (
    id integer NOT NULL,
    account_id integer,
    guid text,
    metadata_type integer,
    library_section_id integer,
    grandparent_title text,
    parent_index integer,
    parent_title text,
    index integer,
    title text,
    thumb_url text,
    viewed_at bigint,
    grandparent_guid text,
    originally_available_at bigint,
    device_id integer,
    view_type integer DEFAULT 0
);


--
-- Name: metadata_item_views_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_item_views_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_item_views_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_item_views_id_seq OWNED BY plex.metadata_item_views.id;


--
-- Name: metadata_items_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_items_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_items_id_seq OWNED BY plex.metadata_items.id;




--
-- Name: metadata_relations; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_relations (
    id integer NOT NULL,
    metadata_item_id integer,
    related_metadata_item_id integer,
    relation_type integer,
    created_at bigint,
    updated_at bigint
);


--
-- Name: metadata_relations_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.metadata_relations_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: metadata_relations_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.metadata_relations_id_seq OWNED BY plex.metadata_relations.id;


--
-- Name: metadata_subscription_desired_items; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.metadata_subscription_desired_items (
    sub_id integer,
    remote_id text
);


--
-- Name: play_queue_generators; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.play_queue_generators (
    id integer NOT NULL,
    playlist_id integer,
    metadata_item_id integer,
    uri text,
    "limit" integer,
    continuous integer,
    "order" double precision,
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    changed_at bigint DEFAULT 0,
    recursive integer,
    type integer,
    extra_data text
);


--
-- Name: play_queue_generators_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.play_queue_generators_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: play_queue_generators_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.play_queue_generators_id_seq OWNED BY plex.play_queue_generators.id;


--
-- Name: play_queue_items; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.play_queue_items (
    id integer NOT NULL,
    play_queue_id integer,
    metadata_item_id integer,
    "order" double precision,
    up_next integer,
    play_queue_generator_id integer
);


--
-- Name: play_queue_items_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.play_queue_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: play_queue_items_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.play_queue_items_id_seq OWNED BY plex.play_queue_items.id;


--
-- Name: play_queues; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.play_queues (
    id integer NOT NULL,
    client_identifier text,
    account_id integer,
    playlist_id integer,
    sync_item_id integer,
    play_queue_generator_id integer,
    generator_start_index integer,
    generator_end_index integer,
    generator_items_count integer,
    generator_ids bytea,
    seed integer,
    current_play_queue_item_id integer,
    last_added_play_queue_item_id integer,
    version integer,
    created_at bigint,
    updated_at bigint,
    metadata_type integer,
    total_items_count integer,
    generator_generator_ids bytea,
    extra_data text
);


--
-- Name: play_queues_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.play_queues_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: play_queues_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.play_queues_id_seq OWNED BY plex.play_queues.id;


--
-- Name: plugin_prefixes; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.plugin_prefixes (
    id integer NOT NULL,
    plugin_id integer,
    name text,
    prefix text,
    art_url text,
    thumb_url text,
    titlebar_url text,
    share integer,
    has_store_services integer,
    prefs integer
);


--
-- Name: plugin_prefixes_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.plugin_prefixes_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: plugin_prefixes_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.plugin_prefixes_id_seq OWNED BY plex.plugin_prefixes.id;


--
-- Name: plugins; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.plugins (
    id integer NOT NULL,
    identifier text,
    framework_version integer,
    access_count integer,
    installed_at bigint,
    accessed_at bigint,
    modified_at bigint
);


--
-- Name: plugins_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.plugins_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: plugins_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.plugins_id_seq OWNED BY plex.plugins.id;


--
-- Name: preferences; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.preferences (
    id integer NOT NULL,
    name text,
    value text
);


--
-- Name: preferences_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.preferences_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: preferences_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.preferences_id_seq OWNED BY plex.preferences.id;


--
-- Name: remote_id_translation; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.remote_id_translation (
    id integer NOT NULL,
    type integer,
    local_id integer,
    remote_id text
);


--
-- Name: remote_id_translation_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.remote_id_translation_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: remote_id_translation_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.remote_id_translation_id_seq OWNED BY plex.remote_id_translation.id;


--
-- Name: schema_migrations; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.schema_migrations (
    version text NOT NULL,
    rollback_sql text,
    optimize_on_rollback integer,
    min_version text,
    id serial NOT NULL
);


--
-- Name: section_locations; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.section_locations (
    id integer NOT NULL,
    library_section_id integer,
    root_path text,
    available integer DEFAULT 1,
    scanned_at bigint,
    created_at bigint,
    updated_at bigint
);


--
-- Name: section_locations_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.section_locations_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: section_locations_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.section_locations_id_seq OWNED BY plex.section_locations.id;




--
-- Name: statistics_bandwidth; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.statistics_bandwidth (
    id integer NOT NULL,
    account_id integer,
    device_id integer,
    timespan integer,
    at bigint,
    lan integer,
    bytes bigint
);


--
-- Name: statistics_bandwidth_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.statistics_bandwidth_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: statistics_bandwidth_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.statistics_bandwidth_id_seq OWNED BY plex.statistics_bandwidth.id;


--
-- Name: statistics_media; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.statistics_media (
    id integer NOT NULL,
    account_id integer,
    device_id integer,
    timespan integer,
    at bigint,
    metadata_type integer,
    count integer,
    duration integer
);


--
-- Name: statistics_media_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.statistics_media_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: statistics_media_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.statistics_media_id_seq OWNED BY plex.statistics_media.id;


--
-- Name: statistics_resources; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.statistics_resources (
    id integer NOT NULL,
    timespan integer,
    at bigint,
    host_cpu_utilization double precision,
    process_cpu_utilization double precision,
    host_memory_utilization double precision,
    process_memory_utilization double precision
);


--
-- Name: statistics_resources_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.statistics_resources_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: statistics_resources_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.statistics_resources_id_seq OWNED BY plex.statistics_resources.id;


--
-- Name: taggings; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.taggings (
    id integer NOT NULL,
    metadata_item_id integer,
    tag_id integer,
    index integer,
    text text,
    time_offset integer,
    end_time_offset integer,
    thumb_url text,
    created_at bigint,
    extra_data text
);
ALTER TABLE ONLY plex.taggings ALTER COLUMN metadata_item_id SET STATISTICS 200;
ALTER TABLE ONLY plex.taggings ALTER COLUMN tag_id SET STATISTICS 200;


--
-- Name: taggings_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.taggings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: taggings_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.taggings_id_seq OWNED BY plex.taggings.id;


--
-- Name: tags_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.tags_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: tags_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.tags_id_seq OWNED BY plex.tags.id;




--
-- Name: versioned_metadata_items; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.versioned_metadata_items (
    id integer NOT NULL,
    metadata_item_id integer,
    generator_id integer,
    target_tag_id integer,
    state integer,
    state_context integer,
    selected_media_id integer,
    version_media_id integer,
    media_decision integer,
    file_size bigint
);


--
-- Name: versioned_metadata_items_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.versioned_metadata_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: versioned_metadata_items_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.versioned_metadata_items_id_seq OWNED BY plex.versioned_metadata_items.id;


--
-- Name: view_settings; Type: TABLE; Schema: plex; Owner: -
--

CREATE TABLE plex.view_settings (
    id integer NOT NULL,
    account_id integer,
    client_type text,
    view_group text,
    view_id integer,
    sort_id integer,
    sort_asc integer,
    created_at bigint,
    updated_at bigint
);


--
-- Name: view_settings_id_seq; Type: SEQUENCE; Schema: plex; Owner: -
--

CREATE SEQUENCE plex.view_settings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: view_settings_id_seq; Type: SEQUENCE OWNED BY; Schema: plex; Owner: -
--

ALTER SEQUENCE plex.view_settings_id_seq OWNED BY plex.view_settings.id;


--
-- Name: accounts id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.accounts ALTER COLUMN id SET DEFAULT nextval('plex.accounts_id_seq'::regclass);


--
-- Name: activities id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.activities ALTER COLUMN id SET DEFAULT nextval('plex.activities_id_seq'::regclass);


--
-- Name: blobs id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.blobs ALTER COLUMN id SET DEFAULT nextval('plex.blobs_id_seq'::regclass);


--
-- Name: custom_channels id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.custom_channels ALTER COLUMN id SET DEFAULT nextval('plex.custom_channels_id_seq'::regclass);


--
-- Name: devices id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.devices ALTER COLUMN id SET DEFAULT nextval('plex.devices_id_seq'::regclass);


--
-- Name: directories id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.directories ALTER COLUMN id SET DEFAULT nextval('plex.directories_id_seq'::regclass);


--
-- Name: download_queue_items id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.download_queue_items ALTER COLUMN id SET DEFAULT nextval('plex.download_queue_items_id_seq'::regclass);


--
-- Name: download_queues id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.download_queues ALTER COLUMN id SET DEFAULT nextval('plex.download_queues_id_seq'::regclass);


--
-- Name: external_metadata_sources id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.external_metadata_sources ALTER COLUMN id SET DEFAULT nextval('plex.external_metadata_sources_id_seq'::regclass);


--
-- Name: hub_templates id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.hub_templates ALTER COLUMN id SET DEFAULT nextval('plex.hub_templates_id_seq'::regclass);


--
-- Name: library_section_permissions id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.library_section_permissions ALTER COLUMN id SET DEFAULT nextval('plex.library_section_permissions_id_seq'::regclass);


--
-- Name: library_sections id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.library_sections ALTER COLUMN id SET DEFAULT nextval('plex.library_sections_id_seq'::regclass);


--
-- Name: locatables id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.locatables ALTER COLUMN id SET DEFAULT nextval('plex.locatables_id_seq'::regclass);


--
-- Name: location_places id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.location_places ALTER COLUMN id SET DEFAULT nextval('plex.location_places_id_seq'::regclass);


--
-- Name: locations id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.locations ALTER COLUMN id SET DEFAULT nextval('plex.locations_id_seq'::regclass);


--
-- Name: media_grabs id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_grabs ALTER COLUMN id SET DEFAULT nextval('plex.media_grabs_id_seq'::regclass);


--
-- Name: media_item_settings id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_item_settings ALTER COLUMN id SET DEFAULT nextval('plex.media_item_settings_id_seq'::regclass);


--
-- Name: media_items id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_items ALTER COLUMN id SET DEFAULT nextval('plex.media_items_id_seq'::regclass);


--
-- Name: media_part_settings id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_part_settings ALTER COLUMN id SET DEFAULT nextval('plex.media_part_settings_id_seq'::regclass);


--
-- Name: media_parts id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_parts ALTER COLUMN id SET DEFAULT nextval('plex.media_parts_id_seq'::regclass);


--
-- Name: media_provider_resources id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_provider_resources ALTER COLUMN id SET DEFAULT nextval('plex.media_provider_resources_id_seq'::regclass);


--
-- Name: media_stream_settings id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_stream_settings ALTER COLUMN id SET DEFAULT nextval('plex.media_stream_settings_id_seq'::regclass);


--
-- Name: media_streams id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_streams ALTER COLUMN id SET DEFAULT nextval('plex.media_streams_id_seq'::regclass);


--
-- Name: media_subscriptions id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_subscriptions ALTER COLUMN id SET DEFAULT nextval('plex.media_subscriptions_id_seq'::regclass);


--
-- Name: metadata_agent_provider_group_items id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_agent_provider_group_items ALTER COLUMN id SET DEFAULT nextval('plex.metadata_agent_provider_group_items_id_seq'::regclass);


--
-- Name: metadata_agent_provider_groups id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_agent_provider_groups ALTER COLUMN id SET DEFAULT nextval('plex.metadata_agent_provider_groups_id_seq'::regclass);


--
-- Name: metadata_agent_providers id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_agent_providers ALTER COLUMN id SET DEFAULT nextval('plex.metadata_agent_providers_id_seq'::regclass);


--
-- Name: metadata_item_accounts id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_accounts ALTER COLUMN id SET DEFAULT nextval('plex.metadata_item_accounts_id_seq'::regclass);


--
-- Name: metadata_item_clusterings id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_clusterings ALTER COLUMN id SET DEFAULT nextval('plex.metadata_item_clusterings_id_seq'::regclass);


--
-- Name: metadata_item_clusters id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_clusters ALTER COLUMN id SET DEFAULT nextval('plex.metadata_item_clusters_id_seq'::regclass);


--
-- Name: metadata_item_setting_markers id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_setting_markers ALTER COLUMN id SET DEFAULT nextval('plex.metadata_item_setting_markers_id_seq'::regclass);


--
-- Name: metadata_item_settings id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_settings ALTER COLUMN id SET DEFAULT nextval('plex.metadata_item_settings_id_seq'::regclass);


--
-- Name: metadata_item_views id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_views ALTER COLUMN id SET DEFAULT nextval('plex.metadata_item_views_id_seq'::regclass);


--
-- Name: metadata_items id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_items ALTER COLUMN id SET DEFAULT nextval('plex.metadata_items_id_seq'::regclass);


--
-- Name: metadata_relations id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_relations ALTER COLUMN id SET DEFAULT nextval('plex.metadata_relations_id_seq'::regclass);


--
-- Name: play_queue_generators id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.play_queue_generators ALTER COLUMN id SET DEFAULT nextval('plex.play_queue_generators_id_seq'::regclass);


--
-- Name: play_queue_items id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.play_queue_items ALTER COLUMN id SET DEFAULT nextval('plex.play_queue_items_id_seq'::regclass);


--
-- Name: play_queues id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.play_queues ALTER COLUMN id SET DEFAULT nextval('plex.play_queues_id_seq'::regclass);


--
-- Name: plugin_prefixes id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.plugin_prefixes ALTER COLUMN id SET DEFAULT nextval('plex.plugin_prefixes_id_seq'::regclass);


--
-- Name: plugins id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.plugins ALTER COLUMN id SET DEFAULT nextval('plex.plugins_id_seq'::regclass);


--
-- Name: preferences id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.preferences ALTER COLUMN id SET DEFAULT nextval('plex.preferences_id_seq'::regclass);


--
-- Name: remote_id_translation id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.remote_id_translation ALTER COLUMN id SET DEFAULT nextval('plex.remote_id_translation_id_seq'::regclass);


--
-- Name: section_locations id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.section_locations ALTER COLUMN id SET DEFAULT nextval('plex.section_locations_id_seq'::regclass);


--
-- Name: statistics_bandwidth id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.statistics_bandwidth ALTER COLUMN id SET DEFAULT nextval('plex.statistics_bandwidth_id_seq'::regclass);


--
-- Name: statistics_media id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.statistics_media ALTER COLUMN id SET DEFAULT nextval('plex.statistics_media_id_seq'::regclass);


--
-- Name: statistics_resources id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.statistics_resources ALTER COLUMN id SET DEFAULT nextval('plex.statistics_resources_id_seq'::regclass);


--
-- Name: taggings id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.taggings ALTER COLUMN id SET DEFAULT nextval('plex.taggings_id_seq'::regclass);


--
-- Name: tags id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.tags ALTER COLUMN id SET DEFAULT nextval('plex.tags_id_seq'::regclass);


--
-- Name: versioned_metadata_items id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.versioned_metadata_items ALTER COLUMN id SET DEFAULT nextval('plex.versioned_metadata_items_id_seq'::regclass);


--
-- Name: view_settings id; Type: DEFAULT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.view_settings ALTER COLUMN id SET DEFAULT nextval('plex.view_settings_id_seq'::regclass);


--
-- Name: accounts accounts_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.accounts
    ADD CONSTRAINT accounts_pkey PRIMARY KEY (id);


--
-- Name: activities activities_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.activities
    ADD CONSTRAINT activities_pkey PRIMARY KEY (id);


--
-- Name: blobs blobs_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.blobs
    ADD CONSTRAINT blobs_pkey PRIMARY KEY (id);


--
-- Name: custom_channels custom_channels_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.custom_channels
    ADD CONSTRAINT custom_channels_pkey PRIMARY KEY (id);


--
-- Name: devices devices_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.devices
    ADD CONSTRAINT devices_pkey PRIMARY KEY (id);


--
-- Name: directories directories_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.directories
    ADD CONSTRAINT directories_pkey PRIMARY KEY (id);


--
-- Name: download_queue_items download_queue_items_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.download_queue_items
    ADD CONSTRAINT download_queue_items_pkey PRIMARY KEY (id);


--
-- Name: download_queues download_queues_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.download_queues
    ADD CONSTRAINT download_queues_pkey PRIMARY KEY (id);


--
-- Name: external_metadata_sources external_metadata_sources_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.external_metadata_sources
    ADD CONSTRAINT external_metadata_sources_pkey PRIMARY KEY (id);


--
-- Name: hub_templates hub_templates_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.hub_templates
    ADD CONSTRAINT hub_templates_pkey PRIMARY KEY (id);


--
-- Name: library_section_permissions library_section_permissions_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.library_section_permissions
    ADD CONSTRAINT library_section_permissions_pkey PRIMARY KEY (id);


--
-- Name: library_sections library_sections_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.library_sections
    ADD CONSTRAINT library_sections_pkey PRIMARY KEY (id);


--
-- Name: locatables locatables_location_id_locatable_id_locatable_type_key; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.locatables
    ADD CONSTRAINT locatables_location_id_locatable_id_locatable_type_key UNIQUE (location_id, locatable_id, locatable_type);


--
-- Name: locatables locatables_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.locatables
    ADD CONSTRAINT locatables_pkey PRIMARY KEY (id);


--
-- Name: location_places location_places_location_id_guid_key; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.location_places
    ADD CONSTRAINT location_places_location_id_guid_key UNIQUE (location_id, guid);


--
-- Name: location_places location_places_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.location_places
    ADD CONSTRAINT location_places_pkey PRIMARY KEY (id);


--
-- Name: locations locations_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.locations
    ADD CONSTRAINT locations_pkey PRIMARY KEY (id);


--
-- Name: maintenance_control maintenance_control_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.maintenance_control
    ADD CONSTRAINT maintenance_control_pkey PRIMARY KEY (table_name);


--
-- Name: media_grabs media_grabs_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_grabs
    ADD CONSTRAINT media_grabs_pkey PRIMARY KEY (id);


--
-- Name: media_item_settings media_item_settings_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_item_settings
    ADD CONSTRAINT media_item_settings_pkey PRIMARY KEY (id);


--
-- Name: media_items media_items_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_items
    ADD CONSTRAINT media_items_pkey PRIMARY KEY (id);


--
-- Name: media_part_settings media_part_settings_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_part_settings
    ADD CONSTRAINT media_part_settings_pkey PRIMARY KEY (id);


--
-- Name: media_parts media_parts_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_parts
    ADD CONSTRAINT media_parts_pkey PRIMARY KEY (id);


--
-- Name: media_provider_resources media_provider_resources_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_provider_resources
    ADD CONSTRAINT media_provider_resources_pkey PRIMARY KEY (id);


--
-- Name: media_stream_settings media_stream_settings_media_stream_id_account_id_key; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_stream_settings
    ADD CONSTRAINT media_stream_settings_media_stream_id_account_id_key UNIQUE (media_stream_id, account_id);


--
-- Name: media_stream_settings media_stream_settings_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_stream_settings
    ADD CONSTRAINT media_stream_settings_pkey PRIMARY KEY (id);


--
-- Name: media_streams media_streams_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_streams
    ADD CONSTRAINT media_streams_pkey PRIMARY KEY (id);


--
-- Name: media_subscriptions media_subscriptions_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.media_subscriptions
    ADD CONSTRAINT media_subscriptions_pkey PRIMARY KEY (id);


--
-- Name: metadata_agent_provider_group_items metadata_agent_provider_group_items_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_agent_provider_group_items
    ADD CONSTRAINT metadata_agent_provider_group_items_pkey PRIMARY KEY (id);


--
-- Name: metadata_agent_provider_groups metadata_agent_provider_groups_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_agent_provider_groups
    ADD CONSTRAINT metadata_agent_provider_groups_pkey PRIMARY KEY (id);


--
-- Name: metadata_agent_providers metadata_agent_providers_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_agent_providers
    ADD CONSTRAINT metadata_agent_providers_pkey PRIMARY KEY (id);


--
-- Name: metadata_item_accounts metadata_item_accounts_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_accounts
    ADD CONSTRAINT metadata_item_accounts_pkey PRIMARY KEY (id);


--
-- Name: metadata_item_clusterings metadata_item_clusterings_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_clusterings
    ADD CONSTRAINT metadata_item_clusterings_pkey PRIMARY KEY (id);


--
-- Name: metadata_item_clusters metadata_item_clusters_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_clusters
    ADD CONSTRAINT metadata_item_clusters_pkey PRIMARY KEY (id);


--
-- Name: metadata_item_setting_markers metadata_item_setting_markers_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_setting_markers
    ADD CONSTRAINT metadata_item_setting_markers_pkey PRIMARY KEY (id);


--
-- Name: metadata_item_settings metadata_item_settings_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_settings
    ADD CONSTRAINT metadata_item_settings_pkey PRIMARY KEY (id);


--
-- Name: metadata_item_views metadata_item_views_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_item_views
    ADD CONSTRAINT metadata_item_views_pkey PRIMARY KEY (id);


--
-- Name: metadata_items metadata_items_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_items
    ADD CONSTRAINT metadata_items_pkey PRIMARY KEY (id);


--
-- Name: metadata_items chk_not_orphan; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE plex.metadata_items
    ADD CONSTRAINT chk_not_orphan CHECK (metadata_type IS NOT NULL OR library_section_id IS NOT NULL);


--
-- Name: metadata_relations metadata_relations_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.metadata_relations
    ADD CONSTRAINT metadata_relations_pkey PRIMARY KEY (id);


--
-- Name: play_queue_generators play_queue_generators_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.play_queue_generators
    ADD CONSTRAINT play_queue_generators_pkey PRIMARY KEY (id);


--
-- Name: play_queue_items play_queue_items_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.play_queue_items
    ADD CONSTRAINT play_queue_items_pkey PRIMARY KEY (id);


--
-- Name: play_queues play_queues_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.play_queues
    ADD CONSTRAINT play_queues_pkey PRIMARY KEY (id);


--
-- Name: plugin_prefixes plugin_prefixes_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.plugin_prefixes
    ADD CONSTRAINT plugin_prefixes_pkey PRIMARY KEY (id);


--
-- Name: plugins plugins_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.plugins
    ADD CONSTRAINT plugins_pkey PRIMARY KEY (id);


--
-- Name: preferences preferences_name_key; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.preferences
    ADD CONSTRAINT preferences_name_key UNIQUE (name);


--
-- Name: preferences preferences_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.preferences
    ADD CONSTRAINT preferences_pkey PRIMARY KEY (id);


--
-- Name: remote_id_translation remote_id_translation_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.remote_id_translation
    ADD CONSTRAINT remote_id_translation_pkey PRIMARY KEY (id);


--
-- Name: schema_migrations schema_migrations_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.schema_migrations
    ADD CONSTRAINT schema_migrations_pkey PRIMARY KEY (version);


--
-- Name: section_locations section_locations_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.section_locations
    ADD CONSTRAINT section_locations_pkey PRIMARY KEY (id);


--
-- Name: sqlite_column_types sqlite_column_types_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.sqlite_column_types
    ADD CONSTRAINT sqlite_column_types_pkey PRIMARY KEY (table_name, column_name);


--
-- Name: statistics_bandwidth statistics_bandwidth_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.statistics_bandwidth
    ADD CONSTRAINT statistics_bandwidth_pkey PRIMARY KEY (id);


--
-- Name: statistics_media statistics_media_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.statistics_media
    ADD CONSTRAINT statistics_media_pkey PRIMARY KEY (id);


--
-- Name: statistics_resources statistics_resources_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.statistics_resources
    ADD CONSTRAINT statistics_resources_pkey PRIMARY KEY (id);


--
-- Name: taggings taggings_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.taggings
    ADD CONSTRAINT taggings_pkey PRIMARY KEY (id);


--
-- Name: tags tags_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.tags
    ADD CONSTRAINT tags_pkey PRIMARY KEY (id);


--
-- Name: test test_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.test
    ADD CONSTRAINT test_pkey PRIMARY KEY (id);


--
-- Name: versioned_metadata_items versioned_metadata_items_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.versioned_metadata_items
    ADD CONSTRAINT versioned_metadata_items_pkey PRIMARY KEY (id);


--
-- Name: view_settings view_settings_pkey; Type: CONSTRAINT; Schema: plex; Owner: -
--

ALTER TABLE ONLY plex.view_settings
    ADD CONSTRAINT view_settings_pkey PRIMARY KEY (id);


--
-- Name: idx_activities_parent_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_activities_parent_id ON plex.activities USING btree (parent_id);


--
-- Name: idx_activities_started_at; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_activities_started_at ON plex.activities USING btree (started_at);


--
-- Name: idx_activities_type; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_activities_type ON plex.activities USING btree (type);


--
-- Name: idx_blobs_linked; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_blobs_linked ON plex.blobs USING btree (linked_type, linked_id);


--
-- Name: idx_blobs_linked_type_id_blob_type; Type: INDEX; Schema: plex; Owner: -
--

CREATE UNIQUE INDEX idx_blobs_linked_type_id_blob_type ON plex.blobs USING btree (linked_type, linked_id, blob_type);

-- Genuine unique indexes from Plex SQLite, including separately declared indexes.
CREATE UNIQUE INDEX idx_play_queues_client_account_type ON plex.play_queues USING btree (client_identifier, account_id, metadata_type);
CREATE UNIQUE INDEX idx_media_provider_resources_uuid ON plex.media_provider_resources USING btree (uuid);
CREATE UNIQUE INDEX idx_media_grabs_uuid ON plex.media_grabs USING btree (uuid);
CREATE UNIQUE INDEX idx_external_metadata_sources_uri ON plex.external_metadata_sources USING btree (uri);
CREATE UNIQUE INDEX idx_blobs_linked_type_guid_blob_type ON plex.blobs USING btree (linked_type, linked_guid, blob_type);
CREATE UNIQUE INDEX idx_metadata_agent_providers_identifier ON plex.metadata_agent_providers USING btree (identifier);
CREATE UNIQUE INDEX idx_metadata_agent_provider_group_items_pair ON plex.metadata_agent_provider_group_items USING btree (metadata_agent_provider_group_id, metadata_agent_provider_id);


--
-- Name: idx_devices_identifier; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_devices_identifier ON plex.devices USING btree (identifier);


--
-- Name: idx_directories_library_section; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_directories_library_section ON plex.directories USING btree (library_section_id);


--
-- Name: idx_directories_library_section_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_directories_library_section_id ON plex.directories USING btree (library_section_id);


--
-- Name: idx_directories_parent_directory_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_directories_parent_directory_id ON plex.directories USING btree (parent_directory_id);


--
-- Name: idx_directories_parent_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_directories_parent_id ON plex.directories USING btree (parent_directory_id);


--
-- Name: idx_directories_path; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_directories_path ON plex.directories USING btree (path);


--
-- Name: idx_emi_source_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_emi_source_id ON plex.external_metadata_items USING btree (external_metadata_source_id);


--
-- Name: idx_library_sections_uuid; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_library_sections_uuid ON plex.library_sections USING btree (uuid);


--
-- Name: idx_locations_lat; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_locations_lat ON plex.locations USING btree (lat_min, lat_max);


--
-- Name: idx_locations_lon; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_locations_lon ON plex.locations USING btree (lon_min, lon_max);


--
-- Name: idx_media_item_settings_account_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_item_settings_account_id ON plex.media_item_settings USING btree (account_id);


--
-- Name: idx_media_item_settings_composite; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_item_settings_composite ON plex.media_item_settings USING btree (media_item_id, account_id);


--
-- Name: idx_media_item_settings_media_item_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_item_settings_media_item_id ON plex.media_item_settings USING btree (media_item_id);


--
-- Name: idx_media_items_deleted_at; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_items_deleted_at ON plex.media_items USING btree (deleted_at);


--
-- Name: idx_media_items_library_section; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_items_library_section ON plex.media_items USING btree (library_section_id);


--
-- Name: idx_media_items_library_section_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_items_library_section_id ON plex.media_items USING btree (library_section_id);


--
-- Name: idx_media_items_metadata_item_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_items_metadata_item_id ON plex.media_items USING btree (metadata_item_id);


--
-- Name: idx_media_items_section_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_items_section_id ON plex.media_items USING btree (library_section_id);


--
-- Name: idx_media_part_settings_account_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_part_settings_account_id ON plex.media_part_settings USING btree (account_id);


--
-- Name: idx_media_part_settings_composite; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_part_settings_composite ON plex.media_part_settings USING btree (media_part_id, account_id);


--
-- Name: idx_media_part_settings_media_part_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_part_settings_media_part_id ON plex.media_part_settings USING btree (media_part_id);


--
-- Name: idx_media_parts_deleted_at; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_parts_deleted_at ON plex.media_parts USING btree (deleted_at);


--
-- Name: idx_media_parts_directory_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_parts_directory_id ON plex.media_parts USING btree (directory_id);


--
-- Name: idx_media_parts_file; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_parts_file ON plex.media_parts USING btree (file);


--
-- Name: idx_media_parts_hash; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_parts_hash ON plex.media_parts USING btree (hash);


--
-- Name: idx_media_parts_media_item_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_parts_media_item_id ON plex.media_parts USING btree (media_item_id);


--
-- Name: idx_media_streams_media_item_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_streams_media_item_id ON plex.media_streams USING btree (media_item_id);


--
-- Name: idx_media_streams_media_part_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_streams_media_part_id ON plex.media_streams USING btree (media_part_id);


--
-- Name: idx_media_streams_stream_type; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_media_streams_stream_type ON plex.media_streams USING btree (stream_type_id);


--
-- Name: idx_metadata_item_settings_account_guid; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_item_settings_account_guid ON plex.metadata_item_settings USING btree (account_id, guid);


--
-- Name: idx_metadata_item_settings_account_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_item_settings_account_id ON plex.metadata_item_settings USING btree (account_id);


--
-- Name: idx_metadata_item_settings_guid; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_item_settings_guid ON plex.metadata_item_settings USING btree (guid);


--
-- Name: idx_metadata_item_settings_last_viewed; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_item_settings_last_viewed ON plex.metadata_item_settings USING btree (last_viewed_at DESC NULLS LAST) WHERE (last_viewed_at IS NOT NULL);


--
-- Name: idx_metadata_item_views_account_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_item_views_account_id ON plex.metadata_item_views USING btree (account_id);


--
-- Name: idx_metadata_item_views_guid; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_item_views_guid ON plex.metadata_item_views USING btree (guid);


--
-- Name: idx_metadata_items_absolute_index; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_absolute_index ON plex.metadata_items USING btree (absolute_index);


--
-- Name: idx_metadata_items_added_at; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_added_at ON plex.metadata_items USING btree (added_at);


--
-- Name: idx_metadata_items_changed_at; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_changed_at ON plex.metadata_items USING btree (changed_at DESC);


--
-- Name: idx_metadata_items_fts; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_fts ON plex.metadata_items USING gin (title_fts);


--
-- Name: idx_metadata_items_guid; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_guid ON plex.metadata_items USING btree (guid);


--
-- Name: idx_metadata_items_hash; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_hash ON plex.metadata_items USING btree (hash);


--
-- Name: idx_metadata_items_id_section; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_id_section ON plex.metadata_items USING btree (id, library_section_id);


--
-- Name: idx_metadata_items_index; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_index ON plex.metadata_items USING btree (index);


--
-- Name: idx_metadata_items_library_section; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_library_section ON plex.metadata_items USING btree (library_section_id, metadata_type);


--
-- Name: idx_metadata_items_library_section_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_library_section_id ON plex.metadata_items USING btree (library_section_id);


--
-- Name: idx_metadata_items_metadata_type; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_metadata_type ON plex.metadata_items USING btree (metadata_type);


--
-- Name: idx_metadata_items_originally_available_at; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_originally_available_at ON plex.metadata_items USING btree (originally_available_at);


--
-- Name: idx_metadata_items_parent_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_parent_id ON plex.metadata_items USING btree (parent_id);


--
-- Name: idx_metadata_items_parent_type; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_parent_type ON plex.metadata_items USING btree (parent_id, metadata_type);


--
-- Name: idx_metadata_items_search; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_search ON plex.metadata_items USING gin (search_vector);


--
-- Name: idx_metadata_items_section_type; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_section_type ON plex.metadata_items USING btree (library_section_id, metadata_type);


--
-- Name: idx_metadata_items_section_type_added; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_section_type_added ON plex.metadata_items USING btree (library_section_id, metadata_type, added_at DESC);


--
-- Name: idx_metadata_items_section_type_sort; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_section_type_sort ON plex.metadata_items USING btree (library_section_id, metadata_type, title_sort);


--
-- Name: idx_metadata_items_title; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_title ON plex.metadata_items USING btree (title);


--
-- Name: idx_metadata_items_title_lower; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_title_lower ON plex.metadata_items USING btree (lower((title)::text));


--
-- Name: idx_metadata_items_title_sort; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_title_sort ON plex.metadata_items USING btree (title_sort);


--
-- Name: idx_metadata_items_title_trgm; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_title_trgm ON plex.metadata_items USING gin (title plex.gin_trgm_ops);


--
-- Name: idx_metadata_items_type; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_items_type ON plex.metadata_items USING btree (metadata_type);


--
-- Name: idx_metadata_relations_metadata_item_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_relations_metadata_item_id ON plex.metadata_relations USING btree (metadata_item_id);


--
-- Name: idx_metadata_relations_related; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_relations_related ON plex.metadata_relations USING btree (related_metadata_item_id);


--
-- Name: idx_metadata_relations_related_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_metadata_relations_related_id ON plex.metadata_relations USING btree (related_metadata_item_id);


--
-- Name: idx_mia_account_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_mia_account_id ON plex.metadata_item_accounts USING btree (account_id);


--
-- Name: idx_mia_metadata_item_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_mia_metadata_item_id ON plex.metadata_item_accounts USING btree (metadata_item_id);


--
-- Name: idx_mis_markers_setting_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_mis_markers_setting_id ON plex.metadata_item_setting_markers USING btree (metadata_item_setting_id);


--
-- Name: idx_play_queue_items_queue_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_play_queue_items_queue_id ON plex.play_queue_items USING btree (play_queue_id);


--
-- Name: idx_plugins_identifier; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_plugins_identifier ON plex.plugins USING btree (identifier);


--
-- Name: idx_preferences_name; Type: INDEX; Schema: plex; Owner: -
--

CREATE UNIQUE INDEX idx_preferences_name ON plex.preferences USING btree (name);


--
-- Name: idx_section_locations_library_section_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_section_locations_library_section_id ON plex.section_locations USING btree (library_section_id);


--
-- Name: idx_statistics_media_at; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_statistics_media_at ON plex.statistics_media USING btree (at);


--
-- Name: idx_statistics_media_lookup; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_statistics_media_lookup ON plex.statistics_media USING btree (account_id, device_id, timespan, at, metadata_type);


--
-- Name: idx_statistics_media_timespan; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_statistics_media_timespan ON plex.statistics_media USING btree (timespan);


--
-- Name: idx_statistics_resources_at; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_statistics_resources_at ON plex.statistics_resources USING btree (at);


--
-- Name: idx_taggings_item_tag; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_taggings_item_tag ON plex.taggings USING btree (metadata_item_id, tag_id);


--
-- Name: idx_taggings_metadata_item_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_taggings_metadata_item_id ON plex.taggings USING btree (metadata_item_id);


--
-- Name: idx_taggings_tag_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_taggings_tag_id ON plex.taggings USING btree (tag_id);


--
-- Name: idx_taggings_tag_metadata; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_taggings_tag_metadata ON plex.taggings USING btree (tag_id, metadata_item_id);


--
-- Name: idx_tags_fts; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_tags_fts ON plex.tags USING gin (search_vector);


--
-- Name: idx_tags_key; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_tags_key ON plex.tags USING btree (key);


--
-- Name: idx_tags_metadata_item_id; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_tags_metadata_item_id ON plex.tags USING btree (metadata_item_id);


--
-- Name: idx_tags_tag; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_tags_tag ON plex.tags USING btree (tag);


--
-- Name: idx_tags_tag_type; Type: INDEX; Schema: plex; Owner: -
--

CREATE INDEX idx_tags_tag_type ON plex.tags USING btree (tag_type);


--
-- Name: metadata_items check_cross_section_parent; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER check_cross_section_parent BEFORE INSERT OR UPDATE OF parent_id, library_section_id ON plex.metadata_items FOR EACH ROW WHEN ((new.parent_id IS NOT NULL)) EXECUTE FUNCTION plex.prevent_cross_section_parent();


--
-- Name: metadata_items metadata_items_search_update; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER metadata_items_search_update BEFORE INSERT OR UPDATE ON plex.metadata_items FOR EACH ROW EXECUTE FUNCTION plex.metadata_items_search_trigger();


--
-- Name: metadata_items metadata_items_set_available_at; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER metadata_items_set_available_at BEFORE INSERT ON plex.metadata_items FOR EACH ROW EXECUTE FUNCTION plex.set_available_at();


--
-- Name: metadata_items prevent_self_ref_parent; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER prevent_self_ref_parent BEFORE INSERT OR UPDATE ON plex.metadata_items FOR EACH ROW EXECUTE FUNCTION plex.prevent_self_referential_parent();


--
-- Name: statistics_media statistics_media_reject_empty; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER statistics_media_reject_empty BEFORE INSERT ON plex.statistics_media FOR EACH ROW EXECUTE FUNCTION plex.reject_empty_statistics();


--
-- Name: tags tags_search_update; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER tags_search_update BEFORE INSERT OR UPDATE ON plex.tags FOR EACH ROW EXECUTE FUNCTION plex.tags_search_trigger();


--
-- Name: statistics_resources trg_clean_statistics_resources; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER trg_clean_statistics_resources AFTER INSERT ON plex.statistics_resources FOR EACH STATEMENT EXECUTE FUNCTION plex.maybe_cleanup_statistics();


--
-- Name: metadata_items trg_fix_orphan_season; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER trg_fix_orphan_season AFTER INSERT ON plex.metadata_items FOR EACH ROW EXECUTE FUNCTION plex.fix_orphan_season_on_episode_insert();


--
-- Name: media_parts trg_fix_orphan_season_media; Type: TRIGGER; Schema: plex; Owner: -
--

CREATE TRIGGER trg_fix_orphan_season_media AFTER INSERT ON plex.media_parts FOR EACH ROW EXECUTE FUNCTION plex.fix_orphan_season_on_media_part_insert();


--
-- PostgreSQL database dump complete
--

\unrestrict k1FTTexA8Xe4WPWhJLvkzFfK77e8SGscLqfdbjfaYwEAOAK1wzzj90sahsatJMU


-- schema_migrations data
COPY plex.schema_migrations (version, rollback_sql, optimize_on_rollback, min_version) FROM stdin;
pg_adapter_1.0.0	\N	\N	\N
20090909023322	\N	\N	\N
20090909023336	\N	\N	\N
20090909023346	\N	\N	\N
20090909023403	\N	\N	\N
20090911050510	\N	\N	\N
20090911050516	\N	\N	\N
20090919073339	\N	\N	\N
20090919204504	\N	\N	\N
20091001173531	\N	\N	\N
20091001174113	\N	\N	\N
20091023094310	\N	\N	\N
20091023095546	\N	\N	\N
20100103201027	\N	\N	\N
20100103201139	\N	\N	\N
20100316084646	\N	\N	\N
20100316084659	\N	\N	\N
20100326085538	\N	\N	\N
20100528200318	\N	\N	\N
20100624074248	\N	\N	\N
20101001034731	\N	\N	\N
20101106014611	\N	\N	\N
20101108123901	\N	\N	\N
20101213214640	\N	\N	\N
20110123005345	\N	\N	\N
20110208224235	\N	\N	\N
20110210210219	\N	\N	\N
20110213224325	\N	\N	\N
20110228205754	\N	\N	\N
20110303103325	\N	\N	\N
20110318010108	\N	\N	\N
20110318042447	\N	\N	\N
20110321192336	\N	\N	\N
20110403060904	\N	\N	\N
20110409010721	\N	\N	\N
20110419033239	\N	\N	\N
20110609004841	\N	\N	\N
20110609004928	\N	\N	\N
20110609004946	\N	\N	\N
20110627212748	\N	\N	\N
20110824001751	\N	\N	\N
20111210074434	\N	\N	\N
20120128005603	\N	\N	\N
20120328011928	\N	\N	\N
20120430195613	\N	\N	\N
20120601092900	\N	\N	\N
20120621065053	\N	\N	\N
20120621223321	\N	\N	\N
20120807024646	\N	\N	\N
20120924061056	\N	\N	\N
20120926052159	\N	\N	\N
20121011035113	\N	\N	\N
20130419192604	\N	\N	\N
20130725054748	\N	\N	\N
20130809052006	\N	\N	\N
20130828040137	\N	\N	\N
20131026213608	\N	\N	\N
20131110155400	\N	\N	\N
20131120000000	\N	\N	\N
20140115061336	\N	\N	\N
20140203073610	\N	\N	\N
20140227213926	\N	\N	\N
20140406023548	\N	\N	\N
20140708200053	\N	\N	\N
20140720200657	\N	\N	\N
20140810002525	\N	\N	\N
20140811001121	\N	\N	\N
20140811002027	\N	\N	\N
20140829040944	\N	\N	\N
20140617214540	\N	\N	\N
20150116180125	\N	\N	\N
20150414163622	\N	\N	\N
20150505122642	\N	\N	\N
20150523204431	\N	\N	\N
20150608182542	\N	\N	\N
20150610165102	\N	\N	\N
20150818011012	\N	\N	\N
20150218161152	\N	\N	\N
20151208182042	\N	\N	\N
20151221000000	\N	\N	\N
20160115145047	\N	\N	\N
20160312202000	\N	\N	\N
20151111185126	\N	\N	\N
20150819235734	\N	\N	\N
20160101152400	\N	\N	\N
20160212000000	\N	\N	\N
20160616000000	select 1	0	\N
20160610202642	select 1	0	\N
20161104000000	\N	\N	\N
20161109175500	\N	\N	\N
20161103000000	\N	\N	\N
20151205173000	\N	\N	\N
20160412152400	\N	\N	\N
20160728152400	\N	\N	\N
20160826152400	\N	\N	\N
20160806152400	\N	\N	\N
20170105152400	\N	\N	\N
20170110000000	\N	\N	\N
20170216152400	\N	\N	\N
20161214000000	\N	\N	\N
20170404000000	select 1	1	\N
20170618000000	\N	\N	\N
20170705000000	\N	\N	\N
20170617032400	\N	\N	\N
20170629000000	\N	\N	\N
20170630000000	\N	\N	\N
20170707000000	\N	\N	\N
20171018032400	\N	\N	\N
20180220032400	\N	\N	\N
20180324032400	update metadata_items set absolute_index=`index`,`index`=1 where metadata_type=18;update metadata_items set absolute_index=null where absolute_index is not null and metadata_type in (1,2)	0	\N
20180501000000	DROP index 'index_title_sort_naturalsort'	1	\N
20180531000000	select 1	1	\N
20180626000000	select 1	0	\N
20180703000000	\N	\N	\N
20180924000000	\N	\N	\N
20180928000000	\N	\N	\N
20181029131300	select 1	0	\N
20181119190600	\N	\N	\N
20181210190600	\N	\N	\N
20161017000000	\N	\N	\N
20170213173900	\N	\N	\N
20170403201322	\N	\N	\N
20170615000000	select 1	1	\N
20190130000000	select 1	0	\N
20190201190600	\N	\N	\N
20190205190600	\N	\N	\N
20190215190600	\N	\N	\N
20190218032400	\N	\N	\N
20190316132700	\N	\N	\N
20190403100000	update media_provider_resources set uri='https://podcasts.provider.plex.tv' where uri='provider://tv.plex.provider.podcasts';update media_provider_resources set uri='https://podcasts-staging.provider.plex.tv' where uri='provider://tv.plex.provider.podcasts-staging';	0	\N
20190501100000	select 1	0	\N
20190528190600	select 1	0	\N
20190520131301	delete from media_provider_resources where identifier = 'tv.plex.providers.epg.cloud';	0	\N
20190520131302	update media_provider_resources set parent_id = null where parent_id in (select parent_id from media_provider_resources where identifier = 'tv.plex.providers.epg.cloud');	0	\N
20190520131303	delete from media_provider_resources where id in (select parent_id from media_provider_resources where identifier = 'tv.plex.providers.epg.cloud');	0	\N
20190603140000	select 1	0	\N
20190612032400	\N	\N	\N
20190614032400	\N	\N	\N
20190708132500	\N	\N	\N
20190430032400	\N	\N	\N
20190111032400	select 1	0	\N
20190815130000	\N	\N	\N
20190912140000	select 1	0	\N
20180330131300	\N	\N	\N
20190801130200	select 1	0	\N
20190604032400	select 1	1	\N
20190919032400	UPDATE library_sections set agent='com.plexapp.agents.plexmusic', scanner='Plex Premium Music Scanner' where agent='tv.plex.agents.music'	0	\N
20190920032400	\N	\N	\N
20190616032400	select 1	1	\N
20191003131300	select 1	0	\N
20191213143300	select 1	1	\N
20200110143300	\N	\N	\N
20200114193300	\N	\N	\N
20200124193500	\N	\N	\N
20200131193503	UPDATE metadata_items SET extra_data = replace(extra_data, 'pv%3AreadOnly=', 'at%3AreadOnly=') WHERE extra_data LIKE '%pv^%3AreadOnly=%' escape '^'	0	\N
20191125131300	select 1	0	\N
20200224131300	select 1	0	\N
20200327131300	select 1	0	\N
20200401131300	UPDATE library_sections SET user_fields = replace(replace(user_fields, 'pr%3Ahidden=0', 'pr%includeInGlobal=1'), 'pr%3Ahidden=1', 'pr%3AincludeInGlobal=0') WHERE user_fields LIKE '%pr^%3Ahidden=%' escape '^'	0	\N
20200506172900	select 1	0	\N
20200515172900	select 1	0	\N
20200610150000	select 1	0	\N
20200615032400	\N	\N	\N
20200701090000	select 1	0	\N
20200728130000	select 1	0	\N
20200731130000	select 1	0	\N
20200812130000	select 1	0	\N
20200921130000	\N	\N	\N
20201103130000	select 1	0	\N
20201119130000	select 1	0	\N
20210304150000	\N	\N	\N
500000000000	CREATE INDEX 'index_title_sort_naturalsort' ON 'metadata_items' ('title_sort' COLLATE naturalsort)	1	\N
500000000000.011	DROP index if exists 'index_title_sort_naturalsort'	0	\N
500000000000.021	DROP index if exists 'index_title_sort_icu'	0	\N
500000000001	CREATE TRIGGER fts4_tag_titles_after_insert AFTER INSERT ON tags BEGIN INSERT INTO fts4_tag_titles(docid, tag) VALUES(new.rowid, new.tag); END	0	\N
500000000001.011	CREATE TRIGGER fts4_tag_titles_after_update AFTER UPDATE ON tags BEGIN INSERT INTO fts4_tag_titles(docid, tag) VALUES(new.rowid, new.tag); END	0	\N
500000000001.021	CREATE TRIGGER fts4_tag_titles_before_delete BEFORE DELETE ON tags BEGIN DELETE FROM fts4_tag_titles WHERE docid=old.rowid; END	0	\N
500000000001.031	CREATE TRIGGER fts4_tag_titles_before_update BEFORE UPDATE ON tags BEGIN DELETE FROM fts4_tag_titles WHERE docid=old.rowid; END	0	\N
500000000001.041	CREATE TRIGGER fts4_metadata_titles_after_insert AFTER INSERT ON metadata_items BEGIN INSERT INTO fts4_metadata_titles(docid, title, title_sort, original_title) VALUES(new.rowid, new.title, new.title_sort, new.original_title); END	0	\N
500000000001.051	CREATE TRIGGER fts4_metadata_titles_after_update AFTER UPDATE ON metadata_items BEGIN INSERT INTO fts4_metadata_titles(docid, title, title_sort, original_title) VALUES(new.rowid, new.title, new.title_sort, new.original_title); END	0	\N
500000000001.061	CREATE TRIGGER fts4_metadata_titles_before_delete BEFORE DELETE ON metadata_items BEGIN DELETE FROM fts4_metadata_titles WHERE docid=old.rowid; END	0	\N
500000000001.071	CREATE TRIGGER fts4_metadata_titles_before_update BEFORE UPDATE ON metadata_items BEGIN DELETE FROM fts4_metadata_titles WHERE docid=old.rowid; END	0	\N
500000000001.081	drop trigger if exists fts4_tag_titles_after_insert	0	\N
500000000001.091	drop trigger if exists fts4_tag_titles_after_update	0	\N
500000000001.101	drop trigger if exists fts4_tag_titles_before_delete	0	\N
500000000001.111	drop trigger if exists fts4_tag_titles_before_update	0	\N
500000000001.121	drop trigger if exists fts4_metadata_titles_after_insert	0	\N
500000000001.131	drop trigger if exists fts4_metadata_titles_after_update	0	\N
500000000001.141	drop trigger if exists fts4_metadata_titles_before_delete	0	\N
500000000001.151	drop trigger if exists fts4_metadata_titles_before_update	0	\N
500000000001.161	drop trigger if exists fts4_tag_titles_after_insert_icu	0	\N
500000000001.171	drop trigger if exists fts4_tag_titles_after_update_icu	0	\N
500000000001.181	drop trigger if exists fts4_tag_titles_before_delete_icu	0	\N
500000000001.191	drop trigger if exists fts4_tag_titles_before_update_icu	0	\N
500000000001.201	drop trigger if exists fts4_metadata_titles_after_insert_icu	0	\N
500000000001.211	drop trigger if exists fts4_metadata_titles_after_update_icu	0	\N
500000000001.221	drop trigger if exists fts4_metadata_titles_before_delete_icu	0	\N
500000000001.231	drop trigger if exists fts4_metadata_titles_before_update_icu	0	\N
20210628131300	select 1	0	\N
202107070000	select 1	1	\N
202107221100	update metadata_items set refreshed_at=NULL where id in (select DISTINCT metadata_items.id from metadata_items join media_items on media_items.metadata_item_id = metadata_items.id where media_items.id in ( select DISTINCT media_item_id from media_streams where language<>'' and length(language)<>3 and url<>'' and url<>'blob://'))	0	\N
202107221100.011	update media_items set media_analysis_version=0 where media_analysis_version>0 and id in (select distinct media_item_id from media_streams where language<>'' and length(language)<>3)	0	\N
20210726150000	\N	\N	\N
20210830032400	select 1	0	\N
202109061500	select 1	1	\N
20210922132300	\N	\N	\N
20211027132200	\N	\N	\N
20211116115800	select 1	1	\N
20211208163900	select 1	0	\N
202203040100	update metadata_items set originally_available_at = iif(typeof(originally_available_at) in ('integer', 'real'), datetime(originally_available_at, 'unixepoch'), originally_available_at), available_at = iif(typeof(available_at) in ('integer', 'real'), datetime(available_at, 'unixepoch', 'localtime'), available_at), expires_at = iif(typeof(expires_at) in ('integer', 'real'), datetime(expires_at, 'unixepoch', 'localtime'), expires_at), refreshed_at = iif(typeof(refreshed_at) in ('integer', 'real'), datetime(refreshed_at, 'unixepoch', 'localtime'), refreshed_at), added_at = iif(typeof(added_at) in ('integer', 'real'), datetime(added_at, 'unixepoch', 'localtime'), added_at), created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at), deleted_at = iif(typeof(deleted_at) in ('integer', 'real'), datetime(deleted_at, 'unixepoch', 'localtime'), deleted_at)	1	\N
202203040100.011	PRAGMA writable_schema = RESET	0	\N
202203040100.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'metadata_items' AND type = 'table'	0	\N
202203040100.031	PRAGMA writable_schema = TRUE	0	\N
202209271322.011	CREATE INDEX 'index_synchronization_files_on_sync_item_id' ON 'synchronization_files' ('sync_item_id' )	0	\N
202209271322.021	CREATE INDEX 'index_synchronization_files_on_sync_list_id' ON 'synchronization_files' ('sync_list_id' )	0	\N
202203220200	update media_items set begins_at = iif(typeof(begins_at) in ('integer', 'real'), datetime(begins_at, 'unixepoch'), begins_at), ends_at = iif(typeof(ends_at) in ('integer', 'real'), datetime(ends_at, 'unixepoch'), ends_at), created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at), deleted_at = iif(typeof(deleted_at) in ('integer', 'real'), datetime(deleted_at, 'unixepoch', 'localtime'), deleted_at)	1	\N
202203220200.011	PRAGMA writable_schema = RESET	0	\N
202203220200.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_items' AND type = 'table'	0	\N
202203220200.031	PRAGMA writable_schema = TRUE	0	\N
202204252200	update media_parts set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at), deleted_at = iif(typeof(deleted_at) in ('integer', 'real'), datetime(deleted_at, 'unixepoch', 'localtime'), deleted_at)	1	\N
202204252200.011	PRAGMA writable_schema = RESET	0	\N
202204252200.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_parts' AND type = 'table'	0	\N
202204252200.031	PRAGMA writable_schema = TRUE	0	\N
202204252300	update media_streams set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202204252300.011	PRAGMA writable_schema = RESET	0	\N
202204252300.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_streams' AND type = 'table'	0	\N
202204252300.031	PRAGMA writable_schema = TRUE	0	\N
202204252330	update metadata_item_settings set last_viewed_at = iif(typeof(last_viewed_at) in ('integer', 'real'), datetime(last_viewed_at, 'unixepoch', 'localtime'), last_viewed_at), last_skipped_at = iif(typeof(last_skipped_at) in ('integer', 'real'), datetime(last_skipped_at, 'unixepoch', 'localtime'), last_skipped_at), last_rated_at = iif(typeof(last_rated_at) in ('integer', 'real'), datetime(last_rated_at, 'unixepoch', 'localtime'), last_rated_at), created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202204252330.011	PRAGMA writable_schema = RESET	0	\N
202204252330.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'metadata_item_settings' AND type = 'table'	0	\N
202204252330.031	PRAGMA writable_schema = TRUE	0	\N
202205090900	update media_item_settings set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202205090900.011	PRAGMA writable_schema = RESET	0	\N
202205090900.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_item_settings' AND type = 'table'	0	\N
202205090900.031	PRAGMA writable_schema = TRUE	0	\N
202205090930	update media_part_settings set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202205090930.011	PRAGMA writable_schema = RESET	0	\N
202205090930.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_part_settings' AND type = 'table'	0	\N
202205090930.031	PRAGMA writable_schema = TRUE	0	\N
202205090940	update media_stream_settings set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202205090940.011	PRAGMA writable_schema = RESET	0	\N
202205090940.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_stream_settings' AND type = 'table'	0	\N
202205090940.031	PRAGMA writable_schema = TRUE	0	\N
202205091600	update metadata_item_views set originally_available_at = iif(typeof(originally_available_at) in ('integer', 'real'), datetime(originally_available_at, 'unixepoch', 'localtime'), originally_available_at), viewed_at = iif(typeof(viewed_at) in ('integer', 'real'), datetime(viewed_at, 'unixepoch', 'localtime'), viewed_at)	1	\N
202205091600.011	PRAGMA writable_schema = RESET	0	\N
202205091600.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'metadata_item_views' AND type = 'table'	0	\N
202205091600.031	PRAGMA writable_schema = TRUE	0	\N
202205091700	update view_settings set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202205091700.011	PRAGMA writable_schema = RESET	0	\N
202205091700.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'view_settings' AND type = 'table'	0	\N
202205091700.031	PRAGMA writable_schema = TRUE	0	\N
202205181200	\N	\N	\N
202206291100	\N	\N	\N
20220818122500	delete from metadata_relations where relation_type = 100	0	\N
202209091100	CREATE INDEX 'index_cloudsync_files_on_device_identifier_and_original_url' ON 'cloudsync_files' ('device_identifier', 'original_url')	0	\N
202209091100.011	DROP INDEX IF EXISTS 'index_cloudsync_files_on_device_identifier_and_original_url'	0	\N
202209091100.021	CREATE TABLE 'cloudsync_files' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'device_identifier' varchar(255), 'original_url' varchar(255), 'provider' varchar(255), 'new_key' varchar(255), 'query_string' varchar(255), 'extra_data' varchar(255))	0	\N
202209091100.031	DROP TABLE IF EXISTS cloudsync_files	0	\N
20220911115800	select 1	1	\N
202209271322	CREATE INDEX 'index_synchronization_files_on_item_uri' ON 'synchronization_files' ('item_uri' )	0	\N
202209271322.031	CREATE INDEX 'index_synchronization_files_on_client_identifier' ON 'synchronization_files' ('client_identifier' )	0	\N
202209271322.041	CREATE TABLE IF NOT EXISTS 'synchronization_files' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'client_identifier' varchar(255), 'sync_list_id' integer(8), 'sync_item_id' integer(8), 'item_uri' varchar(255), 'num_parts' integer, 'state' integer, 'state_context' integer, 'extra_data' varchar(255))	0	\N
202209271322.051	DROP TABLE IF EXISTS 'synchronization_files'	0	\N
202209271322.061	CREATE UNIQUE INDEX 'index_synced_play_queue_generators_on_sync_list_id_and_play_queue_generator_id' ON 'synced_play_queue_generators' ('sync_list_id', 'play_queue_generator_id' )	0	\N
202209271322.071	CREATE INDEX 'index_synced_play_queue_generators_on_state' ON 'synced_play_queue_generators' ('state' )	0	\N
202209271322.081	CREATE INDEX 'index_synced_play_queue_generators_on_changed_at' ON 'synced_play_queue_generators' ('changed_at' )	0	\N
202209271322.091	CREATE INDEX 'index_synced_play_queue_generators_on_play_queue_generator_id' ON 'synced_play_queue_generators' ('play_queue_generator_id' )	0	\N
202209271322.101	CREATE INDEX 'index_synced_play_queue_generators_on_playlist_id' ON 'synced_play_queue_generators' ('playlist_id' )	0	\N
202209271322.111	CREATE INDEX 'index_synced_play_queue_generators_on_sync_item_id' ON 'synced_play_queue_generators' ('sync_item_id' )	0	\N
202209271322.121	CREATE INDEX 'index_synced_play_queue_generators_on_sync_list_id' ON 'synced_play_queue_generators' ('sync_list_id' )	0	\N
202209271322.131	CREATE TABLE IF NOT EXISTS 'synced_play_queue_generators' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'sync_list_id' integer(8), 'sync_item_id' integer(8), 'playlist_id' integer, 'play_queue_generator_id' integer, 'changed_at' integer(8), 'state' integer, 'state_context' integer, 'first_packaged_at' integer(8))	0	\N
202209271322.141	DROP TABLE IF EXISTS 'synced_play_queue_generators'	0	\N
202209271322.151	CREATE UNIQUE INDEX 'index_synced_metadata_items_on_sync_list_id_and_metadata_item_id' ON 'synced_metadata_items' ('sync_list_id', 'metadata_item_id' )	0	\N
202209271322.161	CREATE INDEX 'index_synced_metadata_items_library_section_id' ON 'synced_metadata_items' ('library_section_id')	0	\N
202209271322.171	CREATE INDEX 'index_synced_metadata_items_parent_id' ON 'synced_metadata_items' ('parent_id')	0	\N
202209271322.181	CREATE INDEX 'index_synced_metadata_items_on_state' ON 'synced_metadata_items' ('state' )	0	\N
202209271322.191	CREATE INDEX 'index_synced_metadata_items_on_first_packaged_at' ON 'synced_metadata_items' ('first_packaged_at' )	0	\N
202209271322.201	CREATE INDEX 'index_synced_metadata_items_on_changed_at' ON 'synced_metadata_items' ('changed_at' )	0	\N
202209271322.211	CREATE INDEX 'index_synced_metadata_items_on_metadata_item_id' ON 'synced_metadata_items' ('metadata_item_id' )	0	\N
202209271322.221	CREATE INDEX 'index_synced_metadata_items_on_sync_item_id' ON 'synced_metadata_items' ('sync_item_id' )	0	\N
202209271322.231	CREATE INDEX 'index_synced_metadata_items_on_sync_list_id' ON 'synced_metadata_items' ('sync_list_id' )	0	\N
202209271322.241	CREATE TABLE IF NOT EXISTS 'synced_metadata_items' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'sync_list_id' integer(8), 'sync_item_id' integer(8), 'metadata_item_id' integer, 'changed_at' integer(8), 'first_packaged_at' integer(8), 'state' integer, 'state_context' integer, 'selected_media_id' integer, 'selected_part_id' integer, 'media_decision' integer, 'file_size' integer(8), 'media_analysis_extra_data' varchar(255), 'parent_id' integer, 'library_section_id' integer)	0	\N
202209271322.251	DROP TABLE IF EXISTS 'synced_metadata_items'	0	\N
202209271322.261	CREATE UNIQUE INDEX 'index_synced_library_sections_on_sync_list_id_and_library_section_id' ON 'synced_library_sections' ('sync_list_id', 'library_section_id')	0	\N
202209271322.271	CREATE INDEX 'index_synced_library_sections_state' ON 'synced_library_sections' ('state')	0	\N
202209271322.281	CREATE INDEX 'index_synced_library_sections_on_reference_count' ON 'synced_library_sections' ('reference_count')	0	\N
202209271322.291	CREATE INDEX 'index_synced_library_sections_on_changed_at' ON 'synced_library_sections' ('changed_at')	0	\N
202209271322.301	CREATE INDEX 'index_synced_library_sections_on_library_section_id' ON 'synced_library_sections' ('library_section_id')	0	\N
202209271322.311	CREATE INDEX 'index_synced_library_sections_on_sync_list_id' ON 'synced_library_sections' ('sync_list_id')	0	\N
202209271322.321	CREATE TABLE IF NOT EXISTS 'synced_library_sections' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'sync_list_id' integer(8), 'library_section_id' integer, 'changed_at' integer(8), 'reference_count' integer, 'first_packaged_at' integer(8), 'state' integer)	0	\N
202209271322.331	DROP TABLE IF EXISTS 'synced_library_sections'	0	\N
202209271322.341	CREATE UNIQUE INDEX 'index_synced_ancestor_items_on_sync_list_id_and_metadata_item_id' ON 'synced_ancestor_items' ('sync_list_id', 'metadata_item_id')	0	\N
202209271322.351	CREATE INDEX 'index_synced_ancestor_items_state' ON 'synced_ancestor_items' ('state')	0	\N
202209271322.361	CREATE INDEX 'index_synced_ancestor_items_parent_id' ON 'synced_ancestor_items' ('parent_id')	0	\N
202209271322.371	CREATE INDEX 'index_synced_ancestor_items_on_reference_count' ON 'synced_ancestor_items' ('reference_count')	0	\N
202209271322.381	CREATE INDEX 'index_synced_ancestor_items_on_changed_at' ON 'synced_ancestor_items' ('changed_at')	0	\N
202209271322.391	CREATE INDEX 'index_synced_ancestor_items_on_metadata_item_id' ON 'synced_ancestor_items' ('metadata_item_id')	0	\N
202209271322.401	CREATE INDEX 'index_synced_ancestor_items_on_sync_list_id' ON 'synced_ancestor_items' ('sync_list_id')	0	\N
202209271322.411	CREATE TABLE IF NOT EXISTS 'synced_ancestor_items' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'sync_list_id' integer(8), 'metadata_item_id' integer, 'changed_at' integer(8), 'reference_count' integer, 'first_packaged_at' integer(8), 'parent_id' integer, 'state' integer)	0	\N
202209271322.421	DROP TABLE IF EXISTS synced_ancestor_items	0	\N
202209271322.431	CREATE INDEX 'index_sync_schema_versions_on_changed_at' ON 'sync_schema_versions' ('changed_at')	0	\N
202209271322.441	CREATE TABLE IF NOT EXISTS 'sync_schema_versions' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'version' integer, 'changed_at' integer(8))	0	\N
202209271322.451	DROP TABLE IF EXISTS 'sync_schema_versions'	0	\N
202309200914	UPDATE metadata_item_settings SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202210260000	update directories set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at), deleted_at = iif(typeof(deleted_at) in ('integer', 'real'), datetime(deleted_at, 'unixepoch', 'localtime'), deleted_at)	1	\N
202210260000.011	PRAGMA writable_schema = RESET	0	\N
202210260000.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'directories' AND type = 'table'	0	\N
202210260000.031	PRAGMA writable_schema = TRUE	0	\N
202210260100	update library_sections set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at), scanned_at = iif(typeof(scanned_at) in ('integer', 'real'), datetime(scanned_at, 'unixepoch', 'localtime'), scanned_at)	1	\N
202210260100.011	PRAGMA writable_schema = RESET	0	\N
202210260100.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'library_sections' AND type = 'table'	0	\N
202210260100.031	PRAGMA writable_schema = TRUE	0	\N
202210260200	update section_locations set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at), scanned_at = iif(typeof(scanned_at) in ('integer', 'real'), datetime(scanned_at, 'unixepoch', 'localtime'), scanned_at)	1	\N
202210260200.011	PRAGMA writable_schema = RESET	0	\N
202210260200.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'section_locations' AND type = 'table'	0	\N
202210260200.031	PRAGMA writable_schema = TRUE	0	\N
202212012200	update tags set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202212012200.011	PRAGMA writable_schema = RESET	0	\N
202212012200.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'tags' AND type = 'table'	0	\N
202212012200.031	PRAGMA writable_schema = TRUE	0	\N
202212012300	update taggings set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at)	1	\N
202212012300.011	PRAGMA writable_schema = RESET	0	\N
202212012300.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'taggings' AND type = 'table'	0	\N
202212012300.031	PRAGMA writable_schema = TRUE	0	\N
202212022100	update media_grabs set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202212022100.011	PRAGMA writable_schema = RESET	0	\N
202212022100.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_grabs' AND type = 'table'	0	\N
202212022100.031	PRAGMA writable_schema = TRUE	0	\N
202212022200	update media_provider_resources set last_seen_at = iif(typeof(last_seen_at) in ('integer', 'real'), datetime(last_seen_at, 'unixepoch', 'localtime'), last_seen_at), created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202212022200.011	PRAGMA writable_schema = RESET	0	\N
202212022200.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_provider_resources' AND type = 'table'	0	\N
202212022200.031	PRAGMA writable_schema = TRUE	0	\N
202212022300	update media_subscriptions set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at), updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202212022300.011	PRAGMA writable_schema = RESET	0	\N
202212022300.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_subscriptions' AND type = 'table'	0	\N
202212022300.031	PRAGMA writable_schema = TRUE	0	\N
202301280000	\N	\N	\N
20230118160000	\N	\N	\N
20230830160000	select 1	0	\N
202309200901	UPDATE external_metadata_items SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200902	UPDATE hub_templates SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200903	UPDATE locatables SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200904	UPDATE media_grabs SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200905	UPDATE media_provider_resources SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200906	UPDATE media_stream_settings SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200907	UPDATE media_subscriptions SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200908	UPDATE metadata_agent_providers SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200909	UPDATE metadata_item_clusters SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200910	UPDATE metadata_item_setting_markers SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200911	UPDATE media_items SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200912	UPDATE media_parts SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200913	UPDATE media_streams SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202402260801	\N	\N	\N
202309200915	UPDATE metadata_items SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200916	UPDATE play_queue_generators SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200917	UPDATE play_queues SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200918	UPDATE taggings SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202309200919	UPDATE tags SET extra_data = extra_data ->> 'url' WHERE extra_data IS NOT NULL and json_valid(extra_data)	0	\N
202311120800	select 1	0	\N
202311171400	select 1	0	\N
20231120161500	\N	\N	\N
202312190800	update blobs set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at)	1	\N
202312190800.011	PRAGMA writable_schema = RESET	0	\N
202312190800.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'blobs' AND type = 'table'	0	\N
202312190800.031	PRAGMA writable_schema = TRUE	0	\N
202401290800	update locatables set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at),updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202401290800.011	PRAGMA writable_schema = RESET	0	\N
202401290800.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'locatables' AND type = 'table'	0	\N
202401290800.031	PRAGMA writable_schema = TRUE	0	\N
202401290801	update accounts set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at),updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202401290801.011	PRAGMA writable_schema = RESET	0	\N
202401290801.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'accounts' AND type = 'table'	0	\N
202401290801.031	PRAGMA writable_schema = TRUE	0	\N
202401290802	update metadata_item_clusters set starts_at = iif(typeof(starts_at) in ('integer', 'real'), datetime(starts_at, 'unixepoch', 'localtime'), starts_at),ends_at = iif(typeof(ends_at) in ('integer', 'real'), datetime(ends_at, 'unixepoch', 'localtime'), ends_at)	1	\N
202401290802.011	PRAGMA writable_schema = RESET	0	\N
202401290802.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'metadata_item_clusters' AND type = 'table'	0	\N
202401290802.031	PRAGMA writable_schema = TRUE	0	\N
202401290803	update metadata_relations set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at),updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202401290803.011	PRAGMA writable_schema = RESET	0	\N
202401290803.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'metadata_relations' AND type = 'table'	0	\N
202401290803.031	PRAGMA writable_schema = TRUE	0	\N
202401290804	update play_queues set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at),updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202401290804.011	PRAGMA writable_schema = RESET	0	\N
202401290804.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'play_queues' AND type = 'table'	0	\N
202401290804.031	PRAGMA writable_schema = TRUE	0	\N
202401290805	update play_queue_generators set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at),updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202401290805.011	PRAGMA writable_schema = RESET	0	\N
202401290805.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'play_queue_generators' AND type = 'table'	0	\N
202401290805.031	PRAGMA writable_schema = TRUE	0	\N
202401290806	update plugins set installed_at = iif(typeof(installed_at) in ('integer', 'real'), datetime(installed_at, 'unixepoch', 'localtime'), installed_at),accessed_at = iif(typeof(accessed_at) in ('integer', 'real'), datetime(accessed_at, 'unixepoch', 'localtime'), accessed_at),modified_at = iif(typeof(modified_at) in ('integer', 'real'), datetime(modified_at, 'unixepoch', 'localtime'), modified_at)	1	\N
202401290806.011	PRAGMA writable_schema = RESET	0	\N
202401290806.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'plugins' AND type = 'table'	0	\N
202401290806.031	PRAGMA writable_schema = TRUE	0	\N
202401290807	update devices set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at),updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202401290807.011	PRAGMA writable_schema = RESET	0	\N
202401290807.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'devices' AND type = 'table'	0	\N
202401290807.031	PRAGMA writable_schema = TRUE	0	\N
202401290808	update statistics_resources set at = iif(typeof(at) in ('integer', 'real'), datetime(at, 'unixepoch', 'localtime'), at)	1	\N
202401290808.011	update statistics_media set at = iif(typeof(at) in ('integer', 'real'), datetime(at, 'unixepoch', 'localtime'), at)	0	\N
202401290808.021	update statistics_bandwidth set at = iif(typeof(at) in ('integer', 'real'), datetime(at, 'unixepoch', 'localtime'), at)	0	\N
202401290808.031	PRAGMA writable_schema = RESET	0	\N
202401290808.041	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'statistics_resources' AND type = 'table'	0	\N
202401290808.051	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'statistics_media' AND type = 'table'	0	\N
202401290808.061	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'statistics_bandwidth' AND type = 'table'	0	\N
202401290808.071	PRAGMA writable_schema = TRUE	0	\N
202402260802	update library_timeline_entries set updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202402260802.011	PRAGMA writable_schema = RESET	0	\N
202402260802.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'library_timeline_entries' AND type = 'table'	0	\N
202402260802.031	PRAGMA writable_schema = TRUE	0	\N
202402260803	update media_metadata_mappings set created_at = iif(typeof(created_at) in ('integer', 'real'), datetime(created_at, 'unixepoch', 'localtime'), created_at),updated_at = iif(typeof(updated_at) in ('integer', 'real'), datetime(updated_at, 'unixepoch', 'localtime'), updated_at)	1	\N
202402260803.011	PRAGMA writable_schema = RESET	0	\N
202402260803.021	UPDATE sqlite_schema SET sql = replace(sql, 'dt_integer(8)', 'datetime') WHERE name = 'media_metadata_mappings' AND type = 'table'	0	\N
202402260803.031	PRAGMA writable_schema = TRUE	0	\N
202403110800	select 1	0	\N
202403120800	\N	\N	\N
202404300800	\N	\N	\N
202406050800	\N	\N	\N
202406250800	\N	\N	\N
202407231422	\N	\N	\N
20240718114400	UPDATE library_sections SET agent = 'com.plexapp.agents.none', language = 'en' WHERE agent = 'tv.plex.agents.none' AND section_type = 13	0	
202407301359	UPDATE library_sections SET agent = 'com.plexapp.agents.none', scanner = 'Plex Music Scanner', language = 'xn' WHERE agent = 'tv.plex.agents.none' AND scanner = 'Plex Music'	0	
202407301359.011	UPDATE library_sections SET agent = 'com.plexapp.agents.none', scanner = 'Plex Series Scanner', language = 'xn' WHERE agent = 'tv.plex.agents.none' AND scanner = 'Plex TV Series'	0	
202407301359.021	UPDATE library_sections SET agent = 'com.plexapp.agents.none', scanner = 'Plex Movie Scanner', language = 'xn' WHERE agent = 'tv.plex.agents.none' AND scanner = 'Plex Movie'	0	
202409201238	select 1	0	
20210207150001	select 1	0	\N
20210207150002	select 1	1	\N
20210207150000	select 1	0	\N
202302020000	select 1	0	\N
202502171508	\N	\N	\N
202503041220	\N	\N	\N
202503041514	\N	\N	\N
202504160804	\N	\N	\N
202504161541	CREATE INDEX 'index_accounts_on_name' ON 'accounts' ('name')	0	
202504161541.011	ALTER TABLE 'accounts' ADD 'salt' varchar(255)	0	
202504161541.021	ALTER TABLE 'accounts' ADD 'hashed_password' varchar(255)	0	
202504211423	\N	\N	\N
202504241552	CREATE TABLE IF NOT EXISTS 'media_metadata_mappings' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'media_guid' varchar(255), 'metadata_guid' varchar(255), 'created_at' dt_integer(8), 'updated_at' dt_integer(8));	0	
202504301225	CREATE INDEX 'index_library_timeline_entries_on_updated_at' ON 'library_timeline_entries' ('updated_at' )	0	
202504301225.011	CREATE INDEX 'index_library_timeline_entries_on_state' ON 'library_timeline_entries' ('state' )	0	
202504301225.021	CREATE INDEX 'index_library_timeline_entries_on_metadata_item_id' ON 'library_timeline_entries' ('metadata_item_id' )	0	
202504301225.031	CREATE INDEX 'index_library_timeline_entries_on_library_section_id' ON 'library_timeline_entries' ('library_section_id' )	0	
202504301225.041	CREATE TABLE IF NOT EXISTS 'library_timeline_entries' ('id' INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, 'library_section_id' integer, 'metadata_item_id' integer, 'state' integer, 'updated_at' dt_integer(8))	0	
202505261219	select 1	1	
202507011200	\N	\N	\N
202507311200	\N	\N	\N
\.

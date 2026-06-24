-- PostgreSQL compatibility functions used by the SQLite->PostgreSQL translator.
-- This file is safe to run multiple times.

CREATE OR REPLACE FUNCTION public.jsonb_mergepatch(target jsonb, patch jsonb)
RETURNS jsonb
LANGUAGE plpgsql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $fn$
DECLARE
    result jsonb;
    k text;
    v jsonb;
BEGIN
    -- RFC 7396: non-object patch replaces target entirely.
    IF jsonb_typeof(patch) <> 'object' THEN
        RETURN patch;
    END IF;

    -- RFC 7396: object patch against non-object target starts from {}.
    IF jsonb_typeof(target) <> 'object' THEN
        result := '{}'::jsonb;
    ELSE
        result := target;
    END IF;

    FOR k, v IN SELECT e.key, e.value FROM jsonb_each(patch) AS e(key, value) LOOP
        -- RFC 7396: null in patch means remove key.
        IF v = 'null'::jsonb THEN
            result := result - k;
        ELSIF (result ? k)
              AND jsonb_typeof(result -> k) = 'object'
              AND jsonb_typeof(v) = 'object' THEN
            result := jsonb_set(result, ARRAY[k], public.jsonb_mergepatch(result -> k, v), true);
        ELSE
            result := jsonb_set(result, ARRAY[k], v, true);
        END IF;
    END LOOP;

    RETURN result;
END;
$fn$;

CREATE OR REPLACE FUNCTION public.json_valid(val text)
RETURNS boolean AS $$
BEGIN
  IF val IS NULL THEN
    RETURN NULL;
  END IF;
  PERFORM val::json;
  RETURN TRUE;
EXCEPTION WHEN OTHERS THEN
  RETURN FALSE;
END;
$$ LANGUAGE plpgsql IMMUTABLE;

CREATE OR REPLACE FUNCTION public.eq_bool_int(b boolean, i integer)
RETURNS boolean AS $$
BEGIN
  RETURN (b = (i <> 0));
END;
$$ LANGUAGE plpgsql IMMUTABLE;

CREATE OR REPLACE FUNCTION public.eq_int_bool(i integer, b boolean)
RETURNS boolean AS $$
BEGIN
  RETURN ((i <> 0) = b);
END;
$$ LANGUAGE plpgsql IMMUTABLE;

DROP OPERATOR IF EXISTS public.= (boolean, integer);
CREATE OPERATOR public.= (
  PROCEDURE = public.eq_bool_int,
  LEFTARG = boolean,
  RIGHTARG = integer,
  COMMUTATOR = =
);

DROP OPERATOR IF EXISTS public.= (integer, boolean);
CREATE OPERATOR public.= (
  PROCEDURE = public.eq_int_bool,
  LEFTARG = integer,
  RIGHTARG = boolean,
  COMMUTATOR = =
);



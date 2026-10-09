#!/usr/bin/env python3
"""Lossless, snapshot-based data export using psql and Python's SQLite driver.

The bundled schema does not certify a native Plex version. Outputs
are data exports by default. --native-plex-sqlite rebuilds native virtual
indices in staging with the supplied official CLI; this alone does not certify
Plex server rollback. The official ICU NULL-validator diagnostic is accepted
only with corresponding NULL source values, clean physical table integrity and
regenerated index/source count and MATCH checks. Other errors fail closed.
--native-rollback fails closed until a version-matched
schema and native search/watchstate/playlist/artwork verification are available.
Publication replaces two files sequentially with rollback on ordinary errors;
it is not crash-atomic as a pair. Consumers must be stopped during publication.
"""

import argparse
import contextlib
import decimal
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import sqlite3
import subprocess
import sys
import tempfile
import uuid


LIBRARY = "com.plexapp.plugins.library.db"
ARTWORK = "com.plexapp.plugins.library.blobs.db"
INTEGER_TYPES = {"int2", "int4", "int8"}
REAL_TYPES = {"float4", "float8"}
TEXT_TYPES = {"text", "varchar", "bpchar", "uuid", "json", "jsonb", "interval", "tsvector"}
TIME_TYPES = {"date", "timestamp", "timestamptz"}
SUPPORTED_TYPES = INTEGER_TYPES | REAL_TYPES | TEXT_TYPES | TIME_TYPES | {"bool", "bytea"}


def identifier(value):
    if any(character in value for character in "\x00\n\r"):
        raise ValueError("Unsupported newline or NUL in SQL identifier")
    return '"' + value.replace('"', '""') + '"'


class Postgres:
    def __init__(self):
        environment = dict(os.environ, PGCLIENTENCODING="UTF8")
        self.process = subprocess.Popen(
            ["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1", "-P", "pager=off"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            encoding="utf-8", env=environment,
        )

    def query(self, sql):
        marker = "export_end_" + uuid.uuid4().hex
        self.process.stdin.write(sql + "\n\\echo " + marker + "\n")
        self.process.stdin.flush()
        for line in self.process.stdout:
            if line.rstrip("\n") == marker:
                return
            yield json.loads(line)
        raise RuntimeError(f"PostgreSQL export failed (psql exit {self.process.wait()})")

    def execute(self, sql):
        if list(self.query(sql)):
            raise RuntimeError("Unexpected PostgreSQL command output")

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.process.stdin.close()
        self.process.stdout.close()


def catalog(postgres, schema):
    schema_literal = "'" + schema.replace("'", "''") + "'"
    rows = postgres.query(f"""
        SELECT json_build_array(c.relname, a.attname, t.typname, tn.nspname)
        FROM pg_catalog.pg_class c
        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
        JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
        JOIN pg_catalog.pg_namespace tn ON tn.oid = t.typnamespace
        WHERE n.nspname = {schema_literal} AND c.relkind IN ('r', 'p')
          AND NOT c.relispartition AND a.attnum > 0 AND NOT a.attisdropped
        ORDER BY c.relname, a.attnum;
    """)
    tables = {}
    for table, column, pg_type, namespace in rows:
        identifier(table)
        identifier(column)
        if namespace != "pg_catalog" or pg_type not in SUPPORTED_TYPES:
            raise ValueError(f"Unsupported schema type: {table}.{column}: {namespace}.{pg_type}")
        tables.setdefault(table, []).append((column, pg_type))
    if "metadata_items" not in tables or "blobs" not in tables:
        raise ValueError("Unsupported schema: metadata_items and blobs tables are required")
    return tables


def schema_statements(path, include_virtual=False):
    pending = ""
    for line in path.read_text(encoding="utf-8").splitlines(keepends=True):
        pending += line
        if sqlite3.complete_statement(pending):
            statement = pending.strip()
            pending = ""
            if statement.startswith("CREATE VIRTUAL TABLE") and not include_virtual:
                continue
            if not statement.startswith(("CREATE TABLE", "CREATE INDEX", "CREATE UNIQUE INDEX", "CREATE VIRTUAL TABLE", "CREATE TRIGGER")):
                raise ValueError("Unsupported statement in SQLite schema")
            yield statement
    if pending.strip():
        raise ValueError("Incomplete SQLite schema statement")


def native_derived_table(table):
    return table.startswith(("fts4_", "spellfix_")) or table in {
        "locations_node", "locations_parent", "locations_rowid",
    }


def statement_table(statement):
    match = re.search(r'(?:TABLE(?: IF NOT EXISTS)?|ON)\s+["`]?([A-Za-z_][A-Za-z_0-9]*)', statement)
    return match.group(1) if match else None


def native_environment():
    # CLI must run with its own SQLite and modules, never the PostgreSQL shim.
    return {key: value for key, value in os.environ.items()
            if not key.startswith(("PLEX_PG_", "DYLD_")) and key not in {
                "LD_PRELOAD", "LD_LIBRARY_PATH", "LD_LOADER_PATH", "LD_AUDIT", "SQLITE_AUTO_EXTENSION", "SQLITE_EXTENSIONS",
            }}


def run_native_cli(executable, database, script):
    result = subprocess.run([str(executable), str(database)], input=".bail on\n" + script,
                            text=True, capture_output=True, env=native_environment(), timeout=300)
    if result.returncode:
        raise RuntimeError("Native Plex SQLite finalization failed: " + result.stderr.strip())
    return result.stdout.strip()


def literal(value):
    return "'" + value.replace("'", "''") + "'"


def native_json(executable, database, query):
    return json.loads(run_native_cli(executable, database, query))


def validate_native_library(executable, database, virtuals):
    if run_native_cli(executable, database, "PRAGMA foreign_key_check;"):
        raise ValueError("Native library foreign key validation failed")
    output = run_native_cli(executable, database, "PRAGMA integrity_check;")
    recognized = {
        "fts4_metadata_titles_icu": ("metadata_items", ("title", "title_sort", "original_title")),
        "fts4_tag_titles_icu": ("tags", ("tag",)),
    }
    limited = []
    if output != "ok":
        for line in output.splitlines():
            match = re.fullmatch(
                r"unable to validate the inverted index for FTS4 table main\.(fts4_metadata_titles_icu|fts4_tag_titles_icu): SQL logic error",
                line,
            )
            if not match or match.group(1) in limited or match.group(1) not in virtuals:
                raise ValueError("Native library integrity validation failed: " + output)
            table = match.group(1)
            source, columns = recognized[table]
            condition = " OR ".join(identifier(column) + " IS NULL" for column in columns)
            count = native_json(executable, database,
                                f"SELECT json(count(*)) FROM {identifier(source)} WHERE {condition};")
            if not count:
                raise ValueError("Native ICU validation error without NULL-source control: " + line)
            limited.append(table)
    # Even the recognized ICU NULL diagnostic never exempts physical tables,
    # ordinary indices, foreign keys or actual inverted-index search evidence.
    physical = native_json(executable, database,
        "SELECT json_group_array(name) FROM (SELECT name FROM sqlite_master WHERE type='table' "
        "AND sql NOT LIKE 'CREATE VIRTUAL TABLE%' ORDER BY name);")
    scoped = run_native_cli(executable, database,
                           "\n".join(f"PRAGMA integrity_check({literal(table)});" for table in physical))
    if scoped.splitlines() != ["ok"] * len(physical):
        raise ValueError("Native physical-table integrity validation failed: " + scoped)
    checks = []
    for table in virtuals:
        if not table.startswith('fts4_'):
            continue
        source = 'metadata_items' if 'metadata_titles' in table else 'tags'
        columns = ('title', 'title_sort', 'original_title') if source == 'metadata_items' else ('tag',)
        counts = native_json(executable, database,
            f"SELECT json_array((SELECT count(*) FROM {identifier(source)}), "
            f"(SELECT count(*) FROM {identifier(table + '_docsize')}));")
        if counts[0] != counts[1]:
            raise ValueError("Native regenerated index/source count mismatch: " + table)
        sample = native_json(executable, database,
            "SELECT json_group_array(json_array(" + ','.join(map(identifier, ('id',) + columns)) + ")) FROM "
            f"(SELECT * FROM {identifier(source)} ORDER BY id LIMIT 32);")
        for row in sample:
            for value in row[1:]:
                if not isinstance(value, str) or not any(character.isalnum() for character in value):
                    continue
                phrase = '"' + value.replace('"', '""') + '"'
                checks.append(f"SELECT json_array({literal(table)}, {int(row[0])}, EXISTS(SELECT 1 "
                              f"FROM {identifier(table)} WHERE docid={int(row[0])} "
                              f"AND {identifier(table)} MATCH {literal(phrase)}));")
    if checks:
        matches = run_native_cli(executable, database, "\n".join(checks))
        rows = [json.loads(line) for line in matches.splitlines()]
        if len(rows) != len(checks) or any(row[2] != 1 for row in rows):
            raise ValueError("Native regenerated index/source MATCH validation failed")
    report = {"known_icu_null_validator_limitation": limited, "physical_tables_validated": len(physical),
              "fts_source_match_checks": len(checks), "fts_source_counts_equal": True,
              "native_plex_server_certification": "required", "full_integrity_diagnostics": output.splitlines()}
    print("Native SQLite validation: " + json.dumps(report, sort_keys=True), flush=True)
    if limited:
        print("Known official Plex SQLite ICU NULL-validator limitation reproduced; "
              "NULL values preserved, physical integrity and source MATCH checks passed. "
              "Native Plex server rollback certification remains required.", flush=True)


def finalize_native(stage, schema_path, executable):
    statements = list(schema_statements(schema_path, include_virtual=True))
    library = stage / LIBRARY
    script = ["BEGIN;"]
    # The temporary ordinary table stores logical rows, never RTree nodes.
    script.extend(['CREATE TEMP TABLE plex_export_locations AS SELECT * FROM locations;', 'DROP TABLE locations;'])
    virtuals = []
    for statement in statements:
        if statement.startswith("CREATE VIRTUAL TABLE"):
            script.append(statement)
            virtuals.append(statement_table(statement))
    script.extend([
        'INSERT INTO locations SELECT id,lat_min,lat_max,lon_min,lon_max FROM plex_export_locations;',
        "SELECT CASE WHEN EXISTS(SELECT * FROM locations EXCEPT SELECT * FROM plex_export_locations) "
        "OR EXISTS(SELECT * FROM plex_export_locations EXCEPT SELECT * FROM locations) "
        "THEN 'RTREE_CONTENT_MISMATCH' ELSE 'RTREE_CONTENT_OK' END;",
        'DROP TABLE plex_export_locations;',
    ])
    for table in virtuals:
        if table.startswith('fts4_'):
            script.append(f"INSERT INTO {identifier(table)}({identifier(table)}) VALUES('rebuild');")
    for table, source, column in (("spellfix_metadata_titles", "metadata_items", "title"),
                                  ("spellfix_tag_titles", "tags", "tag")):
        if table in virtuals:
            script.append(f"INSERT INTO {identifier(table)}(word) SELECT DISTINCT {identifier(column)} "
                          f"FROM {identifier(source)} WHERE {identifier(column)} IS NOT NULL;")
    for statement in statements:
        if statement.startswith("CREATE TRIGGER"):
            script.append(statement)
    script.append("COMMIT;")
    output = run_native_cli(executable, library, "\n".join(script))
    if output != "RTREE_CONTENT_OK":
        raise ValueError("Native library integrity/foreign key/RTree validation failed: " + output)
    validate_native_library(executable, library, virtuals)
    output = run_native_cli(executable, stage / ARTWORK, "PRAGMA foreign_key_check; PRAGMA integrity_check;")
    if output != "ok":
        raise ValueError("Native artwork integrity/foreign key validation failed: " + output)


def create_destinations(stage, schema_path, native=False):
    library = sqlite3.connect(stage / LIBRARY)
    artwork = sqlite3.connect(stage / ARTWORK)
    try:
        for statement in schema_statements(schema_path):
            if native and (statement.startswith("CREATE TRIGGER") or native_derived_table(statement_table(statement) or "")):
                continue
            library.execute(statement)
        if native:
            library.execute('CREATE TABLE locations(id INTEGER PRIMARY KEY, lat_min REAL, lat_max REAL, lon_min REAL, lon_max REAL)')
        blob_schema = library.execute(
            "SELECT sql FROM sqlite_master WHERE tbl_name = 'blobs' AND sql IS NOT NULL "
            "ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END"
        ).fetchall()
        if not blob_schema:
            raise ValueError("SQLite schema has no blobs table")
        for (statement,) in blob_schema:
            artwork.execute(statement)
        library.execute('DROP TABLE "blobs"')
        library.commit()
        artwork.commit()
        return library, artwork
    except BaseException:
        library.close()
        artwork.close()
        raise


def column_mapping(database, table, columns, native=False):
    destination = database.execute(f"PRAGMA table_info({identifier(table)})").fetchall()
    target_types = {row[1]: row[2] for row in destination}
    source_names = {column for column, _ in columns}
    if native and source_names != set(target_types):
        raise ValueError(f"Native schema columns mismatch for {table}; refusing unsupported version/data loss")
    if source_names != set(target_types):
        print(f"  {table}: native schema unsupported; preserving exact source columns", flush=True)
        primary_key = [row[1] for row in sorted(destination, key=lambda row: row[5])
                       if row[5] and row[1] in source_names]
        if destination:
            database.execute(f"DROP TABLE {identifier(table)}")
        definitions = []
        for column, pg_type in columns:
            affinity = "INTEGER" if pg_type in INTEGER_TYPES | {"bool"} else (
                "REAL" if pg_type in REAL_TYPES else "BLOB" if pg_type == "bytea" else "TEXT"
            )
            affinity = target_types.get(column, affinity)
            definitions.append(f"{identifier(column)} {affinity}")
        if primary_key:
            definitions.append("PRIMARY KEY (" + ", ".join(map(identifier, primary_key)) + ")")
        database.execute(f"CREATE TABLE {identifier(table)} ({', '.join(definitions)})")
        destination = database.execute(f"PRAGMA table_info({identifier(table)})").fetchall()
    target_types = {row[1]: row[2] for row in destination}
    return [(column, pg_type, target_types[column]) for column, pg_type in columns]


def row_values(row, mapping):
    if len(row) != len(mapping):
        raise ValueError("Exported row does not match explicit column mapping")
    values = []
    for value, (column, pg_type, target_type) in zip(row, mapping):
        if value is None:
            converted = None
        elif pg_type == "bytea":
            converted = bytes.fromhex(value)
        elif pg_type in INTEGER_TYPES:
            converted = int(value)
        elif pg_type == "bool":
            if value not in ("true", "false"):
                raise ValueError(f"Invalid boolean in {column}")
            converted = int(value == "true")
        elif pg_type in REAL_TYPES:
            converted = float(value)
            if not math.isfinite(converted):
                raise ValueError(f"Unsupported non-finite number in {column}")
        elif pg_type in TIME_TYPES and "INT" in target_type.upper():
            epoch = decimal.Decimal(value)
            if epoch != epoch.to_integral_value():
                raise ValueError(f"Subsecond timestamp cannot fit integer column {column}")
            converted = int(epoch)
        else:
            converted = value
        values.append(converted)
    return values


def row_digest(values):
    tagged = [(type(value).__name__, value.hex() if isinstance(value, bytes) else value)
              for value in values]
    encoded = json.dumps(tagged, ensure_ascii=True, separators=(",", ":")).encode("utf-8")
    return int.from_bytes(hashlib.sha256(encoded).digest(), "big")


def export_table(postgres, schema, database, table, columns, native=False):
    destination_table = "pg_export_" + table if table.lower().startswith("sqlite_") else table
    if destination_table != table:
        print(f"  {table}: SQLite reserved name; exported as {destination_table}", flush=True)
    mapping = column_mapping(database, destination_table, columns, native=native)
    names = ", ".join(identifier(column) for column, _, _ in mapping)
    expressions = []
    for column, pg_type, target_type in mapping:
        quoted = identifier(column)
        if pg_type == "bytea":
            expression = f"encode({quoted}, 'hex')"
        elif pg_type in TIME_TYPES and "INT" in target_type.upper():
            expression = f"extract(epoch FROM {quoted})::text"
        else:
            expression = f"{quoted}::text"
        expressions.append(expression)
    qualified = f"{identifier(schema)}.{identifier(table)}"
    expected, = list(postgres.query(f"SELECT to_json(count(*)) FROM {qualified};"))
    placeholders = ", ".join("?" for _ in mapping)
    insert = f"INSERT INTO {identifier(destination_table)} ({names}) VALUES ({placeholders})"
    digest = 0
    count = 0
    for row in postgres.query(
        f"SELECT array_to_json(ARRAY[{', '.join(expressions)}]) FROM {qualified};"
    ):
        values = row_values(row, mapping)
        database.execute(insert, values)
        digest = (digest + row_digest(values)) % (1 << 256)
        count += 1
    actual_digest = 0
    actual_count = 0
    for row in database.execute(f"SELECT {names} FROM {identifier(destination_table)}"):
        actual_digest = (actual_digest + row_digest(row)) % (1 << 256)
        actual_count += 1
    if count != expected or actual_count != expected or actual_digest != digest:
        raise ValueError(f"Count/type/value validation failed for {table}")
    print(f"  {table}: {count} rows validated", flush=True)


def native_catalog(postgres, schema, tables, library, artwork):
    selected = {}
    shim_tables = {"maintenance_control", "sqlite_column_types"}
    derived_columns = {"metadata_items": {"search_vector", "title_fts", "subtype"},
                       "tags": {"search_vector"}, "schema_migrations": {"id"}}
    for table, columns in tables.items():
        if table in shim_tables or native_derived_table(table):
            continue
        database = artwork if table == "blobs" else library
        target = {row[1] for row in database.execute(f"PRAGMA table_info({identifier(table)})")}
        if not target:
            raise ValueError(f"Native schema has no table {table}; refusing unsupported version/data loss")
        extras = {column for column, _ in columns} - target - derived_columns.get(table, set())
        if extras:
            raise ValueError(f"Unsupported native columns in {table}: {sorted(extras)}")
        if table == "metadata_items" and any(column == "subtype" for column, _ in columns):
            schema_literal = "'" + schema.replace("'", "''") + "'"
            generated, = list(postgres.query(
                "SELECT to_json(is_generated) FROM information_schema.columns "
                f"WHERE table_schema={schema_literal} AND table_name='metadata_items' AND column_name='subtype';"
            ))
            if generated != "ALWAYS":
                raise ValueError("Native schema cannot discard non-generated metadata_items.subtype")
        selected[table] = [(column, pg_type) for column, pg_type in columns if column in target]
    return selected


def check_outputs(output):
    for name in (LIBRARY, ARTWORK):
        path = output / name
        if path.is_symlink() or (path.exists() and not path.is_file()):
            raise ValueError(f"Refusing non-regular destination: {path}")
        for suffix in ("-wal", "-shm", "-journal"):
            sidecar = Path(str(path) + suffix)
            if os.path.lexists(sidecar):
                raise ValueError(f"Stop destination consumers and resolve SQLite sidecar: {sidecar}")


def promote(stage, output):
    check_outputs(output)
    promoted = []
    backups = {}
    for name in (LIBRARY, ARTWORK):
        destination = output / name
        if destination.exists():
            backup = stage / (name + ".previous")
            os.link(destination, backup)
            backups[name] = backup
        os.chmod(stage / name, 0o600)
        with (stage / name).open("rb") as source:
            os.fsync(source.fileno())
    directory = os.open(output, os.O_RDONLY)
    previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
    try:
        try:
            for name in (LIBRARY, ARTWORK):
                os.replace(stage / name, output / name)
                promoted.append(name)
            os.fsync(directory)
        except BaseException:
            for name in reversed(promoted):
                if name in backups:
                    os.replace(backups[name], output / name)
                else:
                    (output / name).unlink()
            os.fsync(directory)
            raise
    finally:
        os.close(directory)
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--schema", default=os.environ.get("PLEX_PG_SCHEMA", "plex"))
    parser.add_argument("--output-dir", type=Path, default=Path.cwd())
    parser.add_argument("--sqlite-schema", type=Path,
                        default=Path(__file__).resolve().parent.parent / "schema/sqlite_schema.sql")
    parser.add_argument("--native-plex-sqlite", type=Path,
                        help="Rebuild native virtual indices with this official Plex SQLite executable (server certification still required)")
    parser.add_argument("--yes", action="store_true", help="Replace existing inactive output files")
    parser.add_argument("--native-rollback", action="store_true",
                        help="Require certified native rollback (currently unsupported)")
    args = parser.parse_args()
    if args.native_rollback:
        raise ValueError("Native Plex rollback unsupported: no version-matched schema/extension certification")
    if args.native_plex_sqlite and not (args.native_plex_sqlite.is_file() and os.access(args.native_plex_sqlite, os.X_OK)):
        raise ValueError("Native Plex SQLite executable missing or not executable")
    identifier(args.schema)
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    directory = os.open(output, os.O_RDONLY)
    try:
        fcntl.flock(directory, fcntl.LOCK_EX | fcntl.LOCK_NB)
        check_outputs(output)
        if any((output / name).exists() for name in (LIBRARY, ARTWORK)) and not args.yes:
            if not sys.stdin.isatty():
                raise ValueError("Existing outputs preserved; use --yes to replace inactive files")
            if input("Replace existing data exports? [y/N]: ").lower() not in ("y", "yes"):
                print("Cancelled; existing outputs preserved.")
                return
        print("Native Plex rollback: UNCERTIFIED (Plex server must verify version/schema and restored behavior)." if args.native_plex_sqlite
              else "Native Plex rollback: UNSUPPORTED (data export; native indices omitted).", flush=True)
        with tempfile.TemporaryDirectory(prefix=".pg-to-sqlite-", dir=output) as temporary:
            stage = Path(temporary)
            library, artwork = create_destinations(stage, args.sqlite_schema, native=bool(args.native_plex_sqlite))
            with contextlib.closing(library), contextlib.closing(artwork), contextlib.closing(Postgres()) as postgres:
                postgres.execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; "
                                 "SET LOCAL standard_conforming_strings = on; "
                                 "SET LOCAL TIME ZONE 'UTC'; SET LOCAL DateStyle = 'ISO, YMD'; "
                                 "SET LOCAL extra_float_digits = 3; SET LOCAL row_security = off; "
                                 "SET LOCAL lock_timeout = '10s';")
                tables = catalog(postgres, args.schema)
                destination_names = ["pg_export_" + table if table.lower().startswith("sqlite_") else table
                                     for table in tables]
                if len({name.casefold() for name in destination_names}) != len(destination_names):
                    raise ValueError("Unsupported schema: SQLite destination table name collision")
                postgres.execute("LOCK TABLE " + ", ".join(
                    f"{identifier(args.schema)}.{identifier(table)}" for table in tables
                ) + " IN ACCESS SHARE MODE;")
                if catalog(postgres, args.schema) != tables:
                    raise ValueError("Source schema changed during snapshot setup")
                if args.native_plex_sqlite:
                    tables = native_catalog(postgres, args.schema, tables, library, artwork)
                for table, columns in tables.items():
                    export_table(postgres, args.schema, artwork if table == "blobs" else library, table, columns,
                                 native=bool(args.native_plex_sqlite))
                for database in (library, artwork):
                    database.commit()
                    if database.execute("PRAGMA foreign_key_check").fetchone() is not None:
                        raise ValueError("SQLite foreign key validation failed")
                    if database.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                        raise ValueError("SQLite integrity validation failed")
                postgres.execute("COMMIT;")
            if args.native_plex_sqlite:
                finalize_native(stage, args.sqlite_schema, args.native_plex_sqlite)
            promote(stage, output)
        print(f"Validated data exports published: {output / LIBRARY}\n{output / ARTWORK}")
        print("Native indices rebuilt; verify with the matching Plex server before rollback. Retain original SQLite backups." if args.native_plex_sqlite
              else "Do not install as native Plex databases; retain original SQLite backups.")
    finally:
        os.close(directory)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, sqlite3.Error, decimal.InvalidOperation, subprocess.TimeoutExpired) as error:
        print(f"ERROR: {error}; export not completed.", file=sys.stderr)
        sys.exit(1)

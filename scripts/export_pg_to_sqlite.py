#!/usr/bin/env python3
"""Lossless, snapshot-based data export using psql and Python's SQLite driver.

The bundled schema omits native Plex extensions/version certification. Outputs
are data exports only; --native-rollback fails closed until a version-matched
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


def schema_statements(path):
    pending = ""
    for line in path.read_text(encoding="utf-8").splitlines(keepends=True):
        pending += line
        if sqlite3.complete_statement(pending):
            statement = pending.strip()
            pending = ""
            if statement.startswith("CREATE VIRTUAL TABLE"):
                continue
            if not statement.startswith(("CREATE TABLE", "CREATE INDEX", "CREATE UNIQUE INDEX")):
                raise ValueError("Unsupported statement in SQLite schema")
            yield statement
    if pending.strip():
        raise ValueError("Incomplete SQLite schema statement")


def create_destinations(stage, schema_path):
    library = sqlite3.connect(stage / LIBRARY)
    artwork = sqlite3.connect(stage / ARTWORK)
    try:
        for statement in schema_statements(schema_path):
            library.execute(statement)
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


def column_mapping(database, table, columns):
    destination = database.execute(f"PRAGMA table_info({identifier(table)})").fetchall()
    target_types = {row[1]: row[2] for row in destination}
    source_names = {column for column, _ in columns}
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


def export_table(postgres, schema, database, table, columns):
    destination_table = "pg_export_" + table if table.lower().startswith("sqlite_") else table
    if destination_table != table:
        print(f"  {table}: SQLite reserved name; exported as {destination_table}", flush=True)
    mapping = column_mapping(database, destination_table, columns)
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
    parser.add_argument("--yes", action="store_true", help="Replace existing inactive output files")
    parser.add_argument("--native-rollback", action="store_true",
                        help="Require certified native rollback (currently unsupported)")
    args = parser.parse_args()
    if args.native_rollback:
        raise ValueError("Native Plex rollback unsupported: no version-matched schema/extension certification")
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
        print("Native Plex rollback: UNSUPPORTED (version schema and native extensions not certified).", flush=True)
        with tempfile.TemporaryDirectory(prefix=".pg-to-sqlite-", dir=output) as temporary:
            stage = Path(temporary)
            library, artwork = create_destinations(stage, args.sqlite_schema)
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
                for table, columns in tables.items():
                    export_table(postgres, args.schema, artwork if table == "blobs" else library, table, columns)
                for database in (library, artwork):
                    database.commit()
                    if database.execute("PRAGMA foreign_key_check").fetchone() is not None:
                        raise ValueError("SQLite foreign key validation failed")
                    if database.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                        raise ValueError("SQLite integrity validation failed")
                postgres.execute("COMMIT;")
            promote(stage, output)
        print(f"Validated data exports published: {output / LIBRARY}\n{output / ARTWORK}")
        print("Do not install as native Plex databases; retain original SQLite backups.")
    finally:
        os.close(directory)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, sqlite3.Error, decimal.InvalidOperation) as error:
        print(f"ERROR: {error}; export not completed.", file=sys.stderr)
        sys.exit(1)

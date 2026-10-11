#!/usr/bin/env python3
"""
Migrate a single table from SQLite to PostgreSQL using COPY protocol.

Avoids the CSV truncation bug in sqlite3 CLI where large TEXT fields
(>8KB with embedded quotes) get silently truncated during CSV export.

Usage: migrate_table.py <sqlite_db> <table> <select_expr> <pg_cols> <schema>
"""

import os
import sys
import sqlite3
import subprocess
import io
from pathlib import Path
from contextlib import ExitStack
import tempfile


def source_snapshot(database, stack):
    path = Path(database).resolve()
    if os.environ.get("MIGRATION_SQLITE_FROZEN") == "1":
        with path.open('rb') as handle:
            header = handle.read(100)
        if header[:16] != b'SQLite format 3\x00' or header[18:20] != b'\x01\x01' or any(
            Path(str(path) + suffix).exists() for suffix in ('-wal', '-shm', '-journal')
        ):
            raise RuntimeError('Expected private frozen DELETE-mode SQLite snapshot')
        return path
    temporary = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='plex-table-source-')))
    helper = Path(__file__).resolve().with_name('migrate_lib.sh')
    subprocess.run(['bash', '-c', 'source "$1"; snapshot_sqlite_sources "$2" "$3"',
                    'sqlite-source-snapshot', str(helper), str(path), str(temporary)], check=True)
    return temporary / 'library.db'


def main():
    if len(sys.argv) != 6:
        print(f"Usage: {sys.argv[0]} <sqlite_db> <table> <select_expr> <pg_cols> <schema>",
              file=sys.stderr)
        sys.exit(1)

    sqlite_db = sys.argv[1]
    table = sys.argv[2]
    select_expr = sys.argv[3]
    pg_cols = sys.argv[4]
    schema = sys.argv[5]

    with ExitStack() as stack:
        # Connect to SQLite
        frozen = source_snapshot(sqlite_db, stack)
        conn = sqlite3.connect(frozen.as_uri() + "?mode=ro", uri=True)
        conn.text_factory = str
        cur = conn.cursor()

        # Read rows streaming
        sql = f'SELECT {select_expr} FROM "{table}"'
        cur.execute(sql)

        first_row = cur.fetchone()
        if first_row is None:
            conn.close()
            sys.exit(0)

        # Stream to PostgreSQL via psql COPY FROM STDIN
        copy_cmd = f"COPY {schema}.\"{table}\"({pg_cols}) FROM STDIN"

        env = os.environ.copy()
        proc = subprocess.Popen(
            ["psql", "-q", "-c", copy_cmd],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )

        # Build tab-separated data for COPY
        # COPY uses \t as delimiter, \n as newline, \N for NULL
        # Backslashes in data must be escaped as \\
        batch_size = int(env.get("MIGRATE_BATCH_ROWS", "1000"))
        buf = io.StringIO()

        def write_row(row):
            fields = []
            for val in row:
                if val is None:
                    fields.append("\\N")
                elif isinstance(val, bytes):
                    # BLOB: encode as PostgreSQL bytea hex format
                    fields.append("\\\\x" + val.hex())
                elif isinstance(val, str):
                    # Escape backslashes, tabs, and newlines for COPY format
                    escaped = val.replace("\\", "\\\\")
                    escaped = escaped.replace("\t", "\\t")
                    escaped = escaped.replace("\n", "\\n")
                    escaped = escaped.replace("\r", "\\r")
                    fields.append(escaped)
                else:
                    fields.append(str(val))
            buf.write("\t".join(fields) + "\n")

        write_row(first_row)
        row_count = 1
        for row in cur:
            write_row(row)
            row_count += 1
            if row_count % batch_size == 0:
                if proc.stdin is not None:
                    proc.stdin.write(buf.getvalue().encode("utf-8"))
                    buf.seek(0)
                    buf.truncate(0)

        if proc.stdin is not None:
            if buf.tell() > 0:
                proc.stdin.write(buf.getvalue().encode("utf-8"))
            proc.stdin.close()
            proc.stdin = None

        conn.close()
        stdout, stderr = proc.communicate()

        if proc.returncode != 0:
            print(f"COPY failed for {table}: {stderr.decode()}", file=sys.stderr)
            sys.exit(1)


if __name__ == "__main__":
    main()

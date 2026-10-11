#!/usr/bin/env python3
"""Isolated genuine SQLite unique-index parity checks on PostgreSQL 15 and 18."""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
SPECS = [
    ('play_queues', 'idx_play_queues_client_account_type', ['client_identifier', 'account_id', 'metadata_type']),
    ('media_provider_resources', 'idx_media_provider_resources_uuid', ['uuid']),
    ('media_grabs', 'idx_media_grabs_uuid', ['uuid']),
    ('external_metadata_sources', 'idx_external_metadata_sources_uri', ['uri']),
    ('blobs', 'idx_blobs_linked_type_id_blob_type', ['linked_type', 'linked_id', 'blob_type']),
    ('blobs', 'idx_blobs_linked_type_guid_blob_type', ['linked_type', 'linked_guid', 'blob_type']),
    ('metadata_agent_providers', 'idx_metadata_agent_providers_identifier', ['identifier']),
    ('metadata_agent_provider_group_items', 'idx_metadata_agent_provider_group_items_pair',
     ['metadata_agent_provider_group_id', 'metadata_agent_provider_id']),
]


def run(command, *, env=None, stdin=None, ok=True):
    result = subprocess.run(command, env=env, input=stdin, text=True, capture_output=True)
    if (result.returncode == 0) != ok:
        raise AssertionError(f'{command[0]} unexpected exit {result.returncode}: {result.stderr}\n{result.stdout}')
    return result


def exercise(image):
    name = 'plex-unique-parity-' + uuid.uuid4().hex[:12]
    env = dict(os.environ, PGHOST='127.0.0.1', PGUSER='postgres', PGDATABASE='postgres',
               PGPASSWORD='isolated-unique-only', PG_SCHEMA='plex')
    run(['docker', 'run', '-d', '--name', name, '-e', 'POSTGRES_PASSWORD=isolated-unique-only',
         '-p', '127.0.0.1::5432', image])
    try:
        env['PGPORT'] = run(['docker', 'port', name, '5432']).stdout.strip().rsplit(':', 1)[1]
        deadline = time.monotonic() + 60
        while subprocess.run(['psql', '-X', '-qAt', '-c', 'SELECT 1'], env=env,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
            if time.monotonic() > deadline:
                raise AssertionError('PostgreSQL startup timed out')
            time.sleep(.25)

        def sql(statement, ok=True):
            return run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', '-c', statement], env=env, ok=ok)

        def upgrade(schema='parity_custom', ok=True):
            return run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', '-v', f'PG_SCHEMA={schema}',
                        '-f', str(ROOT / 'schema/sqlite_constraint_parity_upgrade.sql')], env=env, ok=ok)

        # Exercise actual full fresh schema, including PostgreSQL version adaptations.
        env.update(PG_PORT=env['PGPORT'], PG_HOST=env['PGHOST'], PG_USER=env['PGUSER'],
                   PG_DATABASE=env['PGDATABASE'])
        run(['bash', '-c', 'source "$1"; load_pg_schema_file "$2" "$3"', 'unique-test',
             str(ROOT / 'scripts/migrate_lib.sh'), str(ROOT / 'schema/plex_schema.sql'),
             str(ROOT / 'schema/sqlite_column_types.sql')], env=env)
        upgrade('plex')
        upgrade('plex')
        replace_env = dict(env, PLEX_REPLACE_TEST_PG_URL=f"postgres://postgres:isolated-unique-only@127.0.0.1:{env['PGPORT']}/postgres")
        run(['cargo', 'test', '--manifest-path', str(ROOT / 'rust/plex-pg-core/Cargo.toml'), '--lib',
             'source_schema__replace_differential_conflict_matrix', '--', '--ignored', '--nocapture'], env=replace_env)

        assert sql("SELECT count(*) FROM pg_indexes WHERE schemaname='plex' AND indexname IN (" +
                   ','.join("'%s'" % name for _, name, _ in SPECS) + ')').stdout.strip() == '8'

        # Use source table DDL plus all source indexes to verify actual UNIQUE declarations.
        source = (ROOT / 'schema/sqlite_schema.sql').read_text()
        sqlite = sqlite3.connect(':memory:')
        tables = sorted({table for table, _, _ in SPECS})
        sqlite.executescript('\n'.join(line for line in source.splitlines()
            if line.startswith(('CREATE TABLE', 'CREATE INDEX', 'CREATE UNIQUE INDEX'))
            and any(f'"{table}"' in line for table in tables)))
        for table, _, columns in SPECS:
            unique_keys = []
            for index in sqlite.execute(f'PRAGMA index_list("{table}")').fetchall():
                if index[2]:
                    unique_keys.append([row[2] for row in sqlite.execute(f'PRAGMA index_info("{index[1]}")')])
            assert columns in unique_keys, (table, columns, unique_keys)

        def create_custom():
            sql('DROP SCHEMA IF EXISTS parity_custom CASCADE; CREATE SCHEMA parity_custom;')
            for table in tables:
                columns = list(dict.fromkeys(column for target, _, cols in SPECS if target == table for column in cols))
                ddl = ', '.join(f'"{column}" ' + ('integer' if column.endswith('_id') or column in ('metadata_type', 'blob_type') else 'text')
                                for column in columns)
                sql(f'CREATE TABLE parity_custom.{table}(id serial PRIMARY KEY, extra_data text, {ddl});')
                # NULL-key duplicates must remain legal under default UNIQUE semantics.
                sql(f"INSERT INTO parity_custom.{table}(extra_data) VALUES('preserve-null'),('preserve-null');")

        def snapshot():
            return '\n'.join(sql(f'SELECT row_to_json(t) FROM parity_custom.{table} t ORDER BY id').stdout
                             for table in tables)

        create_custom()
        before = snapshot()
        upgrade()
        upgrade()
        assert before == snapshot()
        for table, _, columns in SPECS:
            values = ['7' if column.endswith('_id') or column in ('metadata_type', 'blob_type') else "'same'" for column in columns]
            cols = ','.join(columns)
            vals = ','.join(values)
            sql(f'INSERT INTO parity_custom.{table}({cols}) VALUES({vals});')
            sql(f'INSERT INTO parity_custom.{table}({cols}) VALUES({vals});', ok=False)
            before_count = sql(f'SELECT count(*) FROM parity_custom.{table}').stdout.strip()
            sql(f'INSERT INTO parity_custom.{table}({cols}) VALUES({vals}) ON CONFLICT DO NOTHING;')
            sql(f'INSERT INTO parity_custom.{table}(id) VALUES(1) ON CONFLICT DO NOTHING;')
            assert before_count == sql(f'SELECT count(*) FROM parity_custom.{table}').stdout.strip()
            # Exercise exactly the unambiguous natural targets used by the translator.
            sql(f"INSERT INTO parity_custom.{table}({cols},extra_data) VALUES({vals},'changed') ON CONFLICT({cols}) DO UPDATE SET extra_data=excluded.extra_data;")
            assert sql(f"SELECT count(*) FROM parity_custom.{table} WHERE extra_data='changed'").stdout.strip() == ('2' if table == 'blobs' and 'linked_guid' in columns else '1')
            # A PostgreSQL natural-key insert has the same unique-error contract as source SQLite.
            # Use source-required timestamps for metadata_agent_providers.
            sqlite_cols, sqlite_vals = cols, vals
            if table == 'metadata_agent_providers':
                sqlite_cols += ',created_at,updated_at'
                sqlite_vals += ',1,1'
            sqlite.execute(f'INSERT INTO "{table}"({sqlite_cols}) VALUES({sqlite_vals})')
            try:
                sqlite.execute(f'INSERT INTO "{table}"({sqlite_cols}) VALUES({sqlite_vals})')
            except sqlite3.IntegrityError:
                pass
            else:
                raise AssertionError(f'Source SQLite failed to enforce {table} {columns}')

        # Failure on the LAST candidate must roll back earlier index creations and retain data.
        create_custom()
        table, index_name, columns = SPECS[-1]
        sql(f'INSERT INTO parity_custom.{table}({",".join(columns)}) VALUES(7,7),(7,7);')
        before = snapshot()
        failed = upgrade(ok=False)
        assert 'duplicate non-NULL keys' in failed.stderr
        assert before == snapshot()
        assert sql("SELECT count(*) FROM pg_indexes WHERE schemaname='parity_custom' AND indexname LIKE 'idx_%'").stdout.strip() == '0'
        sql(f'DELETE FROM parity_custom.{table} WHERE id=(SELECT max(id) FROM parity_custom.{table});')
        sql(f'CREATE INDEX {index_name} ON parity_custom.{table}({columns[0]});')
        before = snapshot()
        failed = upgrade(ok=False)
        assert 'Refusing modified SQLite parity index' in failed.stderr
        assert before == snapshot()
        assert sql("SELECT count(*) FROM pg_indexes WHERE schemaname='parity_custom' AND indexname LIKE 'idx_%'").stdout.strip() == '1'
        sql(f'DROP INDEX parity_custom.{index_name}')
        upgrade()
        # Same columns but non-default NULL treatment must be rejected.
        sql(f'DELETE FROM parity_custom.{table} WHERE {columns[0]} IS NULL;')
        sql(f'DROP INDEX parity_custom.{index_name}; CREATE UNIQUE INDEX {index_name} ON parity_custom.{table}({",".join(columns)}) NULLS NOT DISTINCT;')
        assert 'Refusing modified' in upgrade(ok=False).stderr
        assert 'Invalid PG_SCHEMA' in upgrade('Bad-Schema', ok=False).stderr
        return {'image': image, 'server': sql('SELECT version()').stdout.strip(),
                'checks': ['full fresh schema', 'actual translated REPLACE differential matrix', 'follow-up generated identity',
                           'FK cascade + failed replacement preserves caller work', 'single-evaluation VALUES', 'prepared typed/NULL binds', 'eight source UNIQUE keys', 'SQLite/PG duplicate errors', 'natural ON CONFLICT updates', 'PK/natural IGNORE',
                           'NULL duplicates', 'nondefault schema', 'idempotence', 'all-row preservation',
                           'duplicate upgrade rollback', 'modified-index rollback', 'NULL treatment validation']}
    finally:
        run(['docker', 'rm', '-f', name])


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--image', action='append', default=[])
    args = parser.parse_args()
    for image in args.image or ['postgres:15', 'postgres:18']:
        print(json.dumps(exercise(image)), flush=True)

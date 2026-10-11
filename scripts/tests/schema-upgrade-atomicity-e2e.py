#!/usr/bin/env python3
"""Prove all startup schema parity upgrades share one transaction on PG15/18."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
UNIQUE = ROOT / 'schema/sqlite_constraint_parity_upgrade.sql'
FTS = ROOT / 'schema/fts_view_parity_upgrade.sql'
LIB = ROOT / 'scripts/migrate_lib.sh'


def run(command, env=None, ok=True):
    result = subprocess.run(command, env=env, text=True, capture_output=True)
    if (result.returncode == 0) != ok:
        raise AssertionError(f'{command[0]} unexpected exit {result.returncode}: {result.stderr}\n{result.stdout}')
    return result


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def exercise(image):
    name = 'plex-upgrade-atomic-' + uuid.uuid4().hex[:12]
    env = dict(os.environ, PGHOST='127.0.0.1', PGUSER='postgres', PGDATABASE='postgres',
               PGPASSWORD='isolated-schema-only', PG_SCHEMA='plex')
    run(['docker', 'run', '-d', '--name', name, '-e', 'POSTGRES_PASSWORD=isolated-schema-only',
         '-p', '127.0.0.1::5432', image])
    try:
        env['PGPORT'] = run(['docker', 'port', name, '5432']).stdout.strip().rsplit(':', 1)[1]
        deadline = time.monotonic() + 60
        while subprocess.run(['psql', '-X', '-qAt', '-c', 'SELECT 1'], env=env,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
            if time.monotonic() > deadline:
                raise AssertionError('Isolated PostgreSQL startup timed out')
            time.sleep(.25)

        def sql(statement):
            return run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', '-c', statement], env=env).stdout.strip()

        def helper(statement, *args, ok=True):
            return run(['bash', '-c', 'source "$1"; ' + statement, 'atomic-upgrade', str(LIB), *map(str, args)], env=env, ok=ok)

        helper('load_pg_schema_file "$2" "$3"', ROOT / 'schema/plex_schema.sql', ROOT / 'schema/sqlite_column_types.sql')
        spec = importlib.util.spec_from_file_location('unique_specs', ROOT / 'scripts/tests/unique-schema-parity-e2e.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        for _, index, _ in module.SPECS:
            sql(f'DROP INDEX plex.{index}')
        sql('''ALTER TABLE plex.metadata_item_settings ADD CONSTRAINT metadata_item_settings_account_guid_unique UNIQUE(account_id,guid);
            ALTER TABLE plex.statistics_bandwidth ADD CONSTRAINT statistics_bandwidth_account_id_device_id_timespan_at_lan_key UNIQUE(account_id,device_id,timespan,at,lan);
            INSERT INTO plex.metadata_item_settings(id,account_id,guid,extra_data) VALUES(41,2,'preserved','exact row');
            INSERT INTO plex.statistics_bandwidth(id,account_id,device_id,timespan,at,lan,bytes) VALUES(42,2,3,4,5,1,987);
            INSERT INTO plex.metadata_items(id,metadata_type,title,title_sort,original_title) VALUES(71,1,'original','sort','alternative');
            INSERT INTO plex.tags(id,tag) VALUES(72,'genre');
            DROP VIEW plex.fts4_metadata_titles,plex.fts4_metadata_titles_icu,plex.fts4_tag_titles,plex.fts4_tag_titles_icu;
            CREATE VIEW plex.fts4_metadata_titles AS SELECT id AS rowid,title,title_fts FROM plex.metadata_items;
            CREATE VIEW plex.fts4_metadata_titles_icu AS SELECT id AS rowid,title,title_fts FROM plex.metadata_items;
            CREATE VIEW plex.fts4_tag_titles AS SELECT id AS rowid,tag AS title,search_vector AS title_fts FROM plex.tags;
            CREATE VIEW plex.fts4_tag_titles_icu AS SELECT id AS rowid,tag AS title,search_vector AS title_fts FROM plex.tags;''')

        def schema_signature():
            return sql("SELECT conname||':'||pg_get_constraintdef(oid) FROM pg_constraint WHERE connamespace='plex'::regnamespace ORDER BY conname; "
                       "SELECT indexname||':'||indexdef FROM pg_indexes WHERE schemaname='plex' ORDER BY indexname; "
                       "SELECT viewname||':'||definition FROM pg_views WHERE schemaname='plex' ORDER BY viewname;")

        def row_signature():
            statements = []
            for table in sql("SELECT tablename FROM pg_tables WHERE schemaname='plex' ORDER BY tablename").splitlines():
                statements.append(f'SELECT {repr(table)}||\':\'||row_to_json(t)::text FROM plex."{table}" t ORDER BY row_to_json(t)::text;')
            return sql(' '.join(statements))

        with tempfile.TemporaryDirectory(prefix='plex-upgrade-atomicity-') as temporary:
            directory = Path(temporary)
            source = directory / 'unchanged-source.db'
            with sqlite3.connect(source) as connection:
                connection.execute('CREATE TABLE metadata_items(id INTEGER PRIMARY KEY,title TEXT)')
                connection.execute("INSERT INTO metadata_items VALUES(1,'source preserved')")
            connection.close()
            hashes = {path: digest(path) for path in (source, UNIQUE, FTS)}
            before_schema = schema_signature()
            before_rows = row_signature()
            # A duplicate in the final genuine target must roll back artificial
            # removals and seven earlier CREATE UNIQUE INDEX operations.
            sql('INSERT INTO plex.metadata_agent_provider_group_items(id,metadata_agent_provider_group_id,metadata_agent_provider_id) VALUES(1,7,8),(2,7,8)')
            duplicate_rows = row_signature()
            failed = helper('apply_sqlite_schema_parity_upgrades "$2" "$3"', UNIQUE, FTS, ok=False)
            assert 'duplicate non-NULL keys' in failed.stderr
            assert schema_signature() == before_schema
            assert row_signature() == duplicate_rows
            sql('DELETE FROM plex.metadata_agent_provider_group_items')
            assert row_signature() == before_rows
            last_table, last_index, _ = module.SPECS[-1]
            sql(f'CREATE INDEX {last_index} ON plex.{last_table}(metadata_agent_provider_group_id)')
            modified_schema = schema_signature()
            failed = helper('apply_sqlite_schema_parity_upgrades "$2" "$3"', UNIQUE, FTS, ok=False)
            assert 'Refusing modified SQLite parity index' in failed.stderr
            assert schema_signature() == modified_schema
            assert row_signature() == before_rows
            sql(f'DROP INDEX plex.{last_index}')
            broken_fts = directory / 'invalid-final-fts.sql'
            contents = FTS.read_text()
            left, match, right = contents.rpartition('    tags.tag\n')
            assert match
            broken_fts.write_text(left + '    tags.nonexistent_fts_column\n' + right)
            failed = helper('apply_sqlite_schema_parity_upgrades "$2" "$3"', UNIQUE, broken_fts, ok=False)
            assert 'nonexistent_fts_column' in failed.stderr
            assert schema_signature() == before_schema
            assert row_signature() == before_rows
            helper('apply_sqlite_schema_parity_upgrades "$2" "$3"', UNIQUE, directory / 'missing.sql', ok=False)
            assert schema_signature() == before_schema
            invalid_wrapper = directory / 'inner-commit.sql'
            invalid_wrapper.write_text(contents.replace('BEGIN;', 'BEGIN;\nCOMMIT;', 1))
            failed = helper('apply_sqlite_schema_parity_upgrades "$2" "$3"', UNIQUE, invalid_wrapper, ok=False)
            assert 'Unexpected top-level transaction control' in failed.stderr
            assert schema_signature() == before_schema
            assert row_signature() == before_rows
            helper('apply_sqlite_schema_parity_upgrades "$2" "$3"', UNIQUE, FTS)
            assert row_signature() == before_rows
            assert sql("SELECT count(*) FROM pg_constraint WHERE conname IN ('metadata_item_settings_account_guid_unique','statistics_bandwidth_account_id_device_id_timespan_at_lan_key')") == '0'
            index_names = ','.join("'" + index + "'" for _, index, _ in module.SPECS)
            assert sql(f'SELECT count(*) FROM pg_indexes WHERE schemaname=\'plex\' AND indexname IN ({index_names})') == '8'
            assert sql("SELECT count(*) FROM information_schema.columns WHERE table_schema='plex' AND table_name='fts4_metadata_titles'") == '5'
            successful_schema = schema_signature()
            helper('apply_sqlite_schema_parity_upgrades "$2" "$3"', UNIQUE, FTS)
            assert schema_signature() == successful_schema
            assert row_signature() == before_rows
            assert hashes == {path: digest(path) for path in hashes}
        return {'image': image, 'server_version': sql('SHOW server_version'), 'checks': [
            'final genuine-index duplicate rollback restores artificial constraints and earlier indices',
            'modified final genuine index fails with all earlier DDL rolled back',
            'invalid final FTS view rolls back artificial/genuine and earlier FTS changes',
            'missing file and extra transaction-control validation fail before mutation',
            'combined success/idempotence preserves every table row and source/SQL hashes',
        ]}
    finally:
        run(['docker', 'rm', '-f', name])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', action='append', required=True)
    args = parser.parse_args()
    for image in args.image:
        print(json.dumps(exercise(image), indent=2), flush=True)


if __name__ == '__main__':
    main()

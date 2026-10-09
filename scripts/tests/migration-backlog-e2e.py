#!/usr/bin/env python3
"""Isolated PostgreSQL/SQLite regression evidence for migration backlog.

Uses only Python stdlib, Docker, psql and sqlite3. Never connects to an existing
PostgreSQL instance: each requested image gets a temporary container/database.
Run: PATH=/opt/homebrew/opt/libpq/bin:$PATH python3 scripts/tests/migration-backlog-e2e.py --image postgres:15 --image postgres:18
"""
import argparse
import hashlib
import importlib.util
import json
import os
import re
import selectors
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import time
import threading
import uuid

ROOT = Path(__file__).resolve().parents[2]
LIB = ROOT / 'scripts/migrate_lib.sh'


def run(command, *, env=None, stdin=None, ok=True):
    result = subprocess.run(command, input=stdin, text=True, capture_output=True, env=env)
    if ok and result.returncode:
        raise AssertionError(f'{command[0]} failed: {result.stderr}\n{result.stdout}')
    if not ok and not result.returncode:
        raise AssertionError(f'{command[0]} unexpectedly succeeded')
    return result


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def exercise(image, native_image=None):
    name = 'plex-migration-test-' + uuid.uuid4().hex[:12]
    env = dict(os.environ, PGUSER='postgres', PGDATABASE='postgres', PGHOST='127.0.0.1',
               PGPASSWORD='isolated-test-only', PLEX_PG_PASSWORD='isolated-test-only', PG_SCHEMA='plex', PG_USER='postgres',
               PG_HOST='127.0.0.1', PG_DATABASE='postgres', SHIM_DIR=str(ROOT / 'scripts'),
               MIGRATION_INTERACTIVE='0')
    checks = []
    run(['docker', 'run', '-d', '--name', name, '-e', 'POSTGRES_PASSWORD=isolated-test-only',
         '-p', '127.0.0.1::5432', image])
    try:
        port = run(['docker', 'port', name, '5432']).stdout.strip().rsplit(':', 1)[1]
        env.update(PGPORT=port, PG_PORT=port)
        deadline = time.monotonic() + 60
        while subprocess.run(['psql', '-X', '-qAt', '-c', 'SELECT 1'], env=env,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
            if time.monotonic() > deadline:
                raise AssertionError('isolated PostgreSQL startup timed out')
            time.sleep(.25)

        def sql(statement, ok=True):
            return run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', '-c', statement], env=env, ok=ok).stdout.strip()

        def shell(statement, ok=True, extra=None):
            return run(['bash', '-c', 'source "$1"; ' + statement, 'migration-test', str(LIB)],
                       env=dict(env, **(extra or {})), ok=ok)

        shell('load_pg_schema_file "$SHIM_DIR/../schema/plex_schema.sql" "$SHIM_DIR/../schema/sqlite_column_types.sql"')
        assert sql("SELECT count(*) FROM pg_extension WHERE extname='pg_trgm'") == '1'
        sql('CREATE DATABASE migration_trgm_placement')
        env['PGDATABASE'] = 'migration_trgm_placement'
        sql('CREATE EXTENSION pg_trgm WITH SCHEMA public')
        shell('load_pg_schema_file "$SHIM_DIR/../schema/plex_schema.sql" "$SHIM_DIR/../schema/sqlite_column_types.sql"')
        assert sql("SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='pg_trgm'") == 'public'
        env['PGDATABASE'] = 'postgres'
        checks.append('full fresh schema + pg_trgm bootstrap/existing public extension placement')
        assert sql("SELECT count(*) FROM pg_constraint WHERE conname IN ('metadata_item_settings_account_guid_unique', 'statistics_bandwidth_account_id_device_id_timespan_at_lan_key')") == '0'
        # Recreate older-release constraints and populated rows to exercise upgrade.
        sql('''INSERT INTO plex.metadata_item_settings(id,account_id,guid,extra_data) VALUES(41,2,'same','preserve');
            INSERT INTO plex.statistics_bandwidth(id,account_id,device_id,timespan,at,lan,bytes) VALUES(42,2,3,4,5,1,987);
            ALTER TABLE plex.metadata_item_settings ADD CONSTRAINT metadata_item_settings_account_guid_unique UNIQUE(account_id,guid);
            ALTER TABLE plex.statistics_bandwidth ADD CONSTRAINT statistics_bandwidth_account_id_device_id_timespan_at_lan_key UNIQUE(account_id,device_id,timespan,at,lan);
            ALTER TABLE plex.metadata_item_settings ADD CONSTRAINT administrator_rating UNIQUE(rating);''')
        before = sql("SELECT row_to_json(t) FROM plex.metadata_item_settings t; SELECT row_to_json(t) FROM plex.statistics_bandwidth t;")
        shell('apply_sqlite_constraint_parity_upgrade; apply_sqlite_constraint_parity_upgrade')
        assert before == sql("SELECT row_to_json(t) FROM plex.metadata_item_settings t; SELECT row_to_json(t) FROM plex.statistics_bandwidth t;")
        assert sql("SELECT count(*) FROM pg_constraint WHERE conname IN ('metadata_item_settings_pkey','statistics_bandwidth_pkey','administrator_rating')") == '3'
        sql("INSERT INTO plex.metadata_item_settings(id,account_id,guid) VALUES(43,2,'same'); INSERT INTO plex.statistics_bandwidth(id,account_id,device_id,timespan,at,lan) VALUES(44,2,3,4,5,1);")
        sql("INSERT INTO plex.metadata_item_settings(id) VALUES(41)", ok=False)
        checks.append('populated idempotent constraint upgrade, duplicates, genuine keys, exact rows')
        env['PG_SCHEMA'] = 'migration_custom'
        sql('''CREATE SCHEMA migration_custom;
            CREATE TABLE migration_custom.metadata_item_settings(id serial PRIMARY KEY, account_id int, guid text, rating int,
              CONSTRAINT metadata_item_settings_account_guid_unique UNIQUE(account_id,guid));
            CREATE TABLE migration_custom.statistics_bandwidth(id serial PRIMARY KEY, account_id int, device_id int, timespan int, at bigint, lan int,
              CONSTRAINT statistics_bandwidth_account_id_device_id_timespan_at_lan_key UNIQUE(device_id));''')
        shell('apply_sqlite_constraint_parity_upgrade', ok=False)
        assert sql("SELECT count(*) FROM pg_constraint c JOIN pg_namespace n ON n.oid=c.connamespace WHERE n.nspname='migration_custom' AND c.contype='u'") == '2'
        sql('ALTER TABLE migration_custom.statistics_bandwidth DROP CONSTRAINT statistics_bandwidth_account_id_device_id_timespan_at_lan_key; ALTER TABLE migration_custom.statistics_bandwidth ADD CONSTRAINT statistics_bandwidth_account_id_device_id_timespan_at_lan_key UNIQUE(account_id,device_id,timespan,at,lan);')
        shell('apply_sqlite_constraint_parity_upgrade; apply_sqlite_constraint_parity_upgrade')
        checks.append('nondefault-schema upgrade + changed constraint transactional rollback')
        sql('''CREATE TABLE migration_custom.devices(id serial PRIMARY KEY); INSERT INTO migration_custom.devices(id) VALUES(15192);
            CREATE TABLE migration_custom.empty_custom(id serial PRIMARY KEY); ALTER SEQUENCE migration_custom.empty_custom_id_seq START WITH 37;
            CREATE TABLE migration_custom.negative(id serial PRIMARY KEY); INSERT INTO migration_custom.negative(id) VALUES(-4);
            CREATE TABLE migration_custom.identity_rows(id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY); INSERT INTO migration_custom.identity_rows(id) VALUES(812);
            CREATE TABLE migration_custom.at_max(id smallserial PRIMARY KEY); INSERT INTO migration_custom.at_max(id) VALUES(32767);
            CREATE TABLE migration_custom.blobs(id serial PRIMARY KEY, blob bytea); INSERT INTO migration_custom.blobs(id,blob) VALUES(901,decode('00ff5c','hex'));''')
        shell('sync_all_sequences')
        assert sql('INSERT INTO migration_custom.devices DEFAULT VALUES RETURNING id') == '15193'
        assert sql('INSERT INTO migration_custom.empty_custom DEFAULT VALUES RETURNING id') == '37'
        assert sql('INSERT INTO migration_custom.negative DEFAULT VALUES RETURNING id') == '1'
        assert sql('INSERT INTO migration_custom.identity_rows DEFAULT VALUES RETURNING id') == '813'
        assert sql('INSERT INTO migration_custom.blobs DEFAULT VALUES RETURNING id') == '902'
        sql('INSERT INTO migration_custom.at_max DEFAULT VALUES', ok=False)
        shell('sync_all_sequences', ok=False, extra={'PGPORT': '1'})
        sql('CREATE SEQUENCE migration_custom.unowned_seq; CREATE TABLE migration_custom.unowned(id int DEFAULT nextval(\'migration_custom.unowned_seq\'));')
        shell('sync_all_sequences', ok=False)
        sql('DROP TABLE migration_custom.unowned; DROP SEQUENCE migration_custom.unowned_seq')
        checks.append('all sequences: devices 15192, custom start, negative, identity, blobs, max failure, PG failure, missing ownership failure')
        with tempfile.TemporaryDirectory(prefix='.plex-migration-backlog-', dir=ROOT) as temporary:
            temp = Path(temporary)
            shadow = temp / 'shadow.db'
            with sqlite3.connect(shadow) as connection:
                connection.execute('CREATE TABLE schema_migrations(version TEXT UNIQUE)')
                connection.executemany('INSERT INTO schema_migrations VALUES(?)', [('stale1',), ('stale2',)])
            sql('CREATE TABLE migration_custom.schema_migrations(version text PRIMARY KEY)')
            versions = ["unicode雪'\\quoted", 'second\nline']
            values = ','.join("('" + value.replace("'", "''") + "')" for value in versions)
            sql('INSERT INTO migration_custom.schema_migrations VALUES ' + values)
            shell('sync_shadow_migrations "$TEST_SHADOW"', extra={'TEST_SHADOW': str(shadow)})
            with sqlite3.connect(shadow) as connection:
                assert sorted(row[0] for row in connection.execute('SELECT version FROM schema_migrations')) == sorted(versions)
                connection.execute("CREATE TRIGGER reject_marker BEFORE INSERT ON schema_migrations BEGIN SELECT RAISE(ABORT,'injected insert failure'); END")
            old_hash = digest(shadow)
            shell('sync_shadow_migrations "$TEST_SHADOW"', ok=False, extra={'TEST_SHADOW': str(shadow)})
            assert digest(shadow) == old_hash
            shell('sync_shadow_migrations "$TEST_SHADOW"', ok=False, extra={'TEST_SHADOW': str(shadow), 'PGPORT': '1'})
            assert digest(shadow) == old_hash
            with sqlite3.connect(shadow) as connection:
                connection.execute('DROP TRIGGER reject_marker')
            sql('DELETE FROM migration_custom.schema_migrations')
            shell('sync_shadow_migrations "$TEST_SHADOW"', extra={'TEST_SHADOW': str(shadow)})
            with sqlite3.connect(shadow) as connection:
                assert connection.execute('SELECT count(*) FROM schema_migrations').fetchone() == (0,)
            checks.append('shadow exact replacement: equal counts, Unicode, quotes, slashes, newline, empty, read/insert failure byte preservation')
            env['PG_SCHEMA'] = 'plex'
            sql('TRUNCATE plex.metadata_item_settings, plex.statistics_bandwidth')
            if native_image:
                readonly = temp / 'readonly-wal'
                readonly.mkdir()
                writable_snapshot = temp / 'writable-snapshot'
                writable_snapshot.mkdir()
                wal_source = readonly / 'library.db'
                with sqlite3.connect(wal_source) as connection:
                    connection.execute('PRAGMA journal_mode=WAL')
                    connection.execute('CREATE TABLE metadata_items(id INTEGER PRIMARY KEY,title TEXT)')
                    connection.execute("INSERT INTO metadata_items VALUES(1,'stopped WAL')")
                connection.close()
                def directory_hashes(directory):
                    return {path.name: digest(path) for path in directory.iterdir() if path.is_file()}
                def mount_snapshot(ok=True):
                    return run(['docker', 'run', '--rm', '--network', 'none',
                        '-v', str(ROOT) + ':/repo:ro', '-v', str(readonly) + ':/source:ro',
                        '-v', str(writable_snapshot) + ':/dest', '--entrypoint', '/bin/bash', native_image,
                        '-c', 'source /repo/scripts/migrate_lib.sh; snapshot_sqlite_sources /source/library.db /dest || exit $?; sqlite3 -readonly /dest/library.db "SELECT count(*) FROM metadata_items; PRAGMA journal_mode;"'], ok=ok)
                before_wal = directory_hashes(readonly)
                assert not (readonly / 'library.db-wal').exists()
                assert mount_snapshot().stdout.strip() == '1\ndelete'
                assert before_wal == directory_hashes(readonly)
                held = sqlite3.connect(wal_source)
                held.execute("INSERT INTO metadata_items VALUES(2,'committed WAL')")
                held.commit()
                assert (readonly / 'library.db-wal').stat().st_size > 0
                before_wal = directory_hashes(readonly)
                assert mount_snapshot().stdout.strip() == '2\ndelete'
                assert before_wal == directory_hashes(readonly)
                # Direct table export snapshots even when the CLI is called
                # outside the full importer, and reads committed WAL content.
                sql('CREATE SCHEMA wal_single_table; CREATE TABLE wal_single_table.metadata_items(id INTEGER PRIMARY KEY,title TEXT)')
                run(['python3', str(ROOT / 'scripts/migrate_table.py'), str(wal_source), 'metadata_items',
                     'id,title', 'id,title', 'wal_single_table'], env=dict(env))
                assert sql('SELECT count(*) FROM wal_single_table.metadata_items') == '2'
                assert before_wal == directory_hashes(readonly)
                sql('DROP SCHEMA wal_single_table CASCADE')

                # Missing SHM is recovered only in the private writable copy.
                saved_shm = readonly / 'library.db-shm'
                saved_shm.rename(readonly / 'saved-shm')
                before_wal = directory_hashes(readonly)
                assert mount_snapshot().stdout.strip() == '2\ndelete'
                assert before_wal == directory_hashes(readonly)
                (readonly / 'saved-shm').rename(saved_shm)
                held.execute('CREATE TABLE write_counter(value INTEGER NOT NULL)')
                held.execute('INSERT INTO write_counter VALUES(0)')
                held.commit()
                stop_writer = threading.Event()
                writer_ready = threading.Event()
                committed = [0]
                def writer():
                    with sqlite3.connect(wal_source) as connection:
                        while not stop_writer.is_set():
                            connection.execute('UPDATE write_counter SET value=value+1')
                            connection.commit()
                            committed[0] += 1
                            writer_ready.set()
                            time.sleep(.002)
                active = threading.Thread(target=writer)
                active.start()
                writer_ready.wait(timeout=5)
                for path in writable_snapshot.iterdir():
                    path.unlink()
                try:
                    changed = mount_snapshot(ok=False)
                    assert 'source' in changed.stderr and 'changed' in changed.stderr, changed.stderr
                    assert not list(writable_snapshot.iterdir())
                finally:
                    stop_writer.set()
                    active.join(timeout=5)
                assert not active.is_alive()
                assert held.execute('SELECT value FROM write_counter').fetchone() == (committed[0],)
                held.close()
                native_schema = run(['docker', 'run', '--rm', '-i', '--network', 'none',
                    '-v', str(readonly) + ':/source', '--entrypoint', '/usr/lib/plexmediaserver/Plex SQLite',
                    native_image, '/source/library.db'], stdin=
                    ".bail on\nCREATE TABLE collated_titles(word TEXT COLLATE icu_root);\n"
                    "CREATE INDEX collated_title_index ON collated_titles(word COLLATE icu_root);\n"
                    "INSERT INTO collated_titles VALUES('Snow'),('snow');\nPRAGMA integrity_check('collated_titles');\n")
                assert native_schema.stdout.strip() == 'ok'
                before_native = directory_hashes(readonly)
                assert mount_snapshot().stdout.strip() == '2\ndelete'
                assert before_native == directory_hashes(readonly)
                inspection = sqlite3.connect(wal_source)
                rootpage = inspection.execute("SELECT rootpage FROM sqlite_master WHERE name='collated_titles'").fetchone()[0]
                inspection.close()
                with wal_source.open('r+b') as damaged:
                    header = damaged.read(100)
                    pagesize = int.from_bytes(header[16:18], 'big')
                    pagesize = 65536 if pagesize == 1 else pagesize
                    damaged.seek((rootpage - 1) * pagesize)
                    damaged.write(b'\xff')
                corrupted_hashes = directory_hashes(readonly)
                invalid = mount_snapshot(ok=False)
                assert 'malformed' in invalid.stderr, invalid.stderr
                assert corrupted_hashes == directory_hashes(readonly)
                assert not list(writable_snapshot.iterdir())
                checks.append('native icu_root collation/index private snapshot validation; physical corruption fails closed and preserves original hashes')
                checks.append('real read-only Docker mount: stopped WAL header, committed WAL with/missing SHM, exact source hashes, changing-writer pair rejection without publication/source writes')
            source = temp / 'library.db'
            blob_source = temp / 'library.blobs.db'
            title = '雪"\\\n' + 't' * 8192
            guid = 'plex://' + 'g' * 512
            extra_data = json.dumps({'payload': title}, ensure_ascii=False)
            accounts_ddl = next(line for line in (ROOT / 'schema/sqlite_schema.sql').read_text().splitlines()
                                if line.startswith('CREATE TABLE IF NOT EXISTS "accounts"'))
            sql('CREATE TABLE plex.preference_boolean_test(id INTEGER PRIMARY KEY,enabled BOOLEAN,active BOOLEAN,nullable BOOLEAN)')
            with sqlite3.connect(source) as connection:
                connection.execute(accounts_ddl)
                connection.execute("INSERT INTO accounts(id,name) VALUES(4,'native textual defaults')")
                assert connection.execute('SELECT typeof(auto_select_subtitle),typeof(auto_select_audio) FROM accounts WHERE id=4').fetchone() == ('text','text')
                connection.executemany('INSERT INTO accounts(id,name,auto_select_subtitle,auto_select_audio) VALUES(?,?,?,?)',
                    [(5,'false','f',0),(6,'null',None,None),(7,'integer true',1,1),(8,'integer false',0,'false'),(9,'integer beyond logical bool',2,-7)])
                connection.execute('CREATE TABLE preference_boolean_test(id INTEGER PRIMARY KEY,enabled BOOLEAN,active INTEGER,nullable REAL)')
                connection.executemany('INSERT INTO preference_boolean_test VALUES(?,?,?,?)', [(1,'t',1,None),(2,'f',0,1.0),(3,None,0,0.0)])
                connection.execute('CREATE TABLE metadata_items(id INTEGER PRIMARY KEY, metadata_type INTEGER, title TEXT, guid TEXT, extra_data TEXT)')
                connection.execute('INSERT INTO metadata_items VALUES(?,?,?,?,?)', (71, 1, title, guid, extra_data))
                connection.execute('CREATE TABLE preferences(id INTEGER PRIMARY KEY,name TEXT,value TEXT)')
                connection.execute("INSERT INTO preferences VALUES(2,'SyncedNeedsChangedAtUpdate','1')")
                for table in ('activities','metadata_agent_providers','metadata_agent_provider_groups','metadata_agent_provider_group_items','plugin_prefixes','plugins'):
                    connection.execute(f'CREATE TABLE {table}(id INTEGER PRIMARY KEY)')
                connection.execute('CREATE TABLE devices(id INTEGER PRIMARY KEY, name TEXT)')
                connection.execute('INSERT INTO devices VALUES(15192,?)', ('imported',))
                for table in ('media_parts', 'media_items', 'metadata_item_settings', 'tags'):
                    connection.execute(f'CREATE TABLE {table}(id INTEGER PRIMARY KEY, extra_data TEXT)')
            with sqlite3.connect(blob_source) as connection:
                connection.execute('CREATE TABLE blobs(id INTEGER PRIMARY KEY, linked_type TEXT, linked_id INTEGER, linked_guid TEXT, created_at INTEGER, blob_type INTEGER, blob BLOB)')
                connection.execute('INSERT INTO blobs VALUES(?,?,?,?,?,?,?)', (902, 'art', 71, guid, 123, 1, bytes(range(256))))
            with sqlite3.connect(source) as connection:
                connection.execute('PRAGMA journal_mode=WAL')
            with sqlite3.connect(blob_source) as connection:
                connection.execute('PRAGMA journal_mode=WAL')
            hashes = [digest(source), digest(blob_source)]
            import_env = {'SQLITE_DB': str(source), 'LOG_DIR': str(temp / 'logs')}
            seed_file = ROOT / 'schema/seed_data.sql'
            shell('migration_psql -1 -q -f "$SEED_FILE"', extra={'SEED_FILE': str(seed_file)})
            seeded_tables = ['accounts','activities','devices','metadata_agent_providers','metadata_agent_provider_groups',
                             'metadata_agent_provider_group_items','plugin_prefixes','plugins','preferences','tags']
            assert sql('SELECT count(*) FROM plex.schema_migrations') == '445'
            assert sum(int(sql(f'SELECT count(*) FROM plex.{table}')) for table in seeded_tables) == 31
            def seed_rows():
                return sql(' '.join(f'SELECT row_to_json(t) FROM plex.{table} t ORDER BY id;' for table in seeded_tables))
            def seed_structure():
                return sql("SELECT conname||pg_get_constraintdef(oid) FROM pg_constraint WHERE connamespace='plex'::regnamespace ORDER BY conname; "
                           "SELECT tgname||pg_get_triggerdef(t.oid) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='plex' ORDER BY c.relname,tgname;")
            seed_before = seed_rows()
            structure_before = seed_structure()
            shell('destination_has_data; test "$?" = 1')
            shell('destination_guard_sql | migration_psql -1 -q')
            mutations = [
                ("UPDATE plex.accounts SET name='custom administrator' WHERE id=1", "UPDATE plex.accounts SET name='Administrator' WHERE id=1"),
                ("UPDATE plex.preferences SET value='1' WHERE id=2", "UPDATE plex.preferences SET value='0' WHERE id=2"),
                ("UPDATE plex.plugins SET access_count=42 WHERE id=1", "UPDATE plex.plugins SET access_count=NULL WHERE id=1"),
                ("UPDATE plex.plugin_prefixes SET prefs=0 WHERE id=3", "UPDATE plex.plugin_prefixes SET prefs=1 WHERE id=3"),
                ("UPDATE plex.devices SET name='actual user device' WHERE id=1", "UPDATE plex.devices SET name='' WHERE id=1"),
                ("UPDATE plex.activities SET subtitle='actual activity' WHERE id=1", "UPDATE plex.activities SET subtitle='' WHERE id=1"),
                ("UPDATE plex.metadata_agent_providers SET online=0 WHERE id=1", "UPDATE plex.metadata_agent_providers SET online=1 WHERE id=1"),
                ("UPDATE plex.metadata_agent_provider_groups SET title='custom title' WHERE id=1", "UPDATE plex.metadata_agent_provider_groups SET title='Plex Movie' WHERE id=1"),
                ("UPDATE plex.metadata_agent_provider_group_items SET \"order\"=12 WHERE id=1", "UPDATE plex.metadata_agent_provider_group_items SET \"order\"=1000 WHERE id=1"),
                ("UPDATE plex.tags SET extra_data='custom data' WHERE id=1", ""),
            ]
            tag_original = sql('SELECT extra_data FROM plex.tags WHERE id=1')
            mutations[-1] = (mutations[-1][0], "UPDATE plex.tags SET extra_data='" + tag_original.replace("'", "''") + "' WHERE id=1")
            for change, restore in mutations:
                sql(change)
                changed_before = seed_rows()
                shell('destination_has_data; test "$?" = 0')
                shell('destination_guard_sql | migration_psql -1 -q', ok=False)
                shell('migrate_sqlite_to_pg', ok=False, extra=import_env)
                assert changed_before == seed_rows()
                assert structure_before == seed_structure()
                assert hashes == [digest(source),digest(blob_source)]
                sql(restore)
                assert seed_before == seed_rows()
            for insert, cleanup, table in (
                ("INSERT INTO plex.accounts(id,name) VALUES(999,'real additional account')", 'DELETE FROM plex.accounts WHERE id=999', 'accounts'),
                ("INSERT INTO plex.preferences(id,name,value) VALUES(999,'actual user preference','custom')", 'DELETE FROM plex.preferences WHERE id=999', 'preferences'),
            ):
                sql(insert)
                changed_before = seed_rows()
                shell('destination_has_data; test "$?" = 0')
                shell('destination_guard_sql | migration_psql -1 -q', ok=False)
                shell('migrate_sqlite_to_pg', ok=False, extra=import_env)
                assert changed_before == seed_rows()
                assert structure_before == seed_structure()
                assert hashes == [digest(source),digest(blob_source)]
                sql(cleanup)
            sql("ALTER TABLE plex.tags DISABLE TRIGGER tags_search_update; UPDATE plex.tags SET search_vector=to_tsvector('simple','custom vector') WHERE id=1; ALTER TABLE plex.tags ENABLE TRIGGER tags_search_update;")
            changed_before = seed_rows()
            shell('destination_has_data; test "$?" = 0')
            shell('destination_guard_sql | migration_psql -1 -q', ok=False)
            assert changed_before == seed_rows()
            sql('UPDATE plex.tags SET tag=tag WHERE id=1')
            assert seed_before == seed_rows()
            # Verify the activation guard sees a writer committed while it waits
            # for its destination locks, rather than retaining a stale snapshot.
            guard_file = temp / 'locked-bootstrap-guard.sql'
            guard_file.write_text(shell('destination_guard_sql').stdout + '\nDELETE FROM plex.accounts;\n')
            writer = subprocess.Popen(['psql','-X','-qAt','-v','ON_ERROR_STOP=1'], env=env, stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
            guard = None
            try:
                writer.stdin.write("BEGIN; LOCK TABLE plex.accounts IN ACCESS EXCLUSIVE MODE; UPDATE plex.preferences SET value='1' WHERE id=2;\n\\echo WRITER_READY\n")
                writer.stdin.flush()
                ready = selectors.DefaultSelector()
                ready.register(writer.stdout,selectors.EVENT_READ)
                assert ready.select(timeout=5), 'writer did not acquire bootstrap lock'
                assert writer.stdout.readline().strip() == 'WRITER_READY'
                ready.close()
                guard = subprocess.Popen(['psql','-X','-qAt','-v','ON_ERROR_STOP=1','-1','-f',str(guard_file)],
                                         env=dict(env,PGAPPNAME='issue24-activation-guard'),stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
                deadline = time.monotonic()+5
                while sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='issue24-activation-guard' AND wait_event_type='Lock'") != '1':
                    assert time.monotonic() < deadline, 'activation did not wait on writer lock'
                    time.sleep(.02)
                writer.stdin.write('COMMIT;\n\\q\n')
                writer.stdin.flush()
                writer.communicate(timeout=5)
                _, errors = guard.communicate(timeout=5)
                assert guard.returncode != 0 and 'Destination changed or contains data: preferences' in errors, errors
                assert sql('SELECT count(*) FROM plex.accounts') == '1'
                assert sql('SELECT value FROM plex.preferences WHERE id=2') == '1'
            finally:
                for process in (guard,writer):
                    if process is not None and process.poll() is None:
                        process.kill()
                        process.communicate(timeout=5)
            sql("UPDATE plex.preferences SET value='0' WHERE id=2")
            assert seed_rows() == seed_before
            checks.append('issue24 exact full31-row/10-table repository bootstrap accepted; changed field in each table rejects without changing rows/constraints/triggers/source hashes')
            checks.append('issue24 locked activation catches concurrent writer commit and preserves its preference plus Administrator')
            # Reproduce the report's exact account/preferences-only initialized
            # destination and prove a complete source import, not only detection.
            sql('CREATE DATABASE issue24_subset')
            env.update(PGDATABASE='issue24_subset',PG_DATABASE='issue24_subset')
            shell('load_pg_schema_file "$SHIM_DIR/../schema/plex_schema.sql" "$SHIM_DIR/../schema/sqlite_column_types.sql"')
            sql('CREATE TABLE plex.preference_boolean_test(id INTEGER PRIMARY KEY,enabled BOOLEAN,active BOOLEAN,nullable BOOLEAN)')
            subset = ''
            pending = ''
            for line in seed_file.read_text().splitlines(keepends=True):
                pending += line
                if sqlite3.complete_statement(pending):
                    statement = re.sub(r'(?m)^\s*--[^\n]*','',pending).strip()
                    pending = ''
                    if re.match(r'^INSERT INTO plex\.(accounts|preferences)\b',statement):
                        subset += statement + '\n'
            sql(subset)
            assert sql("SELECT (SELECT count(*) FROM plex.accounts)||','||(SELECT count(*) FROM plex.preferences)||','||(SELECT count(*) FROM plex.schema_migrations)") == '1,1,445'
            shell('check_and_migrate', extra=import_env)
            assert sql('SELECT count(*) FROM plex.metadata_items') == '1'
            assert sql('SELECT value FROM plex.preferences WHERE id=2') == '1'
            assert hashes == [digest(source),digest(blob_source)]
            env.update(PGDATABASE='postgres',PG_DATABASE='postgres')
            checks.append('issue24 accounts1/preferences1/schema_migrations445 subset imports successfully on actual full PG schema and keeps source hashes')
            shell('check_and_migrate', extra=import_env)
            assert hashes == [digest(source), digest(blob_source)]
            values = json.loads(sql('SELECT json_build_array(title,guid,extra_data) FROM plex.metadata_items WHERE id=71'))
            assert values == [title, guid, extra_data]
            assert json.loads(sql('SELECT json_agg(json_build_array(id,auto_select_subtitle,auto_select_audio) ORDER BY id) FROM plex.accounts')) == [[4,1,1],[5,0,0],[6,None,None],[7,1,1],[8,0,0],[9,2,-7]]
            assert json.loads(sql('SELECT json_agg(json_build_array(id,enabled,active,nullable) ORDER BY id) FROM plex.preference_boolean_test')) == [[1,True,True,None],[2,False,False,True],[3,None,False,False]]
            # Audit every declared native Boolean against actual PG destination
            # types, including fields beyond the reported accounts preferences.
            template = sqlite3.connect(':memory:')
            pending = ''
            for line in (ROOT / 'schema/sqlite_schema.sql').read_text().splitlines(keepends=True):
                pending += line
                if sqlite3.complete_statement(pending):
                    statement = pending.strip()
                    pending = ''
                    if not statement.startswith('CREATE VIRTUAL TABLE'):
                        template.execute(statement)
            boolean_columns = []
            for (table,) in template.execute("SELECT name FROM sqlite_master WHERE type='table'"):
                for column in template.execute(f'PRAGMA table_info("{table}")'):
                    if column[2].upper() in ('BOOLEAN','BOOL'):
                        boolean_columns.append((table,column[1]))
            template.close()
            native_pg_types = {tuple(row[:2]): row[2] for row in map(json.loads, sql("SELECT json_build_array(table_name,column_name,udt_name) FROM information_schema.columns WHERE table_schema='plex'").splitlines())}
            assert len(boolean_columns) >= 20
            assert all(native_pg_types[column] in ('int2','int4','int8','bool') for column in boolean_columns)
            checks.append(f'native Boolean audit: {len(boolean_columns)} actual source fields map to ' + ','.join(sorted({native_pg_types[column] for column in boolean_columns})))
            checks.append('exact native accounts BOOLEAN t/f defaults -> PG INTEGER 1/0, integers 2/-7/NULL preserved; INTEGER/BOOLEAN source -> PG BOOLEAN true/false/NULL')

            assert sql("SELECT encode(blob,'hex') FROM plex.blobs WHERE id=902") == bytes(range(256)).hex()
            assert sql('INSERT INTO plex.devices(name) VALUES(\'next\') RETURNING id') == '15193'
            database_before = sql('SELECT row_to_json(t) FROM plex.metadata_items t ORDER BY id')
            shell('migrate_sqlite_to_pg', ok=False, extra=import_env)
            assert database_before == sql('SELECT row_to_json(t) FROM plex.metadata_items t ORDER BY id')
            assert hashes == [digest(source), digest(blob_source)]
            checks.append('full import: >255/8KB Unicode text, JSON, blobs, exact immutable source hashes, imported sequence, populated destination preservation')
            sql('TRUNCATE plex.metadata_items, plex.devices, plex.blobs, plex.accounts, plex.preferences, plex.preference_boolean_test')
            for table, field, invalid_value in (('accounts','auto_select_subtitle','arbitrary text'),
                                                ('accounts','auto_select_audio',.5),
                                                ('preference_boolean_test','active',2),
                                                ('preference_boolean_test','enabled','not-a-boolean')):
                with sqlite3.connect(source) as connection:
                    connection.execute(f'UPDATE {table} SET {field}=? WHERE id=?', (invalid_value,4 if table=='accounts' else 1))
                invalid_hashes = [digest(source),digest(blob_source)]
                failed_bool = shell('migrate_sqlite_to_pg', ok=False, extra=import_env)
                assert invalid_hashes == [digest(source),digest(blob_source)]
                assert sql('SELECT count(*) FROM plex.accounts') == '0'
                assert sql('SELECT count(*) FROM plex.preference_boolean_test') == '0'
                assert '__UNREPRESENTABLE_SQLITE_BOOLEAN__' in (temp / 'logs/migration_errors.log').read_text()
                assert sql("SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'plex_import_%'") == '0'
                with sqlite3.connect(source) as connection:
                    connection.execute(f'UPDATE {table} SET {field}=? WHERE id=?', (1 if field=='active' else 't',4 if table=='accounts' else 1))
            checks.append('unrepresentable Boolean text/fraction rejected for INTEGER destinations, integer 2 rejected for BOOLEAN destination: no activated rows, exact source hashes and staged schema cleanup')
            with sqlite3.connect(source) as connection:
                connection.execute('UPDATE metadata_items SET metadata_type=\'not-an-integer\'')
            hashes = [digest(source), digest(blob_source)]
            result = shell('migrate_sqlite_to_pg', ok=False, extra=import_env)
            assert sql('SELECT count(*) FROM plex.metadata_items') == '0'
            assert sql('SELECT count(*) FROM plex.devices') == '0'
            assert hashes == [digest(source), digest(blob_source)]
            assert 'invalid input syntax' in (temp / 'logs/migration_errors.log').read_text()
            assert sql("SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'plex_import_%'") == '0'
            checks.append('failed COPY: visible diagnostics, no activation, staging removed, exact source hashes')
            sql('DROP TABLE plex.preference_boolean_test')
            # Successful data export and failed export must preserve existing files.
            sql('INSERT INTO plex.metadata_items(id,metadata_type,title,guid) VALUES(72,1,\'export\',\'long\'); INSERT INTO plex.blobs(id,blob) VALUES(1001,decode(\'00ff\',\'hex\'));')
            output = temp / 'export'
            command = ['python3', str(ROOT / 'scripts/export_pg_to_sqlite.py'), '--schema', 'plex', '--output-dir', str(output)]
            run(command, env=env)
            exports = [output / 'com.plexapp.plugins.library.db', output / 'com.plexapp.plugins.library.blobs.db']
            export_hashes = [digest(path) for path in exports]
            run(command, env=env, ok=False)
            run(command + ['--yes'], env=dict(env, PGPORT='1'), ok=False)
            assert export_hashes == [digest(path) for path in exports]
            with sqlite3.connect(exports[0]) as connection:
                assert connection.execute('SELECT title FROM metadata_items WHERE id=72').fetchone() == ('export',)
            with sqlite3.connect(exports[1]) as connection:
                assert connection.execute('SELECT blob FROM blobs WHERE id=1001').fetchone() == (b'\x00\xff',)
            # Inject failure during the second publication, after the first file
            # was replaced, and prove ordinary-error rollback restores the pair.
            spec = importlib.util.spec_from_file_location('backlog_export', ROOT / 'scripts/export_pg_to_sqlite.py')
            exporter = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(exporter)
            stage = temp / 'partial-publication'
            stage.mkdir()
            for path in exports:
                (stage / path.name).write_bytes(b'replacement fixture')
            original_replace = exporter.os.replace
            calls = 0
            def fail_second(source_path, target_path):
                nonlocal calls
                calls += 1
                if calls == 2:
                    raise OSError('injected second file publication failure')
                original_replace(source_path, target_path)
            exporter.os.replace = fail_second
            try:
                try:
                    exporter.promote(stage, output)
                except OSError as error:
                    assert str(error) == 'injected second file publication failure'
                else:
                    raise AssertionError('second publication unexpectedly succeeded')
            finally:
                exporter.os.replace = original_replace
            assert export_hashes == [digest(path) for path in exports]
            checks.append('exact data export + existing export rejection/PG failure/second publication failure preserves both hashes')
            if native_image:
                native_cli = temp / 'plex-sqlite-wrapper'
                native_cli.write_text('#!/bin/bash\nexec docker run --rm -i --network none -v "$(dirname "$1"):$(dirname "$1")" --entrypoint "/usr/lib/plexmediaserver/Plex SQLite" "$PLEX_TEST_NATIVE_IMAGE" "$@"\n')
                native_cli.chmod(0o700)
                native_env = dict(env, PLEX_TEST_NATIVE_IMAGE=native_image)
                native_output = temp / 'native-export'
                native_command = ['python3', str(ROOT / 'scripts/export_pg_to_sqlite.py'), '--schema', 'plex',
                                  '--output-dir', str(native_output), '--native-plex-sqlite', str(native_cli)]
                sql("UPDATE plex.metadata_items SET title_sort=title,original_title=title; INSERT INTO plex.locations(id,lat_min,lat_max,lon_min,lon_max) VALUES(3,1.25,1.5,2.25,2.5); INSERT INTO plex.tags(id,tag) VALUES(9,'Snow');")
                run(native_command, env=native_env)
                native_library = native_output / 'com.plexapp.plugins.library.db'
                native_artwork = native_output / 'com.plexapp.plugins.library.blobs.db'
                result = run([str(native_cli), str(native_library), "SELECT id FROM metadata_items WHERE id IN (SELECT docid FROM fts4_metadata_titles_icu WHERE fts4_metadata_titles_icu MATCH 'export'); SELECT docid FROM fts4_tag_titles WHERE fts4_tag_titles MATCH 'Snow'; SELECT id||'|'||lat_min FROM locations; PRAGMA integrity_check;"], env=native_env)
                assert result.stdout.strip() == '72\n9\n3|1.25\nok'
                native_hashes = [digest(native_library), digest(native_artwork)]
                # NULL indexed columns reproduce the diagnostic in an entirely
                # native-created control, without PostgreSQL or export involved.
                control = run(['docker', 'run', '--rm', '-i', '--network', 'none', '--entrypoint',
                    '/usr/lib/plexmediaserver/Plex SQLite', native_image, ':memory:'], stdin=
                    "CREATE TABLE metadata_items(id INTEGER PRIMARY KEY,title TEXT,title_sort TEXT,original_title TEXT);\n"
                    "INSERT INTO metadata_items VALUES(72,'export',NULL,NULL);\n"
                    "CREATE VIRTUAL TABLE fts4_metadata_titles_icu USING fts4(content='metadata_items',title,title_sort,original_title,tokenize=collating 'root@colStrength=primary;colAlternate=shifted');\n"
                    "INSERT INTO fts4_metadata_titles_icu(fts4_metadata_titles_icu) VALUES('rebuild');\nPRAGMA integrity_check;\n")
                assert control.stdout.strip() == 'unable to validate the inverted index for FTS4 table main.fts4_metadata_titles_icu: SQL logic error'
                unusual_title = 'export 雪 \"quoted\"\\path'
                sql("UPDATE plex.metadata_items SET title=" + "'" + unusual_title.replace("'", "''") + "'" + ",title_sort=NULL,original_title=NULL; INSERT INTO plex.tags(id,tag) VALUES(10,NULL)")
                nullable = run(native_command + ['--yes'], env=native_env)
                assert 'Known official Plex SQLite ICU NULL-validator limitation reproduced' in nullable.stdout
                with sqlite3.connect(native_library) as connection:
                    assert connection.execute('SELECT title_sort,original_title FROM metadata_items WHERE id=72').fetchone() == (None, None)
                native_hashes = [digest(native_library), digest(native_artwork)]
                corrupt_cli = temp / 'corrupt-native-cli'
                corrupt_cli.write_text("""#!/usr/bin/env python3
import os, pathlib, sqlite3, subprocess, sys
script=sys.stdin.read()
if os.environ['PLEX_TEST_CORRUPT'] == 'index' and 'PRAGMA integrity_check;' in script and pathlib.Path(sys.argv[1]).name.endswith('library.db'):
    script = 'DELETE FROM fts4_metadata_titles WHERE docid=72;\\n' + script
result=subprocess.run([os.environ['PLEX_TEST_REAL_CLI'], *sys.argv[1:]], input=script, text=True, capture_output=True)
sys.stdout.write(result.stdout)
sys.stderr.write(result.stderr)
if result.returncode == 0 and os.environ['PLEX_TEST_CORRUPT'] == 'physical' and 'RTREE_CONTENT' in script:
    path=pathlib.Path(sys.argv[1])
    with sqlite3.connect(path) as connection:
        page=connection.execute("SELECT rootpage FROM sqlite_master WHERE name='metadata_items'").fetchone()[0]
    with path.open('r+b') as output:
        header=output.read(100)
        size=int.from_bytes(header[16:18],'big')
        size=65536 if size == 1 else size
        output.seek((page-1)*size)
        output.write(b'\\xff')
sys.exit(result.returncode)
""")
                corrupt_cli.chmod(0o700)
                corrupt_command = native_command[:-1] + [str(corrupt_cli), '--yes']
                for fault in ('index', 'physical'):
                    corrupt_env = dict(native_env, PLEX_TEST_CORRUPT=fault, PLEX_TEST_REAL_CLI=str(native_cli))
                    result = run(corrupt_command, env=corrupt_env, ok=False)
                    assert 'ERROR:' in result.stderr
                    if fault == 'physical':
                        assert 'malformed' in result.stderr, result.stderr
                    else:
                        assert 'integrity validation failed' in result.stderr or 'index/source count mismatch' in result.stderr, result.stderr
                    assert native_hashes == [digest(native_library), digest(native_artwork)]

                sql('ALTER TABLE plex.metadata_items ALTER COLUMN subtype DROP EXPRESSION; UPDATE plex.metadata_items SET subtype=1')
                unsupported = run(native_command + ['--yes'], env=native_env, ok=False)
                assert 'non-generated metadata_items.subtype' in unsupported.stderr
                assert native_hashes == [digest(native_library), digest(native_artwork)]
                sql('ALTER TABLE plex.metadata_items DROP COLUMN subtype; ALTER TABLE plex.metadata_items ADD COLUMN subtype integer GENERATED ALWAYS AS ((rating_count - ((rating_count / 100) * 100))) STORED')
                failing_cli = temp / 'failing-native-cli'
                failing_cli.write_text('#!/bin/sh\necho "injected native finalizer failure" >&2\nexit 1\n')
                failing_cli.chmod(0o700)
                failed_native = native_command[:-1] + [str(failing_cli), '--yes']
                result = run(failed_native, env=native_env, ok=False)
                assert 'injected native finalizer failure' in result.stderr
                assert native_hashes == [digest(native_library), digest(native_artwork)]
                assert hashes == [digest(source), digest(blob_source)]
                checks.append('official Plex SQLite native FTS4/ICU/spellfix/RTree, NULL-preserving native ICU control, scoped physical integrity + source MATCH/count validation, physical/index corruption and finalizer failure preserve both output hashes and source')

        return {'image': image, 'server_version': sql('SHOW server_version'), 'checks': checks}
    finally:
        run(['docker', 'rm', '-f', name])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', action='append', required=True)
    parser.add_argument('--native-image', help='Optional local image containing official /usr/lib/plexmediaserver/Plex SQLite')
    args = parser.parse_args()
    for executable in ('docker', 'psql', 'sqlite3'):
        if not shutil.which(executable):
            raise SystemExit(f'Missing prerequisite: {executable}')
    for image in args.image:
        print(json.dumps(exercise(image, args.native_image), indent=2), flush=True)


if __name__ == '__main__':
    main()

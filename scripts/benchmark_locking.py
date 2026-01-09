#!/usr/bin/env python3
"""
SQLite vs PostgreSQL Locking Benchmark

Simulates the real Plex pain point:
- Long-running write transaction (like library scan)
- Concurrent reads trying to access data
- Measures how long reads are blocked

This is where PostgreSQL shines - no reader blocking.
"""

import os
import sys
import time
import sqlite3
import threading
from pathlib import Path

try:
    import psycopg2
    from psycopg2 import pool
except ImportError:
    print("ERROR: pip install psycopg2-binary")
    sys.exit(1)

# Colors
GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
BLUE = "\033[34m"
CYAN = "\033[36m"
BOLD = "\033[1m"
NC = "\033[0m"

# Find Plex DB
def find_plex_db():
    paths = [
        Path.home() / "Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db",
        Path.home() / "Library/Application Support/Plex Media Server/Plug-in Support/Databases/shadow/com.plexapp.plugins.library.db",
    ]
    for p in paths:
        if p.exists():
            return str(p)
    return None

# PostgreSQL connection config
PG_HOST = os.environ.get("PLEX_PG_HOST", "localhost")
PG_PORT = int(os.environ.get("PLEX_PG_PORT", 5432))
PG_SOCKET = os.environ.get("PLEX_PG_SOCKET", "/var/run/postgresql")
PG_DATABASE = os.environ.get("PLEX_PG_DATABASE", "plex")
PG_USER = os.environ.get("PLEX_PG_USER", "plex")
PG_PASSWORD = os.environ.get("PLEX_PG_PASSWORD", "plex")
PG_SCHEMA = os.environ.get("PLEX_PG_SCHEMA", "plex")

def get_pg_config(use_socket: bool = False) -> dict:
    """Get PostgreSQL connection config for TCP or Unix socket."""
    if use_socket:
        return {"host": PG_SOCKET, "database": PG_DATABASE, "user": PG_USER, "password": PG_PASSWORD}
    return {"host": PG_HOST, "port": PG_PORT, "database": PG_DATABASE, "user": PG_USER, "password": PG_PASSWORD}

def check_socket_available() -> bool:
    """Check if Unix socket is available."""
    return (Path(PG_SOCKET) / f".s.PGSQL.{PG_PORT}").exists()

def test_sqlite_locking(db_path, write_duration=3):
    """Test SQLite behavior during long write transaction"""
    print(f"\n{YELLOW}[SQLite Locking Test]{NC}")
    print(f"  Simulating {write_duration}s write transaction (like library scan)...")
    print(f"  While writer holds lock, readers try to query...\n")

    read_times = []
    read_errors = []
    writer_done = threading.Event()

    def writer():
        """Simulate library scan - long write transaction"""
        conn = sqlite3.connect(db_path, timeout=1)
        conn.execute("CREATE TABLE IF NOT EXISTS lock_test (id INTEGER PRIMARY KEY, val TEXT)")
        conn.execute("BEGIN EXCLUSIVE")  # Lock the entire database
        print(f"  {RED}Writer: EXCLUSIVE lock acquired{NC}")

        # Simulate long write operation
        for i in range(write_duration * 10):
            conn.execute(f"INSERT OR REPLACE INTO lock_test (id, val) VALUES ({i}, 'test')")
            time.sleep(0.1)

        conn.commit()
        print(f"  {GREEN}Writer: Transaction committed, lock released{NC}")
        conn.execute("DROP TABLE IF EXISTS lock_test")
        conn.commit()
        conn.close()
        writer_done.set()

    def reader(reader_id):
        """Try to read while writer holds lock"""
        while not writer_done.is_set():
            conn = sqlite3.connect(db_path, timeout=0.1)  # Short timeout
            start = time.perf_counter()
            try:
                conn.execute("SELECT COUNT(*) FROM metadata_items").fetchone()
                elapsed = (time.perf_counter() - start) * 1000
                read_times.append(elapsed)
            except sqlite3.OperationalError as e:
                elapsed = (time.perf_counter() - start) * 1000
                read_errors.append((reader_id, elapsed, str(e)))
            finally:
                conn.close()
            time.sleep(0.05)

    # Start writer and readers
    writer_thread = threading.Thread(target=writer)
    reader_threads = [threading.Thread(target=reader, args=(i,)) for i in range(3)]

    writer_thread.start()
    time.sleep(0.1)  # Let writer acquire lock first
    for t in reader_threads:
        t.start()

    writer_thread.join()
    for t in reader_threads:
        t.join()

    # Results
    successful = len(read_times)
    blocked = len(read_errors)

    print(f"\n  Results:")
    print(f"    Successful reads: {GREEN}{successful}{NC}")
    print(f"    Blocked reads:    {RED}{blocked}{NC} (database locked)")
    if read_times:
        print(f"    Avg read time:    {sum(read_times)/len(read_times):.2f}ms")

    return successful, blocked

def test_postgresql_locking(write_duration=3):
    """Test PostgreSQL behavior during long write transaction"""
    print(f"\n{YELLOW}[PostgreSQL Locking Test]{NC}")
    print(f"  Simulating {write_duration}s write transaction...")
    print(f"  PostgreSQL uses MVCC - readers should NOT be blocked...\n")

    read_times = []
    read_errors = []
    writer_done = threading.Event()

    pg_pool = pool.ThreadedConnectionPool(1, 10, **get_pg_config())

    def writer():
        """Simulate library scan - long write transaction"""
        conn = pg_pool.getconn()
        cur = conn.cursor()
        cur.execute(f"CREATE TABLE IF NOT EXISTS {PG_SCHEMA}.lock_test (id INTEGER PRIMARY KEY, val TEXT)")
        conn.commit()

        cur.execute("BEGIN")
        print(f"  {CYAN}Writer: Transaction started{NC}")

        # Simulate long write operation
        for i in range(write_duration * 10):
            cur.execute(f"INSERT INTO {PG_SCHEMA}.lock_test (id, val) VALUES ({i}, 'test') ON CONFLICT (id) DO UPDATE SET val = 'test'")
            time.sleep(0.1)

        conn.commit()
        print(f"  {GREEN}Writer: Transaction committed{NC}")
        cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.lock_test")
        conn.commit()
        pg_pool.putconn(conn)
        writer_done.set()

    def reader(reader_id):
        """Try to read while writer is working"""
        conn = pg_pool.getconn()
        cur = conn.cursor()
        try:
            while not writer_done.is_set():
                start = time.perf_counter()
                try:
                    cur.execute(f"SELECT COUNT(*) FROM {PG_SCHEMA}.metadata_items")
                    cur.fetchone()
                    elapsed = (time.perf_counter() - start) * 1000
                    read_times.append(elapsed)
                except Exception as e:
                    elapsed = (time.perf_counter() - start) * 1000
                    read_errors.append((reader_id, elapsed, str(e)))
                time.sleep(0.05)
        finally:
            pg_pool.putconn(conn)

    # Start writer and readers
    writer_thread = threading.Thread(target=writer)
    reader_threads = [threading.Thread(target=reader, args=(i,)) for i in range(3)]

    writer_thread.start()
    time.sleep(0.1)
    for t in reader_threads:
        t.start()

    writer_thread.join()
    for t in reader_threads:
        t.join()

    pg_pool.closeall()

    # Results
    successful = len(read_times)
    blocked = len(read_errors)

    print(f"\n  Results:")
    print(f"    Successful reads: {GREEN}{successful}{NC}")
    print(f"    Blocked reads:    {blocked}")
    if read_times:
        print(f"    Avg read time:    {sum(read_times)/len(read_times):.2f}ms")

    return successful, blocked

def test_concurrent_writers(db_path, write_duration=3, num_writers=3):
    """Test multiple concurrent writers - SQLite's real limitation"""
    print(f"\n{YELLOW}[SQLite: {num_writers} Concurrent Writers]{NC}")
    print(f"  Simulating Plex + Kometa + PMM all writing simultaneously...")

    write_counts = [0] * num_writers
    write_errors = [0] * num_writers
    stop_flag = threading.Event()

    def writer(writer_id):
        conn = sqlite3.connect(db_path, timeout=5)
        conn.execute("CREATE TABLE IF NOT EXISTS write_test (id INTEGER PRIMARY KEY, val TEXT, writer INTEGER)")
        while not stop_flag.is_set():
            try:
                conn.execute(f"INSERT INTO write_test (val, writer) VALUES ('test', {writer_id})")
                conn.commit()
                write_counts[writer_id] += 1
            except sqlite3.OperationalError:
                write_errors[writer_id] += 1
            time.sleep(0.001)  # Small delay
        conn.close()

    threads = [threading.Thread(target=writer, args=(i,)) for i in range(num_writers)]
    for t in threads:
        t.start()

    time.sleep(write_duration)
    stop_flag.set()

    for t in threads:
        t.join()

    # Cleanup
    conn = sqlite3.connect(db_path)
    conn.execute("DROP TABLE IF EXISTS write_test")
    conn.commit()
    conn.close()

    total_writes = sum(write_counts)
    total_errors = sum(write_errors)
    wps = total_writes / write_duration

    print(f"  Total writes:  {GREEN}{total_writes}{NC} ({wps:.0f}/sec)")
    print(f"  Lock errors:   {RED}{total_errors}{NC}")

    return total_writes, total_errors

def test_pg_concurrent_writers(write_duration=3, num_writers=3):
    """Test multiple concurrent writers on PostgreSQL"""
    print(f"\n{YELLOW}[PostgreSQL: {num_writers} Concurrent Writers]{NC}")
    print(f"  PostgreSQL handles multiple writers with row-level locking...")

    pg_pool = pool.ThreadedConnectionPool(1, num_writers + 5, **get_pg_config())

    # Setup
    conn = pg_pool.getconn()
    cur = conn.cursor()
    cur.execute(f"CREATE TABLE IF NOT EXISTS {PG_SCHEMA}.write_test (id SERIAL, val TEXT, writer INTEGER)")
    conn.commit()
    pg_pool.putconn(conn)

    write_counts = [0] * num_writers
    write_errors = [0] * num_writers
    stop_flag = threading.Event()

    def writer(writer_id):
        conn = pg_pool.getconn()
        cur = conn.cursor()
        try:
            while not stop_flag.is_set():
                try:
                    cur.execute(f"INSERT INTO {PG_SCHEMA}.write_test (val, writer) VALUES ('test', {writer_id})")
                    conn.commit()
                    write_counts[writer_id] += 1
                except Exception:
                    write_errors[writer_id] += 1
                    conn.rollback()
                time.sleep(0.001)
        finally:
            pg_pool.putconn(conn)

    threads = [threading.Thread(target=writer, args=(i,)) for i in range(num_writers)]
    for t in threads:
        t.start()

    time.sleep(write_duration)
    stop_flag.set()

    for t in threads:
        t.join()

    # Cleanup
    conn = pg_pool.getconn()
    cur = conn.cursor()
    cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.write_test")
    conn.commit()
    pg_pool.putconn(conn)
    pg_pool.closeall()

    total_writes = sum(write_counts)
    total_errors = sum(write_errors)
    wps = total_writes / write_duration

    print(f"  Total writes:  {GREEN}{total_writes}{NC} ({wps:.0f}/sec)")
    print(f"  Lock errors:   {total_errors}")

    return total_writes, total_errors

def main():
    print(f"\n{BLUE}{'═' * 64}{NC}")
    print(f"{BLUE}{BOLD}  SQLite vs PostgreSQL: Locking & Concurrency Comparison{NC}")
    print(f"{BLUE}{'═' * 64}{NC}")
    print()
    print(f"  SQLite: Only ONE writer at a time (others wait/fail)")
    print(f"  PostgreSQL: Multiple writers with row-level locking")

    db_path = find_plex_db()
    if not db_path:
        print(f"{RED}ERROR: Cannot find Plex database{NC}")
        sys.exit(1)

    write_duration = 3  # seconds

    # Test 1: Reader blocking during writes
    print(f"\n{BLUE}{'─' * 64}{NC}")
    print(f"{BOLD}Part 1: Reader Blocking During Writes{NC}")
    print(f"{BLUE}{'─' * 64}{NC}")

    sqlite_success, sqlite_blocked = test_sqlite_locking(db_path, write_duration)
    pg_success, pg_blocked = test_postgresql_locking(write_duration)

    # Test 2: Multiple concurrent writers (the real limitation)
    print(f"\n{BLUE}{'─' * 64}{NC}")
    print(f"{BOLD}Part 2: Multiple Concurrent Writers{NC}")
    print(f"{BLUE}{'─' * 64}{NC}")

    sqlite_writes, sqlite_errors = test_concurrent_writers(db_path, write_duration, num_writers=3)
    pg_writes, pg_errors = test_pg_concurrent_writers(write_duration, num_writers=3)

    # Summary
    print(f"\n{BLUE}{'═' * 64}{NC}")
    print(f"{BOLD}Summary:{NC}\n")

    print(f"  {CYAN}Reader Blocking Test:{NC}")
    print(f"  {'Database':<15} {'Successful Reads':<20} {'Blocked Reads':<15}")
    print(f"  {'-'*50}")
    print(f"  {'SQLite':<15} {sqlite_success:<20} {RED}{sqlite_blocked}{NC}")
    print(f"  {'PostgreSQL':<15} {pg_success:<20} {GREEN}{pg_blocked}{NC}")
    print()

    print(f"  {CYAN}Concurrent Writers Test:{NC}")
    print(f"  {'Database':<15} {'Total Writes':<20} {'Lock Errors':<15}")
    print(f"  {'-'*50}")
    print(f"  {'SQLite':<15} {sqlite_writes:<20} {RED}{sqlite_errors}{NC}")
    print(f"  {'PostgreSQL':<15} {pg_writes:<20} {GREEN}{pg_errors}{NC}")
    print()

    if pg_writes > sqlite_writes:
        speedup = pg_writes / sqlite_writes if sqlite_writes > 0 else float('inf')
        print(f"  {GREEN}PostgreSQL: {speedup:.1f}x more concurrent writes!{NC}")

    print()
    print(f"  {CYAN}This is why PostgreSQL is better for Plex:{NC}")
    print(f"    • Library scans don't block playback")
    print(f"    • Metadata updates don't freeze the UI")
    print(f"    • Multiple users can stream while scanning")
    print(f"    • Kometa/PMM can update while Plex is active")
    print()

if __name__ == "__main__":
    main()

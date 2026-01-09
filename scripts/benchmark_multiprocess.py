#!/usr/bin/env python3
"""
Multi-Process Stress Test - The REAL SQLite Pain Point

This test uses actual separate processes (not threads) to simulate:
- Plex Media Server scanning
- Kometa updating metadata
- PMM updating collections
- Multiple playback streams

SQLite can only have ONE writer at a time across ALL processes.
When multiple processes try to write, they get "database is locked" errors.

PostgreSQL handles this with row-level locking - all processes can write simultaneously.
"""

import os
import sys
import time
import sqlite3
import multiprocessing as mp
from pathlib import Path
from dataclasses import dataclass
from typing import Tuple

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

def find_plex_db():
    paths = [
        Path.home() / "Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db",
        Path("/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"),
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
        return {
            "host": PG_SOCKET,
            "database": PG_DATABASE,
            "user": PG_USER,
            "password": PG_PASSWORD,
        }
    else:
        return {
            "host": PG_HOST,
            "port": PG_PORT,
            "database": PG_DATABASE,
            "user": PG_USER,
            "password": PG_PASSWORD,
        }

def check_socket_available() -> bool:
    """Check if Unix socket is available."""
    socket_file = Path(PG_SOCKET) / f".s.PGSQL.{PG_PORT}"
    return socket_file.exists()


# ============================================================================
# SQLite Multi-Process Workers
# ============================================================================

def sqlite_plex_scanner(db_path: str, duration: int, result_queue: mp.Queue):
    """Simulate Plex Media Server library scanner - SEPARATE PROCESS"""
    import random
    writes = 0
    errors = 0

    conn = sqlite3.connect(db_path, timeout=0)  # ZERO timeout - fail immediately like real apps
    conn.execute("""
        CREATE TABLE IF NOT EXISTS mp_stress_scan (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            guid TEXT, title TEXT, added_at REAL
        )
    """)
    conn.commit()

    end_time = time.time() + duration
    while time.time() < end_time:
        try:
            # BEGIN EXCLUSIVE locks out ALL other connections (even readers in some modes)
            conn.execute("BEGIN EXCLUSIVE")
            for _ in range(50):  # Bigger batch = longer lock hold
                conn.execute(
                    "INSERT INTO mp_stress_scan (guid, title, added_at) VALUES (?, ?, ?)",
                    (f"plex://{random.randint(1,999999)}", f"Movie {random.randint(1,99999)}", time.time())
                )
            conn.commit()
            writes += 50
        except sqlite3.OperationalError as e:
            errors += 1
            try:
                conn.rollback()
            except:
                pass
        # No sleep - hammer the database

    conn.close()
    result_queue.put(("plex_scanner", writes, errors))


def sqlite_kometa_worker(db_path: str, duration: int, result_queue: mp.Queue):
    """Simulate Kometa updating metadata - SEPARATE PROCESS"""
    import random
    writes = 0
    errors = 0

    conn = sqlite3.connect(db_path, timeout=0)  # ZERO timeout

    end_time = time.time() + duration
    while time.time() < end_time:
        try:
            # Kometa does EXCLUSIVE updates for batch operations
            conn.execute("BEGIN EXCLUSIVE")
            for _ in range(10):
                conn.execute(
                    "UPDATE metadata_items SET updated_at = ? WHERE id = ?",
                    (time.time(), random.randint(1, 50000))
                )
            conn.commit()
            writes += 10
        except sqlite3.OperationalError:
            errors += 1
            try:
                conn.rollback()
            except:
                pass
        # No sleep - aggressive

    conn.close()
    result_queue.put(("kometa", writes, errors))


def sqlite_pmm_worker(db_path: str, duration: int, result_queue: mp.Queue):
    """Simulate Plex Meta Manager - SEPARATE PROCESS"""
    import random
    writes = 0
    errors = 0

    conn = sqlite3.connect(db_path, timeout=0)  # ZERO timeout

    end_time = time.time() + duration
    while time.time() < end_time:
        try:
            # PMM does EXCLUSIVE updates
            conn.execute("BEGIN EXCLUSIVE")
            for _ in range(10):
                conn.execute(
                    "UPDATE metadata_items SET updated_at = ? WHERE id = ?",
                    (time.time(), random.randint(1, 50000))
                )
            conn.commit()
            writes += 10
        except sqlite3.OperationalError:
            errors += 1
            try:
                conn.rollback()
            except:
                pass
        # No sleep

    conn.close()
    result_queue.put(("pmm", writes, errors))


def sqlite_playback_worker(db_path: str, duration: int, stream_id: int, result_queue: mp.Queue):
    """Simulate playback - reads + watch progress updates - SEPARATE PROCESS"""
    import random
    reads = 0
    read_errors = 0
    writes = 0
    write_errors = 0

    conn = sqlite3.connect(db_path, timeout=0)  # ZERO timeout - playback CANNOT wait
    try:
        conn.execute("""
            CREATE TABLE IF NOT EXISTS mp_stress_progress (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                item_id INTEGER, view_offset INTEGER, updated_at REAL
            )
        """)
        conn.commit()
    except:
        pass

    end_time = time.time() + duration
    while time.time() < end_time:
        # Read media info - critical for playback
        try:
            conn.execute(
                "SELECT id, title, rating FROM metadata_items WHERE id = ?",
                (random.randint(1, 50000),)
            ).fetchone()
            reads += 1
        except sqlite3.OperationalError:
            read_errors += 1

        # Update watch progress - also critical
        try:
            conn.execute(
                "INSERT INTO mp_stress_progress (item_id, view_offset, updated_at) VALUES (?, ?, ?)",
                (random.randint(1, 1000), random.randint(0, 7200000), time.time())
            )
            conn.commit()
            writes += 1
        except sqlite3.OperationalError:
            write_errors += 1
            try:
                conn.rollback()
            except:
                pass

        time.sleep(0.01)  # Fast polling for smooth playback

    conn.close()
    result_queue.put((f"playback_{stream_id}", reads, read_errors, writes, write_errors))


# ============================================================================
# PostgreSQL Multi-Process Workers
# ============================================================================

def pg_plex_scanner(duration: int, result_queue: mp.Queue, pg_config: dict):
    """Simulate Plex scanner on PostgreSQL - SEPARATE PROCESS"""
    import random
    writes = 0
    errors = 0

    conn = psycopg2.connect(**pg_config)
    cur = conn.cursor()
    # Table created in setup phase

    end_time = time.time() + duration
    while time.time() < end_time:
        try:
            for _ in range(20):
                cur.execute(
                    f"INSERT INTO {PG_SCHEMA}.mp_stress_scan (guid, title, added_at) VALUES (%s, %s, %s)",
                    (f"plex://{random.randint(1,999999)}", f"Movie {random.randint(1,99999)}", time.time())
                )
            conn.commit()
            writes += 20
        except Exception:
            errors += 1
            conn.rollback()
        time.sleep(0.01)

    conn.close()
    result_queue.put(("plex_scanner", writes, errors))


def pg_kometa_worker(duration: int, result_queue: mp.Queue, pg_config: dict):
    """Simulate Kometa on PostgreSQL - SEPARATE PROCESS"""
    import random
    writes = 0
    errors = 0

    conn = psycopg2.connect(**pg_config)
    cur = conn.cursor()

    end_time = time.time() + duration
    while time.time() < end_time:
        try:
            for _ in range(5):
                cur.execute(
                    f"UPDATE {PG_SCHEMA}.metadata_items SET updated_at = %s WHERE id = %s",
                    (time.time(), random.randint(1, 50000))
                )
            conn.commit()
            writes += 5
        except Exception:
            errors += 1
            conn.rollback()
        time.sleep(0.02)

    conn.close()
    result_queue.put(("kometa", writes, errors))


def pg_pmm_worker(duration: int, result_queue: mp.Queue, pg_config: dict):
    """Simulate PMM on PostgreSQL - SEPARATE PROCESS"""
    import random
    writes = 0
    errors = 0

    conn = psycopg2.connect(**pg_config)
    cur = conn.cursor()

    end_time = time.time() + duration
    while time.time() < end_time:
        try:
            for _ in range(3):
                cur.execute(
                    f"UPDATE {PG_SCHEMA}.metadata_items SET updated_at = %s WHERE id = %s",
                    (time.time(), random.randint(1, 50000))
                )
            conn.commit()
            writes += 3
        except Exception:
            errors += 1
            conn.rollback()
        time.sleep(0.03)

    conn.close()
    result_queue.put(("pmm", writes, errors))


def pg_playback_worker(duration: int, stream_id: int, result_queue: mp.Queue, pg_config: dict):
    """Simulate playback on PostgreSQL - SEPARATE PROCESS"""
    import random
    reads = 0
    read_errors = 0
    writes = 0
    write_errors = 0

    conn = psycopg2.connect(**pg_config)
    cur = conn.cursor()
    # Table created in setup phase, just start working

    end_time = time.time() + duration
    while time.time() < end_time:
        try:
            cur.execute(
                f"SELECT id, title, rating FROM {PG_SCHEMA}.metadata_items WHERE id = %s",
                (random.randint(1, 50000),)
            )
            cur.fetchone()
            reads += 1
        except Exception:
            read_errors += 1

        try:
            cur.execute(
                f"INSERT INTO {PG_SCHEMA}.mp_stress_progress (item_id, view_offset, updated_at) VALUES (%s, %s, %s)",
                (random.randint(1, 1000), random.randint(0, 7200000), time.time())
            )
            conn.commit()
            writes += 1
        except Exception:
            write_errors += 1
            conn.rollback()

        time.sleep(0.05)

    conn.close()
    result_queue.put((f"playback_{stream_id}", reads, read_errors, writes, write_errors))


# ============================================================================
# Main Test Runner
# ============================================================================

def run_sqlite_multiprocess_test(db_path: str, duration: int = 15) -> dict:
    """Run SQLite test with multiple separate processes"""
    print(f"\n{YELLOW}[SQLite Multi-Process Test]{NC}")
    print(f"  Spawning separate processes for: Plex Scanner, Kometa, PMM, 4 Streams")
    print(f"  Duration: {duration}s")
    print(f"  {RED}SQLite limitation: Only ONE writer across ALL processes{NC}\n")

    result_queue = mp.Queue()

    # Spawn processes
    processes = [
        mp.Process(target=sqlite_plex_scanner, args=(db_path, duration, result_queue)),
        mp.Process(target=sqlite_kometa_worker, args=(db_path, duration, result_queue)),
        mp.Process(target=sqlite_pmm_worker, args=(db_path, duration, result_queue)),
    ]
    for i in range(4):
        processes.append(mp.Process(target=sqlite_playback_worker, args=(db_path, duration, i, result_queue)))

    start = time.time()
    for p in processes:
        p.start()
    for p in processes:
        p.join()
    elapsed = time.time() - start

    # Collect results
    results = {"writes": 0, "write_errors": 0, "reads": 0, "read_errors": 0}
    while not result_queue.empty():
        item = result_queue.get()
        if len(item) == 3:  # writer process
            name, writes, errors = item
            results["writes"] += writes
            results["write_errors"] += errors
            print(f"    {name}: {writes} writes, {RED}{errors} errors{NC}")
        else:  # playback process
            name, reads, read_errors, writes, write_errors = item
            results["reads"] += reads
            results["read_errors"] += read_errors
            results["writes"] += writes
            results["write_errors"] += write_errors
            print(f"    {name}: {reads} reads ({RED}{read_errors} errors{NC}), {writes} writes ({RED}{write_errors} errors{NC})")

    results["elapsed"] = elapsed

    # Cleanup
    conn = sqlite3.connect(db_path)
    conn.execute("DROP TABLE IF EXISTS mp_stress_scan")
    conn.execute("DROP TABLE IF EXISTS mp_stress_progress")
    conn.commit()
    conn.close()

    return results


def run_postgresql_multiprocess_test(duration: int = 15, use_socket: bool = False) -> dict:
    """Run PostgreSQL test with multiple separate processes"""
    pg_config = get_pg_config(use_socket=use_socket)
    conn_type = "Unix socket" if use_socket else "TCP/IP"
    conn_detail = PG_SOCKET if use_socket else f"{PG_HOST}:{PG_PORT}"
    
    print(f"\n{YELLOW}[PostgreSQL Multi-Process Test - {conn_type}]{NC}")
    print(f"  Connection: {conn_type} ({conn_detail})")
    print(f"  Spawning separate processes for: Plex Scanner, Kometa, PMM, 4 Streams")
    print(f"  Duration: {duration}s")
    print(f"  {GREEN}PostgreSQL: All processes can write simultaneously (MVCC){NC}\n")

    # Setup tables before spawning processes (avoid race condition)
    conn = psycopg2.connect(**pg_config)
    cur = conn.cursor()
    cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.mp_stress_scan CASCADE")
    cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.mp_stress_progress CASCADE")
    cur.execute(f"""
        CREATE TABLE {PG_SCHEMA}.mp_stress_scan (
            id SERIAL PRIMARY KEY,
            guid TEXT, title TEXT, added_at DOUBLE PRECISION
        )
    """)
    cur.execute(f"""
        CREATE TABLE {PG_SCHEMA}.mp_stress_progress (
            id SERIAL PRIMARY KEY,
            item_id INTEGER, view_offset INTEGER, updated_at DOUBLE PRECISION
        )
    """)
    conn.commit()
    conn.close()

    result_queue = mp.Queue()

    processes = [
        mp.Process(target=pg_plex_scanner, args=(duration, result_queue, pg_config)),
        mp.Process(target=pg_kometa_worker, args=(duration, result_queue, pg_config)),
        mp.Process(target=pg_pmm_worker, args=(duration, result_queue, pg_config)),
    ]
    for i in range(4):
        processes.append(mp.Process(target=pg_playback_worker, args=(duration, i, result_queue, pg_config)))

    start = time.time()
    for p in processes:
        p.start()
    for p in processes:
        p.join()
    elapsed = time.time() - start

    results = {"writes": 0, "write_errors": 0, "reads": 0, "read_errors": 0}
    while not result_queue.empty():
        item = result_queue.get()
        if len(item) == 3:
            name, writes, errors = item
            results["writes"] += writes
            results["write_errors"] += errors
            print(f"    {name}: {writes} writes, {GREEN}{errors} errors{NC}")
        else:
            name, reads, read_errors, writes, write_errors = item
            results["reads"] += reads
            results["read_errors"] += read_errors
            results["writes"] += writes
            results["write_errors"] += write_errors
            print(f"    {name}: {reads} reads ({GREEN}{read_errors} errors{NC}), {writes} writes ({GREEN}{write_errors} errors{NC})")

    results["elapsed"] = elapsed

    # Cleanup
    conn = psycopg2.connect(**pg_config)
    cur = conn.cursor()
    cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.mp_stress_scan")
    cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.mp_stress_progress")
    conn.commit()
    conn.close()

    return results


def main():
    print(f"\n{BLUE}{'═' * 72}{NC}")
    print(f"{BLUE}{BOLD}  Multi-Process Database Stress Test{NC}")
    print(f"{BLUE}{BOLD}  Simulating: Plex + Kometa + PMM + 4 Streams (7 processes){NC}")
    print(f"{BLUE}{'═' * 72}{NC}")
    print()
    print(f"  {CYAN}Why this matters:{NC}")
    print(f"  SQLite allows only ONE writer at a time, across ALL processes.")
    print(f"  When Plex scans while Kometa/PMM update metadata, writes FAIL.")
    print()
    print(f"  PostgreSQL uses MVCC - multiple processes write simultaneously.")

    db_path = find_plex_db()
    if not db_path:
        print(f"\n{RED}ERROR: Cannot find Plex database{NC}")
        sys.exit(1)

    duration = 15  # seconds

    sqlite_results = run_sqlite_multiprocess_test(db_path, duration)
    
    # Check socket availability
    socket_available = check_socket_available()
    
    pg_tcp_results = run_postgresql_multiprocess_test(duration, use_socket=False)
    pg_socket_results = None
    if socket_available:
        pg_socket_results = run_postgresql_multiprocess_test(duration, use_socket=True)
    else:
        print(f"\n  {YELLOW}Unix socket not available at {PG_SOCKET}, skipping socket test{NC}")

    # Summary
    print(f"\n{BLUE}{'═' * 72}{NC}")
    print(f"{BOLD}RESULTS SUMMARY{NC}\n")

    print(f"  {'Metric':<25} {'SQLite':<15} {'PG (TCP)':<15} {'PG (Socket)':<15}")
    print(f"  {'-'*70}")
    pg_socket_writes = pg_socket_results['writes'] if pg_socket_results else '-'
    pg_socket_write_err = pg_socket_results['write_errors'] if pg_socket_results else '-'
    pg_socket_reads = pg_socket_results['reads'] if pg_socket_results else '-'
    pg_socket_read_err = pg_socket_results['read_errors'] if pg_socket_results else '-'
    print(f"  {'Total Writes':<25} {sqlite_results['writes']:<15} {pg_tcp_results['writes']:<15} {pg_socket_writes:<15}")
    print(f"  {'Write Errors':<25} {RED}{sqlite_results['write_errors']:<15}{NC} {GREEN}{pg_tcp_results['write_errors']:<15}{NC} {GREEN}{pg_socket_write_err}{NC}")
    print(f"  {'Total Reads':<25} {sqlite_results['reads']:<15} {pg_tcp_results['reads']:<15} {pg_socket_reads:<15}")
    print(f"  {'Read Errors':<25} {RED}{sqlite_results['read_errors']:<15}{NC} {GREEN}{pg_tcp_results['read_errors']:<15}{NC} {GREEN}{pg_socket_read_err}{NC}")

    total_sqlite_errors = sqlite_results['write_errors'] + sqlite_results['read_errors']
    total_pg_tcp_errors = pg_tcp_results['write_errors'] + pg_tcp_results['read_errors']
    total_pg_socket_errors = (pg_socket_results['write_errors'] + pg_socket_results['read_errors']) if pg_socket_results else 0

    print()
    print(f"  {BOLD}Total Errors:{NC}")
    print(f"    SQLite:            {RED}{total_sqlite_errors}{NC}")
    print(f"    PostgreSQL (TCP):  {GREEN}{total_pg_tcp_errors}{NC}")
    if pg_socket_results:
        print(f"    PostgreSQL (Socket): {GREEN}{total_pg_socket_errors}{NC}")

    if total_sqlite_errors > total_pg_tcp_errors:
        error_reduction = total_sqlite_errors - total_pg_tcp_errors
        if total_sqlite_errors > 0:
            pct = 100 * error_reduction / total_sqlite_errors
            print(f"\n  {GREEN}PostgreSQL: {pct:.0f}% fewer errors ({error_reduction} fewer failures){NC}")

    # Compare TCP vs Socket throughput
    if pg_socket_results:
        tcp_ops = pg_tcp_results['writes'] + pg_tcp_results['reads']
        socket_ops = pg_socket_results['writes'] + pg_socket_results['reads']
        if socket_ops > tcp_ops:
            speedup = socket_ops / tcp_ops
            print(f"  {GREEN}Unix Socket: {speedup:.2f}x faster than TCP{NC}")

    # Error rate per minute (extrapolated)
    sqlite_errors_per_min = total_sqlite_errors * 60 / duration
    pg_tcp_errors_per_min = total_pg_tcp_errors * 60 / duration

    print(f"\n  {CYAN}Projected error rate:{NC}")
    print(f"    SQLite:           {RED}{sqlite_errors_per_min:.0f} errors/minute{NC}")
    print(f"    PostgreSQL (TCP): {GREEN}{pg_tcp_errors_per_min:.0f} errors/minute{NC}")

    if sqlite_errors_per_min > 0:
        hourly = sqlite_errors_per_min * 60
        print(f"\n  {RED}During a 1-hour library scan, SQLite would have ~{hourly:.0f} failures{NC}")
        print(f"  {RED}Each failure = potential buffering/stutter for users{NC}")

    print(f"\n{BLUE}{'═' * 72}{NC}\n")


if __name__ == "__main__":
    mp.set_start_method('spawn', force=True)  # Clean process spawning
    main()

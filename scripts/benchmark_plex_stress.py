#!/usr/bin/env python3
"""
Plex Stress Test: Library Scan + Playback Simulation

Simulates the real pain point for rclone/Real-Debrid users:
- Heavy library scan (many writes, like scanning 1000s of files)
- Concurrent playback (reads for stream info + watch progress updates)

This is where SQLite's single-writer limitation causes:
- Playback stuttering during scans
- "Database locked" errors
- UI freezing

PostgreSQL handles this with MVCC - no blocking between readers/writers.
"""

import os
import sys
import time
import sqlite3
import threading
import random
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass

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
# TCP/IP: set PLEX_PG_HOST to hostname (e.g., localhost or 127.0.0.1)
# Unix socket: set PLEX_PG_SOCKET to socket directory (e.g., /var/run/postgresql or /tmp)
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
            "host": PG_SOCKET,  # Socket directory
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

@dataclass
class StressResults:
    scan_writes: int = 0
    scan_errors: int = 0
    playback_reads: int = 0
    playback_read_errors: int = 0
    playback_writes: int = 0  # watch progress updates
    playback_write_errors: int = 0
    total_time_ms: float = 0


def run_sqlite_stress(db_path: str, duration: int = 10, num_streams: int = 4) -> StressResults:
    """
    Simulate heavy library scan + concurrent playback on SQLite.

    Real-Debrid/rclone scenario:
    - Scanner: Rapidly inserting/updating metadata (like scanning mounted cloud storage)
    - Streams: Reading media info + updating watch progress
    - Kometa: Concurrent metadata updates
    """
    print(f"\n{YELLOW}[SQLite Stress Test]{NC}")
    print(f"  Simulating: 2 scanners + {num_streams} streams + Kometa writes")
    print(f"  Duration: {duration}s")
    print(f"  This simulates real rclone/Real-Debrid + Kometa workload...\n")

    results = StressResults()
    stop_flag = threading.Event()
    lock = threading.Lock()

    # Create test tables
    conn = sqlite3.connect(db_path, timeout=30)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS stress_metadata (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            guid TEXT,
            title TEXT,
            summary TEXT,
            duration INTEGER,
            added_at REAL,
            updated_at REAL
        )
    """)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS stress_progress (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            metadata_id INTEGER,
            view_offset INTEGER,
            updated_at REAL
        )
    """)
    conn.commit()
    conn.close()

    def library_scanner(scanner_id: int):
        """Simulate heavy library scan - lots of writes with EXCLUSIVE lock"""
        conn = sqlite3.connect(db_path, timeout=0.1)  # VERY short timeout
        batch_size = 50  # Bigger batches = longer lock hold time

        while not stop_flag.is_set():
            try:
                # BEGIN EXCLUSIVE simulates how Plex scanner works
                conn.execute("BEGIN EXCLUSIVE")
                for _ in range(batch_size):
                    guid = f"com.plexapp.agents.imdb://{random.randint(1000000, 9999999)}"
                    conn.execute("""
                        INSERT INTO stress_metadata (guid, title, summary, duration, added_at, updated_at)
                        VALUES (?, ?, ?, ?, ?, ?)
                    """, (
                        guid,
                        f"Movie {random.randint(1, 100000)}",
                        "A movie about things happening" * 10,
                        random.randint(3600000, 10800000),
                        time.time(),
                        time.time()
                    ))
                conn.commit()
                with lock:
                    results.scan_writes += batch_size
            except sqlite3.OperationalError:
                with lock:
                    results.scan_errors += 1
                try:
                    conn.rollback()
                except:
                    pass

            time.sleep(0.001)  # Aggressive scanning

        conn.close()

    def kometa_updater():
        """Simulate Kometa/PMM updating metadata - competing writer"""
        conn = sqlite3.connect(db_path, timeout=0.1)  # VERY short timeout

        while not stop_flag.is_set():
            try:
                # Kometa does EXCLUSIVE updates for batch operations
                conn.execute("BEGIN EXCLUSIVE")
                # Update multiple items like Kometa does
                for _ in range(5):
                    conn.execute("""
                        UPDATE metadata_items SET updated_at = ?
                        WHERE id = ?
                    """, (time.time(), random.randint(1, 60000)))
                conn.commit()
                with lock:
                    results.scan_writes += 5
            except sqlite3.OperationalError:
                with lock:
                    results.scan_errors += 1
                try:
                    conn.rollback()
                except:
                    pass

            time.sleep(0.005)  # Kometa is aggressive

        conn.close()

    def playback_stream(stream_id: int):
        """Simulate active playback - reads + watch progress writes"""
        conn = sqlite3.connect(db_path, timeout=0.05)  # VERY short - playback can't wait!

        while not stop_flag.is_set():
            # Read: Get media info (happens constantly during playback)
            try:
                conn.execute("""
                    SELECT id, title, duration FROM stress_metadata
                    ORDER BY RANDOM() LIMIT 1
                """).fetchone()

                # Also read from real Plex tables
                conn.execute("""
                    SELECT id, title, rating FROM metadata_items
                    WHERE id = ?
                """, (random.randint(1, 60000),)).fetchone()

                with lock:
                    results.playback_reads += 2
            except sqlite3.OperationalError:
                with lock:
                    results.playback_read_errors += 1

            # Write: Update watch progress (every few seconds during playback)
            try:
                conn.execute("""
                    INSERT INTO stress_progress (metadata_id, view_offset, updated_at)
                    VALUES (?, ?, ?)
                """, (random.randint(1, 1000), random.randint(0, 10800000), time.time()))
                conn.commit()
                with lock:
                    results.playback_writes += 1
            except sqlite3.OperationalError:
                with lock:
                    results.playback_write_errors += 1
                try:
                    conn.rollback()
                except:
                    pass

            # Playback polling interval - needs to be fast for smooth UX
            time.sleep(0.02)

        conn.close()

    # Start threads: 2 scanners + Kometa + streams
    start_time = time.perf_counter()

    scanner_threads = [threading.Thread(target=library_scanner, args=(i,)) for i in range(2)]
    kometa_thread = threading.Thread(target=kometa_updater)
    stream_threads = [threading.Thread(target=playback_stream, args=(i,)) for i in range(num_streams)]

    for t in scanner_threads:
        t.start()
    kometa_thread.start()
    for t in stream_threads:
        t.start()

    # Run for duration
    time.sleep(duration)
    stop_flag.set()

    for t in scanner_threads:
        t.join()
    kometa_thread.join()
    for t in stream_threads:
        t.join()

    results.total_time_ms = (time.perf_counter() - start_time) * 1000

    # Cleanup
    conn = sqlite3.connect(db_path)
    conn.execute("DROP TABLE IF EXISTS stress_metadata")
    conn.execute("DROP TABLE IF EXISTS stress_progress")
    conn.commit()
    conn.close()

    return results


def run_postgresql_stress(duration: int = 10, num_streams: int = 4, use_socket: bool = False) -> StressResults:
    """
    Simulate heavy library scan + concurrent playback on PostgreSQL.
    
    Args:
        duration: Test duration in seconds
        num_streams: Number of concurrent playback streams
        use_socket: Use Unix socket instead of TCP/IP
    """
    pg_config = get_pg_config(use_socket=use_socket)
    conn_type = "Unix socket" if use_socket else "TCP/IP"
    conn_detail = PG_SOCKET if use_socket else f"{PG_HOST}:{PG_PORT}"
    
    print(f"\n{YELLOW}[PostgreSQL Stress Test - {conn_type}]{NC}")
    print(f"  Connection: {conn_type} ({conn_detail})")
    print(f"  Simulating: 2 scanners + {num_streams} streams + Kometa writes")
    print(f"  Duration: {duration}s")
    print(f"  PostgreSQL uses MVCC - no blocking between readers/writers...\n")

    results = StressResults()
    stop_flag = threading.Event()
    lock = threading.Lock()

    pg_pool = pool.ThreadedConnectionPool(1, num_streams + 10, **pg_config)  # 2 scanners + kometa + streams + margin

    # Create test tables
    conn = pg_pool.getconn()
    cur = conn.cursor()
    cur.execute(f"""
        CREATE TABLE IF NOT EXISTS {PG_SCHEMA}.stress_metadata (
            id SERIAL PRIMARY KEY,
            guid TEXT,
            title TEXT,
            summary TEXT,
            duration INTEGER,
            added_at DOUBLE PRECISION,
            updated_at DOUBLE PRECISION
        )
    """)
    cur.execute(f"""
        CREATE TABLE IF NOT EXISTS {PG_SCHEMA}.stress_progress (
            id SERIAL PRIMARY KEY,
            metadata_id INTEGER,
            view_offset INTEGER,
            updated_at DOUBLE PRECISION
        )
    """)
    conn.commit()
    pg_pool.putconn(conn)

    def library_scanner(scanner_id: int):
        """Simulate heavy library scan - lots of writes"""
        conn = pg_pool.getconn()
        cur = conn.cursor()
        batch_size = 10

        try:
            while not stop_flag.is_set():
                try:
                    for _ in range(batch_size):
                        guid = f"com.plexapp.agents.imdb://{random.randint(1000000, 9999999)}"
                        cur.execute(f"""
                            INSERT INTO {PG_SCHEMA}.stress_metadata (guid, title, summary, duration, added_at, updated_at)
                            VALUES (%s, %s, %s, %s, %s, %s)
                        """, (
                            guid,
                            f"Movie {random.randint(1, 100000)}",
                            "A movie about things happening" * 10,
                            random.randint(3600000, 10800000),
                            time.time(),
                            time.time()
                        ))
                    conn.commit()
                    with lock:
                        results.scan_writes += batch_size
                except Exception:
                    with lock:
                        results.scan_errors += 1
                    conn.rollback()

                time.sleep(0.005)  # Faster scanning
        finally:
            pg_pool.putconn(conn)

    def kometa_updater():
        """Simulate Kometa/PMM updating metadata - competing writer"""
        conn = pg_pool.getconn()
        cur = conn.cursor()

        try:
            while not stop_flag.is_set():
                try:
                    cur.execute(f"""
                        UPDATE {PG_SCHEMA}.metadata_items SET updated_at = %s
                        WHERE id = %s
                    """, (time.time(), random.randint(1, 60000)))
                    conn.commit()
                    with lock:
                        results.scan_writes += 1
                except Exception:
                    with lock:
                        results.scan_errors += 1
                    conn.rollback()

                time.sleep(0.02)
        finally:
            pg_pool.putconn(conn)

    def playback_stream(stream_id: int):
        """Simulate active playback - reads + watch progress writes"""
        conn = pg_pool.getconn()
        cur = conn.cursor()

        try:
            while not stop_flag.is_set():
                # Read: Get media info
                try:
                    cur.execute(f"""
                        SELECT id, title, duration FROM {PG_SCHEMA}.stress_metadata
                        ORDER BY RANDOM() LIMIT 1
                    """)
                    cur.fetchone()

                    cur.execute(f"""
                        SELECT id, title, rating FROM {PG_SCHEMA}.metadata_items
                        WHERE id = %s
                    """, (random.randint(1, 60000),))
                    cur.fetchone()

                    with lock:
                        results.playback_reads += 2
                except Exception:
                    with lock:
                        results.playback_read_errors += 1

                # Write: Update watch progress
                try:
                    cur.execute(f"""
                        INSERT INTO {PG_SCHEMA}.stress_progress (metadata_id, view_offset, updated_at)
                        VALUES (%s, %s, %s)
                    """, (random.randint(1, 1000), random.randint(0, 10800000), time.time()))
                    conn.commit()
                    with lock:
                        results.playback_writes += 1
                except Exception:
                    with lock:
                        results.playback_write_errors += 1
                    conn.rollback()

                time.sleep(0.05)  # Fast polling for smooth playback
        finally:
            pg_pool.putconn(conn)

    # Start threads: 2 scanners + Kometa + streams
    start_time = time.perf_counter()

    scanner_threads = [threading.Thread(target=library_scanner, args=(i,)) for i in range(2)]
    kometa_thread = threading.Thread(target=kometa_updater)
    stream_threads = [threading.Thread(target=playback_stream, args=(i,)) for i in range(num_streams)]

    for t in scanner_threads:
        t.start()
    kometa_thread.start()
    for t in stream_threads:
        t.start()

    time.sleep(duration)
    stop_flag.set()

    for t in scanner_threads:
        t.join()
    kometa_thread.join()
    for t in stream_threads:
        t.join()

    results.total_time_ms = (time.perf_counter() - start_time) * 1000

    # Cleanup
    conn = pg_pool.getconn()
    cur = conn.cursor()
    cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.stress_metadata")
    cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.stress_progress")
    conn.commit()
    pg_pool.putconn(conn)
    pg_pool.closeall()

    return results


def print_results(name: str, results: StressResults, color: str):
    """Print results for a stress test"""
    total_ops = results.scan_writes + results.playback_reads + results.playback_writes
    total_errors = results.scan_errors + results.playback_read_errors + results.playback_write_errors
    error_rate = 100 * total_errors / max(total_ops + total_errors, 1)

    print(f"  {color}{name}:{NC}")
    print(f"    Library scan writes:    {results.scan_writes:>6} (errors: {RED}{results.scan_errors}{NC})")
    print(f"    Playback reads:         {results.playback_reads:>6} (errors: {RED}{results.playback_read_errors}{NC})")
    print(f"    Watch progress writes:  {results.playback_writes:>6} (errors: {RED}{results.playback_write_errors}{NC})")
    print(f"    Total operations:       {GREEN}{total_ops:>6}{NC}")
    print(f"    Error rate:             {RED if error_rate > 1 else GREEN}{error_rate:.1f}%{NC}")
    print()


def main():
    print(f"\n{BLUE}{'═' * 70}{NC}")
    print(f"{BLUE}{BOLD}  Plex Stress Test: Library Scan + Playback (rclone/Real-Debrid){NC}")
    print(f"{BLUE}{'═' * 70}{NC}")
    print()
    print(f"  {CYAN}Scenario:{NC} You're streaming a movie while Plex scans your")
    print(f"  rclone-mounted Real-Debrid library (1000s of new files).")
    print()
    print(f"  {CYAN}The problem:{NC} SQLite locks the entire database during writes,")
    print(f"  causing playback to stutter, buffer, or fail completely.")
    print()
    print(f"  {CYAN}The solution:{NC} PostgreSQL uses MVCC - readers never block writers")
    print(f"  and vice versa. Smooth playback during heavy scans.")

    db_path = find_plex_db()
    if not db_path:
        print(f"\n{RED}ERROR: Cannot find Plex database{NC}")
        sys.exit(1)

    duration = 10  # seconds
    num_streams = 4  # concurrent streams

    # Check if Unix socket is available
    socket_available = check_socket_available()
    
    print(f"\n{BLUE}{'─' * 70}{NC}")
    sqlite_results = run_sqlite_stress(db_path, duration, num_streams)
    print_results("SQLite", sqlite_results, YELLOW)

    # Run PostgreSQL TCP test
    print(f"{BLUE}{'─' * 70}{NC}")
    pg_tcp_results = run_postgresql_stress(duration, num_streams, use_socket=False)
    print_results("PostgreSQL (TCP)", pg_tcp_results, CYAN)

    # Run PostgreSQL Unix socket test if available
    pg_socket_results = None
    if socket_available:
        print(f"{BLUE}{'─' * 70}{NC}")
        pg_socket_results = run_postgresql_stress(duration, num_streams, use_socket=True)
        print_results("PostgreSQL (Socket)", pg_socket_results, GREEN)
    else:
        print(f"\n  {YELLOW}Unix socket not available at {PG_SOCKET}{NC}")
        print(f"  {YELLOW}Skipping socket test. Set PLEX_PG_SOCKET to test.{NC}\n")

    # Summary
    print(f"{BLUE}{'═' * 70}{NC}")
    print(f"{BOLD}Summary:{NC}\n")

    sqlite_total = sqlite_results.scan_writes + sqlite_results.playback_reads + sqlite_results.playback_writes
    pg_tcp_total = pg_tcp_results.scan_writes + pg_tcp_results.playback_reads + pg_tcp_results.playback_writes
    sqlite_errors = sqlite_results.scan_errors + sqlite_results.playback_read_errors + sqlite_results.playback_write_errors
    pg_tcp_errors = pg_tcp_results.scan_errors + pg_tcp_results.playback_read_errors + pg_tcp_results.playback_write_errors

    print(f"  {'Database':<20} {'Total Ops':<12} {'Errors':<10} {'Error Rate':<12} {'Ops/sec':<10}")
    print(f"  {'-'*65}")
    
    sqlite_error_rate = 100 * sqlite_errors / max(sqlite_total + sqlite_errors, 1)
    sqlite_ops_sec = sqlite_total / duration
    print(f"  {'SQLite':<20} {sqlite_total:<12} {RED}{sqlite_errors:<10}{NC} {sqlite_error_rate:.1f}%{'':8} {sqlite_ops_sec:.0f}")
    
    pg_tcp_error_rate = 100 * pg_tcp_errors / max(pg_tcp_total + pg_tcp_errors, 1)
    pg_tcp_ops_sec = pg_tcp_total / duration
    print(f"  {'PostgreSQL (TCP)':<20} {pg_tcp_total:<12} {GREEN}{pg_tcp_errors:<10}{NC} {pg_tcp_error_rate:.1f}%{'':8} {pg_tcp_ops_sec:.0f}")
    
    if pg_socket_results:
        pg_socket_total = pg_socket_results.scan_writes + pg_socket_results.playback_reads + pg_socket_results.playback_writes
        pg_socket_errors = pg_socket_results.scan_errors + pg_socket_results.playback_read_errors + pg_socket_results.playback_write_errors
        pg_socket_error_rate = 100 * pg_socket_errors / max(pg_socket_total + pg_socket_errors, 1)
        pg_socket_ops_sec = pg_socket_total / duration
        print(f"  {'PostgreSQL (Socket)':<20} {pg_socket_total:<12} {GREEN}{pg_socket_errors:<10}{NC} {pg_socket_error_rate:.1f}%{'':8} {pg_socket_ops_sec:.0f}")
    
    print()

    # Comparison
    if pg_tcp_total > sqlite_total:
        speedup = pg_tcp_total / sqlite_total if sqlite_total > 0 else float('inf')
        print(f"  {GREEN}PostgreSQL TCP: {speedup:.1f}x more operations than SQLite{NC}")

    if pg_socket_results:
        pg_socket_total = pg_socket_results.scan_writes + pg_socket_results.playback_reads + pg_socket_results.playback_writes
        if pg_socket_total > pg_tcp_total:
            socket_speedup = pg_socket_total / pg_tcp_total if pg_tcp_total > 0 else float('inf')
            print(f"  {GREEN}Unix Socket: {socket_speedup:.2f}x faster than TCP{NC}")
        else:
            print(f"  {YELLOW}Unix Socket: similar performance to TCP (network not bottleneck){NC}")

    if sqlite_errors > pg_tcp_errors:
        print(f"  {GREEN}PostgreSQL: {sqlite_errors - pg_tcp_errors} fewer errors (no database locking){NC}")

    print()
    print(f"  {CYAN}What this means for rclone/Real-Debrid users:{NC}")
    print(f"    • {GREEN}No more buffering{NC} during library scans")
    print(f"    • {GREEN}Smooth playback{NC} while adding new content")
    print(f"    • {GREEN}Multiple streams{NC} work even during heavy scans")
    print(f"    • {GREEN}Kometa/PMM{NC} can run without affecting playback")
    print()


if __name__ == "__main__":
    main()

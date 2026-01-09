#!/usr/bin/env python3
"""
SQLite vs PostgreSQL Direct Comparison Benchmark

Fair comparison using native drivers (no CLI overhead):
- sqlite3 module (in-process)
- psycopg2 (connection pooling)

Usage: python3 scripts/benchmark_compare.py
"""

import os
import sys
import time
import sqlite3
import threading
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor, as_completed

try:
    import psycopg2
    from psycopg2 import pool
except ImportError:
    print("ERROR: psycopg2 not installed. Run: pip install psycopg2-binary")
    sys.exit(1)

# Colors
GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
BLUE = "\033[34m"
CYAN = "\033[36m"
BOLD = "\033[1m"
NC = "\033[0m"

# Find Plex SQLite database
def find_plex_db():
    paths = [
        Path.home() / "Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db",
        Path.home() / "Library/Application Support/Plex Media Server/Plug-in Support/Databases/shadow/com.plexapp.plugins.library.db",
        Path("/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db"),
    ]
    for p in paths:
        if p.exists():
            return str(p)
    return None

# PostgreSQL config
PG_CONFIG = {
    "host": os.environ.get("PLEX_PG_HOST", "localhost"),
    "port": int(os.environ.get("PLEX_PG_PORT", 5432)),
    "database": os.environ.get("PLEX_PG_DATABASE", "plex"),
    "user": os.environ.get("PLEX_PG_USER", "plex"),
    "password": os.environ.get("PLEX_PG_PASSWORD", "plex"),
}
PG_SCHEMA = os.environ.get("PLEX_PG_SCHEMA", "plex")

def run_benchmark(name, sqlite_func, pg_func, iterations=100):
    """Run a benchmark comparing SQLite vs PostgreSQL"""
    print(f"{YELLOW}[{name}]{NC} ({iterations} iterations)")

    # SQLite
    start = time.perf_counter()
    for _ in range(iterations):
        sqlite_func()
    sqlite_time = (time.perf_counter() - start) * 1000
    sqlite_per = sqlite_time / iterations

    # PostgreSQL
    start = time.perf_counter()
    for _ in range(iterations):
        pg_func()
    pg_time = (time.perf_counter() - start) * 1000
    pg_per = pg_time / iterations

    # Results
    print(f"  SQLite:     {sqlite_per:6.2f} ms/query")
    print(f"  PostgreSQL: {pg_per:6.2f} ms/query")

    if sqlite_per < pg_per:
        speedup = pg_per / sqlite_per
        print(f"  Winner:     {RED}SQLite ({speedup:.1f}x faster){NC}")
    else:
        speedup = sqlite_per / pg_per
        print(f"  Winner:     {GREEN}PostgreSQL ({speedup:.1f}x faster){NC}")
    print()

    return sqlite_per, pg_per

def run_concurrent_benchmark(name, sqlite_func, pg_func, clients=10, queries_per=50):
    """Run concurrent benchmark"""
    print(f"{YELLOW}[{name}]{NC} ({clients} clients, {queries_per} queries each)")

    total_queries = clients * queries_per

    # SQLite concurrent
    def sqlite_worker():
        conn = sqlite3.connect(PLEX_DB, check_same_thread=False)
        for _ in range(queries_per):
            sqlite_func(conn)
        conn.close()

    start = time.perf_counter()
    threads = [threading.Thread(target=sqlite_worker) for _ in range(clients)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    sqlite_time = (time.perf_counter() - start) * 1000
    sqlite_qps = total_queries / (sqlite_time / 1000)

    # PostgreSQL concurrent (with connection pool)
    pg_pool = pool.ThreadedConnectionPool(1, clients + 5, **PG_CONFIG)

    def pg_worker():
        conn = pg_pool.getconn()
        try:
            for _ in range(queries_per):
                pg_func(conn)
        finally:
            pg_pool.putconn(conn)

    start = time.perf_counter()
    threads = [threading.Thread(target=pg_worker) for _ in range(clients)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    pg_time = (time.perf_counter() - start) * 1000
    pg_qps = total_queries / (pg_time / 1000)

    pg_pool.closeall()

    # Results
    print(f"  SQLite:     {sqlite_time:.0f}ms total ({sqlite_qps:.0f} queries/sec)")
    print(f"  PostgreSQL: {pg_time:.0f}ms total ({pg_qps:.0f} queries/sec)")

    if sqlite_qps > pg_qps:
        speedup = sqlite_qps / pg_qps
        print(f"  Winner:     {RED}SQLite ({speedup:.1f}x more throughput){NC}")
    else:
        speedup = pg_qps / sqlite_qps
        print(f"  Winner:     {GREEN}PostgreSQL ({speedup:.1f}x more throughput){NC}")
    print()

    return sqlite_qps, pg_qps

def run_mixed_workload(name, duration=5):
    """Run mixed read/write workload - SQLite's weakness"""
    print(f"{YELLOW}[{name}]{NC} ({duration}s, 5 readers + 3 writers)")
    print(f"  SQLite locks entire DB during writes, PostgreSQL doesn't...")
    print()

    sqlite_reads = [0]
    sqlite_writes = [0]
    pg_reads = [0]
    pg_writes = [0]
    stop_flag = [False]

    # SQLite reader
    def sqlite_reader():
        conn = sqlite3.connect(PLEX_DB, timeout=30)
        while not stop_flag[0]:
            try:
                conn.execute("SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 1").fetchone()
                sqlite_reads[0] += 1
            except sqlite3.OperationalError:
                pass  # Database locked
        conn.close()

    # SQLite writer
    def sqlite_writer():
        conn = sqlite3.connect(PLEX_DB, timeout=30)
        conn.execute("CREATE TABLE IF NOT EXISTS benchmark_writes (id INTEGER PRIMARY KEY, val INTEGER)")
        while not stop_flag[0]:
            try:
                conn.execute("INSERT INTO benchmark_writes (val) VALUES (?)", (42,))
                conn.commit()
                sqlite_writes[0] += 1
            except sqlite3.OperationalError:
                pass  # Database locked
        conn.close()

    # Run SQLite mixed workload
    threads = []
    for _ in range(5):
        threads.append(threading.Thread(target=sqlite_reader))
    for _ in range(3):
        threads.append(threading.Thread(target=sqlite_writer))

    for t in threads:
        t.start()
    time.sleep(duration)
    stop_flag[0] = True
    for t in threads:
        t.join()

    # Cleanup SQLite
    conn = sqlite3.connect(PLEX_DB)
    conn.execute("DROP TABLE IF EXISTS benchmark_writes")
    conn.commit()
    conn.close()

    print(f"  SQLite:     {sqlite_reads[0]} reads + {sqlite_writes[0]} writes")

    # Reset
    stop_flag[0] = False

    # PostgreSQL pool
    pg_pool = pool.ThreadedConnectionPool(1, 20, **PG_CONFIG)

    # Setup
    conn = pg_pool.getconn()
    cur = conn.cursor()
    cur.execute(f"CREATE TABLE IF NOT EXISTS {PG_SCHEMA}.benchmark_writes (id SERIAL, val INTEGER)")
    conn.commit()
    pg_pool.putconn(conn)

    # PostgreSQL reader
    def pg_reader():
        conn = pg_pool.getconn()
        try:
            cur = conn.cursor()
            while not stop_flag[0]:
                cur.execute(f"SELECT COUNT(*) FROM {PG_SCHEMA}.metadata_items WHERE metadata_type = 1")
                cur.fetchone()
                pg_reads[0] += 1
        finally:
            pg_pool.putconn(conn)

    # PostgreSQL writer
    def pg_writer():
        conn = pg_pool.getconn()
        try:
            cur = conn.cursor()
            while not stop_flag[0]:
                cur.execute(f"INSERT INTO {PG_SCHEMA}.benchmark_writes (val) VALUES (%s)", (42,))
                conn.commit()
                pg_writes[0] += 1
        finally:
            pg_pool.putconn(conn)

    # Run PostgreSQL mixed workload
    threads = []
    for _ in range(5):
        threads.append(threading.Thread(target=pg_reader))
    for _ in range(3):
        threads.append(threading.Thread(target=pg_writer))

    for t in threads:
        t.start()
    time.sleep(duration)
    stop_flag[0] = True
    for t in threads:
        t.join()

    # Cleanup PostgreSQL
    conn = pg_pool.getconn()
    cur = conn.cursor()
    cur.execute(f"DROP TABLE IF EXISTS {PG_SCHEMA}.benchmark_writes")
    conn.commit()
    pg_pool.putconn(conn)
    pg_pool.closeall()

    print(f"  PostgreSQL: {pg_reads[0]} reads + {pg_writes[0]} writes")

    sqlite_total = sqlite_reads[0] + sqlite_writes[0]
    pg_total = pg_reads[0] + pg_writes[0]

    if pg_total > sqlite_total:
        speedup = pg_total / sqlite_total if sqlite_total > 0 else float('inf')
        print(f"  Winner:     {GREEN}PostgreSQL ({speedup:.1f}x more operations){NC}")
    else:
        speedup = sqlite_total / pg_total if pg_total > 0 else float('inf')
        print(f"  Winner:     {RED}SQLite ({speedup:.1f}x more operations){NC}")
    print()

def main():
    global PLEX_DB, sqlite_conn, pg_conn

    print(f"\n{BLUE}{'═' * 64}{NC}")
    print(f"{BLUE}{BOLD}     SQLite vs PostgreSQL Direct Comparison Benchmark{NC}")
    print(f"{BLUE}{'═' * 64}{NC}\n")

    # Find databases
    PLEX_DB = find_plex_db()
    if not PLEX_DB:
        print(f"{RED}ERROR: Cannot find Plex SQLite database{NC}")
        sys.exit(1)

    print(f"SQLite:     {CYAN}{PLEX_DB}{NC}")
    print(f"PostgreSQL: {CYAN}{PG_CONFIG['host']}:{PG_CONFIG['port']}/{PG_CONFIG['database']}{NC}")
    print()

    # Connect
    sqlite_conn = sqlite3.connect(PLEX_DB)
    sqlite_count = sqlite_conn.execute("SELECT COUNT(*) FROM metadata_items").fetchone()[0]
    print(f"SQLite items:     {GREEN}{sqlite_count}{NC}")

    try:
        pg_conn = psycopg2.connect(**PG_CONFIG)
        pg_cur = pg_conn.cursor()
        pg_cur.execute(f"SELECT COUNT(*) FROM {PG_SCHEMA}.metadata_items")
        pg_count = pg_cur.fetchone()[0]
        print(f"PostgreSQL items: {GREEN}{pg_count}{NC}")
    except Exception as e:
        print(f"{RED}ERROR: Cannot connect to PostgreSQL: {e}{NC}")
        sys.exit(1)

    print(f"\n{BLUE}{'═' * 64}{NC}\n")

    # Benchmark 1: Simple SELECT by ID
    def sqlite_select_id():
        sqlite_conn.execute("SELECT id, title, rating FROM metadata_items WHERE id = 1000").fetchone()

    def pg_select_id():
        pg_cur.execute(f"SELECT id, title, rating FROM {PG_SCHEMA}.metadata_items WHERE id = 1000")
        pg_cur.fetchone()

    run_benchmark("Simple SELECT by ID", sqlite_select_id, pg_select_id, 1000)

    # Benchmark 2: SELECT with LIKE
    def sqlite_like():
        sqlite_conn.execute("SELECT id, title FROM metadata_items WHERE title LIKE '%The%' LIMIT 100").fetchall()

    def pg_like():
        pg_cur.execute(f"SELECT id, title FROM {PG_SCHEMA}.metadata_items WHERE title LIKE '%The%' LIMIT 100")
        pg_cur.fetchall()

    run_benchmark("SELECT with LIKE pattern", sqlite_like, pg_like, 500)

    # Benchmark 3: COUNT aggregate
    def sqlite_count():
        sqlite_conn.execute("SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 1").fetchone()

    def pg_count():
        pg_cur.execute(f"SELECT COUNT(*) FROM {PG_SCHEMA}.metadata_items WHERE metadata_type = 1")
        pg_cur.fetchone()

    run_benchmark("COUNT aggregate", sqlite_count, pg_count, 500)

    # Benchmark 4: JOIN
    def sqlite_join():
        sqlite_conn.execute("""
            SELECT m.title, mi.duration
            FROM metadata_items m
            JOIN media_items mi ON mi.metadata_item_id = m.id
            WHERE m.metadata_type = 1 LIMIT 100
        """).fetchall()

    def pg_join():
        pg_cur.execute(f"""
            SELECT m.title, mi.duration
            FROM {PG_SCHEMA}.metadata_items m
            JOIN {PG_SCHEMA}.media_items mi ON mi.metadata_item_id = m.id
            WHERE m.metadata_type = 1 LIMIT 100
        """)
        pg_cur.fetchall()

    run_benchmark("JOIN query", sqlite_join, pg_join, 200)

    # Benchmark 5: ORDER BY + LIMIT
    def sqlite_order():
        sqlite_conn.execute("""
            SELECT id, title, added_at FROM metadata_items
            WHERE metadata_type = 1
            ORDER BY added_at DESC LIMIT 50
        """).fetchall()

    def pg_order():
        pg_cur.execute(f"""
            SELECT id, title, added_at FROM {PG_SCHEMA}.metadata_items
            WHERE metadata_type = 1
            ORDER BY added_at DESC LIMIT 50
        """)
        pg_cur.fetchall()

    run_benchmark("ORDER BY + LIMIT", sqlite_order, pg_order, 500)

    print(f"{BLUE}{'═' * 64}{NC}\n")
    print(f"{BOLD}Concurrent Access Tests{NC} (where PostgreSQL shines)\n")

    # Benchmark 6: Concurrent reads
    def sqlite_concurrent_read(conn):
        conn.execute("SELECT id, title FROM metadata_items WHERE id = ?", (1000,)).fetchone()

    def pg_concurrent_read(conn):
        cur = conn.cursor()
        cur.execute(f"SELECT id, title FROM {PG_SCHEMA}.metadata_items WHERE id = %s", (1000,))
        cur.fetchone()

    run_concurrent_benchmark("Concurrent Reads", sqlite_concurrent_read, pg_concurrent_read, clients=10, queries_per=100)

    # Benchmark 7: Mixed read/write
    run_mixed_workload("Mixed Read+Write Workload", duration=5)

    # Cleanup
    sqlite_conn.close()
    pg_conn.close()

    print(f"{BLUE}{'═' * 64}{NC}\n")
    print(f"{CYAN}Summary:{NC}")
    print(f"  • Single-query: SQLite often faster (embedded, no network)")
    print(f"  • Concurrent reads: Depends on workload")
    print(f"  • Mixed read+write: {GREEN}PostgreSQL wins{NC} (no locking)")
    print()
    print(f"{CYAN}For Plex, PostgreSQL wins when:{NC}")
    print(f"  • Library scanning while streaming")
    print(f"  • Multiple concurrent streams")
    print(f"  • Kometa/PMM running metadata updates")
    print()

if __name__ == "__main__":
    main()

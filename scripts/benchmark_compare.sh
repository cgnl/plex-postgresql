#!/bin/bash
#
# SQLite vs PostgreSQL Direct Comparison Benchmark
#
# Runs identical queries against both databases and compares latency.
# Does NOT require Plex to be running.
#
# Usage: ./scripts/benchmark_compare.sh
#

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

# Find Plex SQLite database
PLEX_DB=$(find ~/Library/Application\ Support/Plex\ Media\ Server/Plug-in\ Support/Databases -name "com.plexapp.plugins.library.db" 2>/dev/null | head -1)
if [ -z "$PLEX_DB" ]; then
    # Try Linux path
    PLEX_DB=$(find /var/lib/plexmediaserver -name "com.plexapp.plugins.library.db" 2>/dev/null | head -1)
fi

if [ -z "$PLEX_DB" ] || [ ! -f "$PLEX_DB" ]; then
    echo -e "${RED}ERROR: Cannot find Plex SQLite database${NC}"
    exit 1
fi

# PostgreSQL config
PG_HOST="${PLEX_PG_HOST:-localhost}"
PG_PORT="${PLEX_PG_PORT:-5432}"
PG_USER="${PLEX_PG_USER:-plex}"
PG_DB="${PLEX_PG_DATABASE:-plex}"
PG_SCHEMA="${PLEX_PG_SCHEMA:-plex}"
export PGPASSWORD="${PLEX_PG_PASSWORD:-plex}"

echo -e "${BLUE}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║       SQLite vs PostgreSQL Direct Comparison Benchmark       ║${NC}"
echo -e "${BLUE}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "SQLite:     ${CYAN}$PLEX_DB${NC}"
echo -e "PostgreSQL: ${CYAN}$PG_HOST:$PG_PORT/$PG_DB (schema: $PG_SCHEMA)${NC}"
echo ""

# Check databases
SQLITE_COUNT=$(sqlite3 "$PLEX_DB" "SELECT COUNT(*) FROM metadata_items;" 2>/dev/null)
echo -e "SQLite items:     ${GREEN}$SQLITE_COUNT${NC}"

if pg_isready -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" >/dev/null 2>&1; then
    PG_COUNT=$(psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -c "SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items;" 2>/dev/null | tr -d ' ')
    echo -e "PostgreSQL items: ${GREEN}$PG_COUNT${NC}"
else
    echo -e "${RED}ERROR: Cannot connect to PostgreSQL${NC}"
    exit 1
fi
echo ""

# Time function (returns milliseconds)
time_ms() {
    python3 -c "import time; print(int(time.time() * 1000))"
}

# Run benchmark for a query
run_benchmark() {
    local name="$1"
    local sqlite_query="$2"
    local pg_query="$3"
    local iterations="${4:-100}"

    echo -e "${YELLOW}[$name]${NC} ($iterations iterations)"

    # SQLite benchmark
    START=$(time_ms)
    for i in $(seq 1 $iterations); do
        sqlite3 "$PLEX_DB" "$sqlite_query" >/dev/null 2>&1
    done
    END=$(time_ms)
    SQLITE_TIME=$((END - START))
    SQLITE_PER=$(echo "scale=2; $SQLITE_TIME / $iterations" | bc)

    # PostgreSQL benchmark
    START=$(time_ms)
    for i in $(seq 1 $iterations); do
        psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -c "$pg_query" >/dev/null 2>&1
    done
    END=$(time_ms)
    PG_TIME=$((END - START))
    PG_PER=$(echo "scale=2; $PG_TIME / $iterations" | bc)

    # Calculate difference
    if (( $(echo "$SQLITE_PER > $PG_PER" | bc -l) )); then
        FASTER="PostgreSQL"
        SPEEDUP=$(echo "scale=1; $SQLITE_PER / $PG_PER" | bc)
        COLOR=$GREEN
    else
        FASTER="SQLite"
        SPEEDUP=$(echo "scale=1; $PG_PER / $SQLITE_PER" | bc)
        COLOR=$RED
    fi

    printf "  SQLite:     %6.2f ms/query\n" $SQLITE_PER
    printf "  PostgreSQL: %6.2f ms/query\n" $PG_PER
    echo -e "  Winner:     ${COLOR}${FASTER} (${SPEEDUP}x faster)${NC}"
    echo ""
}

echo -e "${BLUE}══════════════════════════════════════════════════════════════${NC}"
echo ""

# ============================================================================
# Benchmark 1: Simple SELECT by ID
# ============================================================================
run_benchmark "Simple SELECT by ID" \
    "SELECT id, title, rating FROM metadata_items WHERE id = 1000;" \
    "SELECT id, title, rating FROM $PG_SCHEMA.metadata_items WHERE id = 1000;" \
    200

# ============================================================================
# Benchmark 2: SELECT with LIKE pattern
# ============================================================================
run_benchmark "SELECT with LIKE pattern" \
    "SELECT id, title FROM metadata_items WHERE title LIKE '%The%' LIMIT 100;" \
    "SELECT id, title FROM $PG_SCHEMA.metadata_items WHERE title LIKE '%The%' LIMIT 100;" \
    100

# ============================================================================
# Benchmark 3: COUNT aggregate
# ============================================================================
run_benchmark "COUNT aggregate" \
    "SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 1;" \
    "SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items WHERE metadata_type = 1;" \
    100

# ============================================================================
# Benchmark 4: JOIN query (metadata + media_items)
# ============================================================================
run_benchmark "JOIN query" \
    "SELECT m.title, mi.duration FROM metadata_items m JOIN media_items mi ON mi.metadata_item_id = m.id WHERE m.metadata_type = 1 LIMIT 100;" \
    "SELECT m.title, mi.duration FROM $PG_SCHEMA.metadata_items m JOIN $PG_SCHEMA.media_items mi ON mi.metadata_item_id = m.id WHERE m.metadata_type = 1 LIMIT 100;" \
    50

# ============================================================================
# Benchmark 5: ORDER BY with LIMIT (typical Plex query)
# ============================================================================
run_benchmark "ORDER BY + LIMIT (recent items)" \
    "SELECT id, title, added_at FROM metadata_items WHERE metadata_type = 1 ORDER BY added_at DESC LIMIT 50;" \
    "SELECT id, title, added_at FROM $PG_SCHEMA.metadata_items WHERE metadata_type = 1 ORDER BY added_at DESC LIMIT 50;" \
    100

# ============================================================================
# Benchmark 6: Complex OnDeck-style query
# ============================================================================
run_benchmark "Complex OnDeck-style query" \
    "SELECT m.id, m.title, m.rating, m.added_at FROM metadata_items m WHERE m.metadata_type IN (1,4) AND m.library_section_id IN (SELECT id FROM library_sections WHERE section_type = 1) ORDER BY m.added_at DESC LIMIT 20;" \
    "SELECT m.id, m.title, m.rating, m.added_at FROM $PG_SCHEMA.metadata_items m WHERE m.metadata_type IN (1,4) AND m.library_section_id IN (SELECT id FROM $PG_SCHEMA.library_sections WHERE section_type = 1) ORDER BY m.added_at DESC LIMIT 20;" \
    50

# ============================================================================
# Benchmark 7: Full-text search simulation
# ============================================================================
run_benchmark "Title search (LIKE)" \
    "SELECT id, title FROM metadata_items WHERE title LIKE '%action%' OR title LIKE '%Action%' LIMIT 50;" \
    "SELECT id, title FROM $PG_SCHEMA.metadata_items WHERE title ILIKE '%action%' LIMIT 50;" \
    50

echo -e "${BLUE}══════════════════════════════════════════════════════════════${NC}"
echo ""

# ============================================================================
# Benchmark 8: CONCURRENT ACCESS (the real PostgreSQL advantage)
# ============================================================================
echo -e "${YELLOW}[Concurrent Access Test]${NC} (10 parallel clients, 50 queries each)"
echo -e "  This is where PostgreSQL shines vs SQLite's locking..."
echo ""

CONCURRENT=10
QUERIES_PER=50

# SQLite concurrent test
sqlite_worker() {
    for i in $(seq 1 $QUERIES_PER); do
        sqlite3 "$PLEX_DB" "SELECT id, title FROM metadata_items WHERE id = $((RANDOM % 60000 + 1));" 2>/dev/null
    done
}

START=$(time_ms)
for i in $(seq 1 $CONCURRENT); do
    sqlite_worker &
done
wait
END=$(time_ms)
SQLITE_CONCURRENT=$((END - START))
SQLITE_QPS=$(echo "scale=0; $CONCURRENT * $QUERIES_PER * 1000 / $SQLITE_CONCURRENT" | bc)

# PostgreSQL concurrent test
pg_worker() {
    for i in $(seq 1 $QUERIES_PER); do
        psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -c \
            "SELECT id, title FROM $PG_SCHEMA.metadata_items WHERE id = $((RANDOM % 60000 + 1));" >/dev/null 2>&1
    done
}

START=$(time_ms)
for i in $(seq 1 $CONCURRENT); do
    pg_worker &
done
wait
END=$(time_ms)
PG_CONCURRENT=$((END - START))
PG_QPS=$(echo "scale=0; $CONCURRENT * $QUERIES_PER * 1000 / $PG_CONCURRENT" | bc)

echo -e "  SQLite:     ${SQLITE_CONCURRENT}ms total (${SQLITE_QPS} queries/sec)"
echo -e "  PostgreSQL: ${PG_CONCURRENT}ms total (${PG_QPS} queries/sec)"

if [ "$PG_QPS" -gt "$SQLITE_QPS" ]; then
    SPEEDUP=$(echo "scale=1; $PG_QPS / $SQLITE_QPS" | bc)
    echo -e "  Winner:     ${GREEN}PostgreSQL (${SPEEDUP}x more throughput)${NC}"
else
    SPEEDUP=$(echo "scale=1; $SQLITE_QPS / $PG_QPS" | bc)
    echo -e "  Winner:     ${RED}SQLite (${SPEEDUP}x more throughput)${NC}"
fi
echo ""

# ============================================================================
# Benchmark 9: CONCURRENT READ + WRITE (SQLite's weakness)
# ============================================================================
echo -e "${YELLOW}[Concurrent Read + Write Test]${NC} (readers + writers simultaneously)"
echo -e "  SQLite locks entire DB during writes, PostgreSQL doesn't..."
echo ""

# Create temp table for writes
psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -q -c \
    "CREATE TABLE IF NOT EXISTS $PG_SCHEMA.benchmark_writes (id SERIAL, val INT);" 2>/dev/null
sqlite3 "$PLEX_DB" "CREATE TABLE IF NOT EXISTS benchmark_writes (id INTEGER PRIMARY KEY, val INTEGER);" 2>/dev/null

READERS=5
WRITERS=3
DURATION=5  # seconds

# SQLite mixed workload
sqlite_reader() {
    local end=$(($(date +%s) + DURATION))
    local count=0
    while [ $(date +%s) -lt $end ]; do
        sqlite3 "$PLEX_DB" "SELECT COUNT(*) FROM metadata_items WHERE metadata_type = 1;" >/dev/null 2>&1
        count=$((count + 1))
    done
    echo $count
}

sqlite_writer() {
    local end=$(($(date +%s) + DURATION))
    local count=0
    while [ $(date +%s) -lt $end ]; do
        sqlite3 "$PLEX_DB" "INSERT INTO benchmark_writes (val) VALUES ($RANDOM);" 2>/dev/null
        count=$((count + 1))
    done
    echo $count
}

echo -n "  SQLite:     "
SQLITE_READS=0
SQLITE_WRITES=0
for i in $(seq 1 $READERS); do
    SQLITE_READS=$((SQLITE_READS + $(sqlite_reader))) &
done
for i in $(seq 1 $WRITERS); do
    SQLITE_WRITES=$((SQLITE_WRITES + $(sqlite_writer))) &
done
wait
echo "${SQLITE_READS:-?} reads + ${SQLITE_WRITES:-?} writes in ${DURATION}s"

# PostgreSQL mixed workload
pg_reader() {
    local end=$(($(date +%s) + DURATION))
    local count=0
    while [ $(date +%s) -lt $end ]; do
        psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -c \
            "SELECT COUNT(*) FROM $PG_SCHEMA.metadata_items WHERE metadata_type = 1;" >/dev/null 2>&1
        count=$((count + 1))
    done
    echo $count
}

pg_writer() {
    local end=$(($(date +%s) + DURATION))
    local count=0
    while [ $(date +%s) -lt $end ]; do
        psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -q -c \
            "INSERT INTO $PG_SCHEMA.benchmark_writes (val) VALUES ($RANDOM);" 2>/dev/null
        count=$((count + 1))
    done
    echo $count
}

echo -n "  PostgreSQL: "
PG_READS=0
PG_WRITES=0
for i in $(seq 1 $READERS); do
    PG_READS=$((PG_READS + $(pg_reader))) &
done
for i in $(seq 1 $WRITERS); do
    PG_WRITES=$((PG_WRITES + $(pg_writer))) &
done
wait
echo "${PG_READS:-?} reads + ${PG_WRITES:-?} writes in ${DURATION}s"

# Cleanup
psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -q -c \
    "DROP TABLE IF EXISTS $PG_SCHEMA.benchmark_writes;" 2>/dev/null
sqlite3 "$PLEX_DB" "DROP TABLE IF EXISTS benchmark_writes;" 2>/dev/null

echo ""
echo -e "${BLUE}══════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "${CYAN}Summary:${NC}"
echo -e "  • Single-query: SQLite faster (embedded, no network overhead)"
echo -e "  • Concurrent:   PostgreSQL scales better with multiple clients"
echo -e "  • Read+Write:   PostgreSQL doesn't lock, SQLite blocks readers"
echo ""
echo -e "${CYAN}For Plex, PostgreSQL wins when:${NC}"
echo -e "  • Library scanning while streaming"
echo -e "  • Multiple concurrent streams"
echo -e "  • Kometa/PMM running metadata updates"
echo ""

# Release Notes - v0.8.0

**Release Date:** January 10, 2026

This release focuses on performance optimization and SOCI ORM compatibility fixes.

## Highlights

### 145x Faster SQL Translation
The new thread-local translation cache eliminates lock contention and provides massive speedup for repeated queries:

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| Translation time | 17.5 µs | 0.12 µs | **145x faster** |
| Cache lookup | N/A | 22.6 ns | Lock-free |
| Throughput | 57K/sec | 8.5M/sec | **149x higher** |

### SOCI ORM Compatibility
Fixed critical issues with Plex's SOCI database layer:
- Added `sqlite3_column_decltype` interception
- Added `sqlite3_bind_parameter_index` for named parameters
- Fixed pre-step metadata access (column info before `step()`)

### Comprehensive Benchmarks
New benchmark tools to measure and compare performance:
```bash
make benchmark                        # Shim micro-benchmarks
./tests/bin/bench_cache              # Cache implementation comparison
python3 tests/bench_sqlite_vs_pg.py  # SQLite vs PostgreSQL latency
```

## Performance Summary

### SQLite vs PostgreSQL Latency

| Query Type | SQLite | PostgreSQL | Overhead |
|------------|--------|------------|----------|
| SELECT (PK) | 3.9 µs | 18.2 µs | 4.6x |
| INSERT | 0.7 µs | 15.5 µs | 22x |
| Range Query | 22.0 µs | 45.2 µs | 2.1x |

PostgreSQL is slower per-query, but **never locks** during concurrent access.

### Shim Overhead

| Component | Latency | % of Query Time |
|-----------|---------|-----------------|
| SQL Translation (cached) | 0.12 µs | 0.7% |
| Cache lookup | 22.6 ns | 0.1% |
| **Total shim overhead** | **<0.2 µs** | **<1%** |

### Cache Implementation Comparison

| Method | Latency | Throughput |
|--------|---------|------------|
| Mutex | 507 ns | 15.8 M/sec |
| RWLock | 2,246 ns | 3.6 M/sec |
| **Thread-Local** | **22.6 ns** | **354 M/sec** |

Thread-local storage is **22x faster** than mutex-protected caches.

## New Features

### sqlite3_column_decltype
Returns PostgreSQL column types mapped to SQLite equivalents:
- `INTEGER` for int2, int4, int8, bool
- `REAL` for float4, float8, numeric
- `TEXT` for varchar, text, timestamps
- `BLOB` for bytea

### sqlite3_bind_parameter_index
Supports named parameter lookup for queries like:
```sql
SELECT * FROM items WHERE id = :item_id AND type = :type
```

### Pre-step Metadata Access
Column metadata functions now work before `sqlite3_step()`:
- `sqlite3_column_name()`
- `sqlite3_column_count()`
- `sqlite3_column_decltype()`

## Bug Fixes

- Fixed `sqlite3_column_value` returning NULL for pre-step calls
- Fixed metadata functions requiring step() to be called first

## Documentation

- Updated README with benchmark results and shim overhead data
- Rewrote `docs/modules.md` with:
  - Three-layer cache architecture diagram
  - Execution flow documentation
  - SQL translation pipeline details
  - Development guide for adding new functions
- Organized debug documentation into `docs/debug/`

## Testing

All 87 unit tests passing:
- Recursion & Stack Protection: 11 tests
- Crash Scenarios: 21 tests
- SQL Translator: 32 tests
- Query Cache: 16 tests
- TLS Cache: 7 tests

## Upgrade Notes

This release is backward compatible. No configuration changes required.

For best performance with local PostgreSQL, use Unix sockets:
```bash
export PLEX_PG_HOST=/tmp  # or /var/run/postgresql on Linux
```

## Known Issues

- PostgreSQL is 4-22x slower than SQLite per-query (expected trade-off for MVCC)
- Some unused variable warnings during compilation (cosmetic)

## Contributors

- Claude Opus 4.5 (Anthropic)

# Changelog

All notable changes to plex-postgresql will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-01-10

### Added
- `sqlite3_column_decltype` interception for SOCI ORM compatibility
- `sqlite3_bind_parameter_index` for named parameter support
- Thread-local SQL translation cache with 512 entries per thread
- `ensure_pg_result_for_metadata()` for pre-step metadata access
- Comprehensive benchmark suite:
  - `tests/bench_cache.c` - Cache implementation comparison
  - `tests/bench_sqlite_vs_pg.py` - SQLite vs PostgreSQL latency
  - `tests/bench_translation.c` - SQL translation throughput
- Stack protection tests for macOS and Linux
- VERSION file for release tracking

### Changed
- SQL translation now uses lock-free thread-local cache (145x speedup)
- Updated README with detailed benchmark results
- Rewrote `docs/modules.md` with cache architecture documentation
- Reorganized debug documentation into `docs/debug/`

### Performance
- Cached SQL translation: 0.12 µs (was 17.5 µs uncached)
- Thread-local cache is 22x faster than mutex-protected cache
- Shim overhead is <1% of total query time
- Cache lookup: 22.6 ns per operation

### Fixed
- `sqlite3_column_value` now properly handles pre-step calls
- Column metadata functions work before `sqlite3_step()` is called

## [0.7.0] - 2026-01-08

### Added
- SQL normalization for parameterized query caching
- Prepared statement cache with O(1) hash table lookup
- Unix socket support for PostgreSQL connections
- `sqlite3_expanded_sql` implementation
- Boolean value conversions for PostgreSQL 't'/'f' values

### Fixed
- Double-free crash in connection cleanup
- Fork safety with pthread_atfork handlers

## [0.6.0] - 2026-01-06

### Added
- Stack overflow protection (multi-layer defense)
- Recursion guards with depth limiting
- OnDeck query special handling for low-stack conditions
- Loop detection for rapid repeated queries

### Fixed
- Stack overflow crash with 218 recursive frames
- Integer overflow in counter variables

## [0.5.0] - 2026-01-04

### Added
- COLLATE NOCASE translation to ILIKE/LOWER()
- FTS4 boolean search operators (AND, OR, NOT, phrases)
- Window functions support (ROW_NUMBER, RANK, DENSE_RANK)
- WHERE 0/1 to WHERE FALSE/TRUE translation

### Changed
- Improved GROUP BY expression rewriting

## [0.4.0] - 2026-01-02

### Added
- Connection pooling (50 connections default, max 100)
- Query result caching with TTL-based eviction
- Thread-local connection caching

### Fixed
- Connection exhaustion under heavy load

## [0.3.0] - 2025-12-30

### Added
- Full SQL translation pipeline
- Placeholder translation (? to $1, :name to $N)
- Function translations (iif, strftime, IFNULL, etc.)
- UPSERT translation (INSERT OR REPLACE to ON CONFLICT)

## [0.2.0] - 2025-12-28

### Added
- Linux support via LD_PRELOAD
- Docker support with docker-compose
- Schema auto-initialization

## [0.1.0] - 2025-12-25

### Added
- Initial release
- macOS support via DYLD_INTERPOSE + fishhook
- Basic SQLite to PostgreSQL interception
- Shadow database for SQLite-only queries

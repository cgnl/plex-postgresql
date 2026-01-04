# Linux Shim Voortgang

## Status: In Progress

### Step 1: Refactor Linux to Use Modules
- [x] Update includes to use pg_*.h modules
- [x] Remove inline PG types (use pg_types.h)
- [x] Remove inline logging (use pg_logging.h)
- [x] Remove inline config (use pg_config.h)
- [x] Use pg_client.h for connection pool
- [x] Use pg_statement.h for statement registry
- [x] Keep LD_PRELOAD + dlsym(RTLD_NEXT) for SQLite interception
- [ ] Copy column access logic from macOS (inline in db_interpose_pg.c)
- [ ] Copy step/prepare logic from macOS

### Step 2: Update Makefile
- [ ] Add Linux objects
- [ ] Link with -lpq

### Step 3: Sync Features
- [x] Connection pool (via pg_client.h)
- [ ] Socket timeouts (need to copy from macOS)
- [x] Prepared statement cache (via pg_client.h)
- [x] sqlite3_value interception (fake value mechanism)

### Step 4: Wrapper Scripts
- [ ] install_wrappers_linux.sh
- [ ] uninstall_wrappers_linux.sh

### Step 5: Test
- [ ] Compile on Linux
- [ ] Docker test

---

## Log

### 2026-01-04 02:30
Started implementation.

### 2026-01-04 02:45
Created db_interpose_pg_linux_new.c with:
- Module includes (pg_types.h, pg_logging.h, pg_config.h, pg_client.h, pg_statement.h)
- dlsym(RTLD_NEXT) for original SQLite functions
- Basic interpose structure

**Issue discovered**: Column access logic (pg_column_int, etc.) is inline in macOS
db_interpose_pg.c, not in modules. Need to copy this logic to Linux version.

**Complexity**: macOS db_interpose_pg.c is 2458 lines with complex inline logic for:
- prepare_v2: SQL cleaning, FTS simplification, translation
- step: Write/read handling, result fetching
- column access: Type detection, value extraction from PGresult

**Next steps**:
1. Copy prepare_v2 logic to Linux (SQL translation, statement creation)
2. Copy step logic (write execution, read result handling)
3. Copy column access logic (PQgetvalue, type conversion)
4. Test compilation on Linux

**Files created**:
- `src/db_interpose_pg_linux_new.c` - New Linux version (WIP, ~600 lines)
  Uses shared modules, needs column/step logic

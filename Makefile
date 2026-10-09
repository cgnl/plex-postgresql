# plex-postgresql Makefile
# Supports both macOS (DYLD_INTERPOSE) and Linux (LD_PRELOAD)

UNAME_S := $(shell uname -s)
LEGACY_INCLUDE := include/legacy

PLEX_BIN ?= /Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server

# Compiler settings
ifeq ($(UNAME_S),Darwin)
    # macOS with Homebrew PostgreSQL
    CC = clang
    PG_INCLUDE = /opt/homebrew/opt/postgresql@15/include
    PG_LIB = /opt/homebrew/opt/postgresql@15/lib
    # Legacy C headers remain available as compatibility wrappers/documentation only.
    CFLAGS = -Wall -Wextra -O2 -Iinclude -I$(LEGACY_INCLUDE) -I$(PG_INCLUDE) -fvisibility=hidden
    LDFLAGS = -L$(PG_LIB) -lpq -lc++ -lc++abi
    TARGET = db_interpose_pg.dylib
    SHARED_FLAGS = -dynamiclib -undefined dynamic_lookup
    CXX = clang++
else
    # Linux
    CC = gcc
    PG_INCLUDE = /usr/include/postgresql
    PG_LIB = /usr/lib
    CFLAGS = -Wall -Wextra -O2 -fPIC -Iinclude -I$(LEGACY_INCLUDE) -I$(PG_INCLUDE)
    LDFLAGS = -lpq -lsqlite3 -ldl -lpthread
    TARGET = db_interpose_pg.so
    SHARED_FLAGS = -shared
endif

# Rust plex-pg-core static library (sqlparser-rs backend + PG modules)
RUST_TRANSLATOR_DIR = rust/plex-pg-core
RUST_TRANSLATOR_LIB = $(RUST_TRANSLATOR_DIR)/target/release/libplex_pg_core.a

# SQL Translator + utility shims now live entirely in Rust (no C objects).
SQL_TR_OBJS =

# PG modules are implemented in Rust (no C objects).
PG_MODULES =

# All interpose/runtime code now lives in Rust (no C objects).
OBJECTS =
LINUX_OBJECTS =

ifeq ($(UNAME_S),Darwin)
    WHOLE_ARCHIVE = -Wl,-all_load $(RUST_TRANSLATOR_LIB)
else
    WHOLE_ARCHIVE = -Wl,--whole-archive $(RUST_TRANSLATOR_LIB) -Wl,--no-whole-archive
endif

.PHONY: all clean install test macos linux run stop unit-test ci-test interpose-build-check ffi-header ffi-header-check test-recursion test-crash test-params test-logging test-soci test-fork test-fts test-buffer test-reaper test-upsert test-parity test-uri test-stmt-free test-bind-mismatch

all: $(TARGET)

# The shim runtime is Rust-only now; keep the target so older workflows still succeed.
interpose-build-check:
	@echo "No hand-written C interpose modules remain; Rust is the shim source of truth"

ffi-header:
	./scripts/generate-ffi-header.sh

ffi-header-check:
	@set -e; \
	tmp_header="$$(mktemp /tmp/plex_pg_core_ffi.XXXXXX)"; \
	./scripts/generate-ffi-header.sh "$$tmp_header"; \
	test -s "$$tmp_header"; \
	if [ -f include/plex_pg_core_ffi.h ]; then \
		diff -u include/plex_pg_core_ffi.h "$$tmp_header"; \
	fi; \
	rm -f "$$tmp_header"

# Build Rust plex-pg-core static library
$(RUST_TRANSLATOR_LIB):
	cd $(RUST_TRANSLATOR_DIR) && cargo build --release --lib --features interpose

# Build the shim library (auto-detect platform) - uses modular approach
$(TARGET): $(RUST_TRANSLATOR_LIB)
	$(CC) $(SHARED_FLAGS) -o $@ $(OBJECTS) $(WHOLE_ARCHIVE) $(CFLAGS) $(LDFLAGS)

# Explicit macOS build - always clean first to avoid corrupt object files
macos: clean $(RUST_TRANSLATOR_LIB)
	clang -dynamiclib -undefined dynamic_lookup -o db_interpose_pg.dylib $(OBJECTS) $(WHOLE_ARCHIVE) \
		-I/opt/homebrew/opt/postgresql@15/include -Iinclude \
		-L/opt/homebrew/opt/postgresql@15/lib -lpq -lc++ -lc++abi

# Explicit Linux build (modular - same structure as Mac)
linux: $(RUST_TRANSLATOR_LIB)
	gcc -shared -fPIC -o db_interpose_pg.so $(LINUX_OBJECTS) $(WHOLE_ARCHIVE) \
		-I/usr/include/postgresql -Iinclude \
		-lpq -lsqlite3 -ldl -lpthread

# Object rules (none)

# Clean build artifacts
clean:
	rm -f db_interpose_pg.dylib db_interpose_pg.so $(OBJECTS)

# Install to system location
install: $(TARGET)
ifeq ($(UNAME_S),Darwin)
	@mkdir -p /usr/local/lib/plex-postgresql
	cp $(TARGET) /usr/local/lib/plex-postgresql/
	@echo "Installed to /usr/local/lib/plex-postgresql/"
else
	@mkdir -p /usr/local/lib/plex-postgresql
	cp $(TARGET) /usr/local/lib/plex-postgresql/
	@ldconfig /usr/local/lib/plex-postgresql 2>/dev/null || true
	@echo "Installed to /usr/local/lib/plex-postgresql/"
endif

# Test the shim
test: $(TARGET)
	@echo "Testing shim library load..."
ifeq ($(UNAME_S),Darwin)
	@DYLD_INSERT_LIBRARIES=./$(TARGET) \
		PLEX_PG_HOST=localhost \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		/bin/echo "Shim loaded successfully"
else
	@LD_PRELOAD=./$(TARGET) \
		PLEX_PG_HOST=localhost \
		PLEX_PG_DATABASE=plex \
		PLEX_PG_USER=plex \
		/bin/echo "Shim loaded successfully"
endif

# Development: rebuild and test
dev: clean all test

# Run Plex (macOS only)
run: $(TARGET)
ifeq ($(UNAME_S),Darwin)
	@echo "Starting Plex Media Server with PostgreSQL shim..."
	@pkill -f "Plex Media Server" 2>/dev/null || true
	@sleep 2
	@DYLD_INSERT_LIBRARIES="$(CURDIR)/db_interpose_pg.dylib" \
	PLEX_NO_SHADOW_SCAN=1 \
	PLEX_PG_HOST=$${PLEX_PG_HOST:-localhost} \
	PLEX_PG_PORT=$${PLEX_PG_PORT:-5432} \
	PLEX_PG_DATABASE=$${PLEX_PG_DATABASE:-plex} \
	PLEX_PG_USER=$${PLEX_PG_USER:-plex} \
	PLEX_PG_PASSWORD=$${PLEX_PG_PASSWORD:-plex} \
	PLEX_PG_SCHEMA=$${PLEX_PG_SCHEMA:-plex} \
	PLEX_PG_LOG_LEVEL=$${PLEX_PG_LOG_LEVEL:-ERROR} \
	PLEX_PG_LOG_FILE=$${PLEX_PG_LOG_FILE:-/tmp/plex_redirect_pg.log} \
	PLEX_PG_MEM_TELEMETRY=$${PLEX_PG_MEM_TELEMETRY:-0} \
	"$(PLEX_BIN)" >> $${PLEX_PG_LOG_FILE:-/tmp/plex_redirect_pg.log} 2>&1 &
	@echo "Plex started. Log: /tmp/plex_redirect_pg.log"
else
	@echo "Run target only supported on macOS"
endif

stop:
	@pkill -9 -f "Plex Media Server" 2>/dev/null || true
	@pkill -9 -f "Plex Plug-in" 2>/dev/null || true
	@echo "Plex stopped"

# ============================================================================
# Unit Tests
# ============================================================================

test-recursion:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib db_interpose_prepare_helpers::tests::
	@echo ""

test-crash:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test crash_scenarios
	@echo ""

test-stack-macos:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test stack_macos -- --ignored
	@echo ""

test-sql:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test ported_batch1 --test ported_batch2 --test ported_batch3 --test ported_batch4
	@echo ""

test-types:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib type_normalization_
	@echo ""

test-soci:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib expected_sqlite_type_for_decltype
	@echo ""

test-cache:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib pg_query_cache::tests::
	@echo ""

test-tls:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib tls_pool_cache_
	@echo ""

test-stmt-cache:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib stmt_cache_
	@echo ""

test-fork:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib pool_reset_for_child_clears_all_slots
	@echo ""

test-reaper:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib reaper_
	@echo ""

benchmark:
	@cargo bench --manifest-path rust/plex-pg-core/Cargo.toml

test-api:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test sqlite_api
	@echo ""

test-expanded:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test sqlite_expanded_sql
	@echo ""

test-params:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test sqlite_bind_parameter_index
	@echo ""

test-logging:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test logging_deadlock
	@echo ""

test-exception:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test exception_handler --lib exception_
	@echo ""

test-fts:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib fts_quotes_
	@echo ""

test-buffer:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib column_text_buffer_
	@echo ""

test-upsert:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test ported_batch2
	@echo ""

test-config:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib pg_config::tests::
	@echo ""

test-bind:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib bind_helpers_
	@echo ""

test-common:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib common_helpers_
	@echo ""

test-parity:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib common_load_sqlite_symbols_sets_pointers
	@echo ""

test-statement:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib pg_statement::tests::
	@echo ""

test-stmt-free:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib stmt_free_sweeps_extra_param_values_without_crash
	@echo ""

test-bind-mismatch:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib stmt_unref_cleans_bind_index_mismatch_slots
	@echo ""

test-uri:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib rewrite_server_uri_
	@echo ""

test-stress:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test stress_load -- --ignored
	@echo ""

test-pool-exhaustion:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test pool_exhaustion
	@echo ""

test-streaming:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test streaming_mode
	@echo ""

test-isolation:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test connection_isolation
	@echo ""

test-shadow:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test shadow_fallback
	@echo ""

test-shadow-elim:
	@echo ""
	@cargo test --manifest-path rust/plex-pg-core/Cargo.toml --test shadow_fallback shadow_elimination_
	@echo ""

unit-test: test-recursion test-crash test-sql test-upsert test-types test-soci test-cache test-tls test-stmt-cache test-fork test-reaper test-buffer test-api test-expanded test-params test-logging test-exception test-fts test-config test-bind test-common test-statement test-stmt-free test-bind-mismatch test-parity test-uri test-streaming test-isolation test-shadow test-shadow-elim
	@echo "All unit tests complete."

ci-test: test-recursion test-crash test-sql test-upsert test-types test-soci test-cache test-tls test-stmt-cache test-fork test-reaper test-buffer test-logging test-exception test-fts test-config test-bind test-common test-statement test-stmt-free test-bind-mismatch test-parity test-uri test-streaming test-isolation test-shadow test-shadow-elim
	@echo "All CI unit tests complete."

# ============================================================================
# Release builds
# ============================================================================

RELEASE_DIR = release
VERSION = $(shell cat VERSION 2>/dev/null || echo "dev")

# Architecture-specific builds (macOS only)
# Note: common module is implemented in Rust and linked via $(RUST_TRANSLATOR_LIB)
release-arm64: clean $(RUST_TRANSLATOR_LIB)
	@echo "Building arm64..."
	$(CC) -dynamiclib -undefined dynamic_lookup -arch arm64 -o db_interpose_pg-arm64.dylib $(OBJECTS) $(WHOLE_ARCHIVE) $(CFLAGS) $(LDFLAGS)
	@echo "Built db_interpose_pg-arm64.dylib"

release-x86_64: clean $(RUST_TRANSLATOR_LIB)
	@echo "Building x86_64..."
	$(CC) -dynamiclib -undefined dynamic_lookup -arch x86_64 -o db_interpose_pg-x86_64.dylib $(OBJECTS) $(WHOLE_ARCHIVE) $(CFLAGS) $(LDFLAGS)
	@echo "Built db_interpose_pg-x86_64.dylib"

# Universal binary (both architectures)
release-universal: $(RUST_TRANSLATOR_LIB)
	@echo "Building universal binary for v$(VERSION)..."
	@mkdir -p $(RELEASE_DIR)/v$(VERSION)
	@# Build arm64
	@$(MAKE) clean >/dev/null
	@$(CC) -dynamiclib -undefined dynamic_lookup -arch arm64 -o $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-arm64.dylib $(OBJECTS) $(WHOLE_ARCHIVE) $(CFLAGS) $(LDFLAGS)
	@echo "  ✓ arm64"
	@# Build x86_64
	@$(MAKE) clean >/dev/null
	@$(CC) -dynamiclib -undefined dynamic_lookup -arch x86_64 -o $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-x86_64.dylib $(OBJECTS) $(WHOLE_ARCHIVE) $(CFLAGS) $(LDFLAGS)
	@echo "  ✓ x86_64"
	@# Create universal binary
	@lipo -create \
		$(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-arm64.dylib \
		$(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-x86_64.dylib \
		-output $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg.dylib
	@echo "  ✓ universal"
	@# Show result
	@echo ""
	@echo "Release v$(VERSION) built:"
	@ls -lh $(RELEASE_DIR)/v$(VERSION)/*.dylib
	@echo ""
	@file $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg.dylib

# Create macOS release tarball
release: release-universal
	@echo "Packaging macOS release..."
	@cd $(RELEASE_DIR)/v$(VERSION) && \
		mkdir -p scripts schema && \
		cp ../../README.md ../../LICENSE ../../THIRD_PARTY_LICENSES ../../CHANGELOG.md ../../RELEASE_NOTES.md ../../INSTALL.md . 2>/dev/null || true && \
		cp ../../scripts/install_wrappers.sh scripts/ && \
		cp ../../scripts/install_wrappers_linux.sh scripts/ && \
		cp ../../scripts/uninstall_wrappers.sh scripts/ && \
		cp ../../scripts/uninstall_wrappers_linux.sh scripts/ && \
		cp ../../scripts/migrate_sqlite_to_pg.sh scripts/ && \
		cp ../../scripts/migrate_lib.sh ../../scripts/migrate_table.py ../../scripts/seed_shadow_table_from_pg.py scripts/ && \
		cp ../../schema/*.sql schema/ && \
		cp ../../scripts/docker-entrypoint.sh scripts/ && \
		tar -czf ../plex-postgresql-v$(VERSION)-macos.tar.gz \
			*.dylib README.md LICENSE THIRD_PARTY_LICENSES CHANGELOG.md RELEASE_NOTES.md INSTALL.md scripts/ schema/
	@echo "  ✓ $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-macos.tar.gz"
	@ls -lh $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-macos.tar.gz

# Build Linux release (requires Docker)
release-linux:
	@echo "Building Linux binaries via Docker..."
	@mkdir -p $(RELEASE_DIR)/v$(VERSION)
	@# Build aarch64
	docker buildx build --platform linux/arm64 --target builder -t plex-pg-builder-arm64 --load . 2>&1 | tail -3
	docker rm -f plex-pg-extract 2>/dev/null || true
	docker create --name plex-pg-extract plex-pg-builder-arm64
	docker cp plex-pg-extract:/libs/db_interpose_pg.so $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-linux-aarch64.so
	docker cp plex-pg-extract:/libs/libpq.so.5 $(RELEASE_DIR)/v$(VERSION)/
	docker rm plex-pg-extract
	@echo "  ✓ linux-aarch64"
	@# Build x86_64
	docker buildx build --platform linux/amd64 --target builder -t plex-pg-builder-amd64 --load . 2>&1 | tail -3
	docker rm -f plex-pg-extract 2>/dev/null || true
	docker create --name plex-pg-extract plex-pg-builder-amd64
	docker cp plex-pg-extract:/libs/db_interpose_pg.so $(RELEASE_DIR)/v$(VERSION)/db_interpose_pg-linux-x86_64.so
	docker rm plex-pg-extract
	@echo "  ✓ linux-x86_64"
	@# Package
	@cd $(RELEASE_DIR)/v$(VERSION) && \
		mkdir -p scripts schema && \
		cp ../../README.md ../../LICENSE ../../THIRD_PARTY_LICENSES ../../CHANGELOG.md ../../RELEASE_NOTES.md ../../INSTALL.md . 2>/dev/null || true && \
		cp ../../scripts/install_wrappers.sh scripts/ && \
		cp ../../scripts/install_wrappers_linux.sh scripts/ && \
		cp ../../scripts/uninstall_wrappers.sh scripts/ && \
		cp ../../scripts/uninstall_wrappers_linux.sh scripts/ && \
		cp ../../scripts/migrate_sqlite_to_pg.sh scripts/ && \
		cp ../../scripts/migrate_lib.sh ../../scripts/migrate_table.py ../../scripts/seed_shadow_table_from_pg.py scripts/ && \
		cp ../../schema/*.sql schema/ && \
		cp ../../scripts/docker-entrypoint.sh scripts/ && \
		tar -czf ../plex-postgresql-v$(VERSION)-linux.tar.gz \
			db_interpose_pg-linux-*.so libpq.so.5 \
			README.md LICENSE THIRD_PARTY_LICENSES CHANGELOG.md RELEASE_NOTES.md INSTALL.md scripts/ schema/
	@echo "  ✓ $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-linux.tar.gz"
	@ls -lh $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-linux.tar.gz

# Full release (macOS + Linux)
release-all: release release-linux
	@echo ""
	@echo "=== Release v$(VERSION) Complete ==="
	@ls -lh $(RELEASE_DIR)/plex-postgresql-v$(VERSION)-*.tar.gz

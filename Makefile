# plex-postgresql Makefile
# Supports both macOS (DYLD_INTERPOSE) and Linux (LD_PRELOAD)

UNAME_S := $(shell uname -s)

PLEX_BIN ?= /Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server

# Compiler settings
ifeq ($(UNAME_S),Darwin)
    # macOS with Homebrew PostgreSQL
    CC = clang
    PG_INCLUDE = /opt/homebrew/opt/postgresql@15/include
    PG_LIB = /opt/homebrew/opt/postgresql@15/lib
    # Added -Isrc to find new headers
    CFLAGS = -Wall -Wextra -O2 -I$(PG_INCLUDE) -Iinclude -Isrc
    LDFLAGS = -L. -lpq -lpgcommon -lpgport
    TARGET = db_interpose_pg.dylib
    SOURCE = src/db_interpose_pg.c
    SHARED_FLAGS = -dynamiclib -undefined dynamic_lookup
else
    # Linux
    CC = gcc
    PG_INCLUDE = /usr/include/postgresql
    PG_LIB = /usr/lib
    CFLAGS = -Wall -Wextra -O2 -fPIC -I$(PG_INCLUDE) -Iinclude -Isrc
    LDFLAGS = -lpq -lsqlite3 -ldl -lpthread
    TARGET = db_interpose_pg.so
    SOURCE = src/db_interpose_pg_linux.c
    SHARED_FLAGS = -shared
endif

# SQL Translator modules
SQL_TR_OBJS = src/sql_translator.o src/sql_tr_helpers.o src/sql_tr_placeholders.o \
              src/sql_tr_functions.o src/sql_tr_query.o src/sql_tr_groupby.o src/sql_tr_types.o \
              src/sql_tr_quotes.o src/sql_tr_keywords.o src/sql_tr_upsert.o

# PG modules
PG_MODULES = src/pg_config.o src/pg_logging.o src/pg_client.o src/pg_statement.o

# All objects
OBJECTS = $(SQL_TR_OBJS) $(PG_MODULES) src/fishhook.o

.PHONY: all clean install test macos linux run stop

all: $(TARGET)

# Build the shim library (auto-detect platform)
$(TARGET): $(SOURCE) $(OBJECTS)
	$(CC) $(SHARED_FLAGS) -o $@ $< $(OBJECTS) $(CFLAGS) $(LDFLAGS)

# Explicit macOS build - use dynamic_lookup instead of linking sqlite3
macos: src/db_interpose_pg.c $(OBJECTS)
	clang -dynamiclib -flat_namespace -undefined dynamic_lookup -o db_interpose_pg.dylib $< $(OBJECTS) \
		-I/opt/homebrew/opt/postgresql@15/include -Iinclude -Isrc \
		-L/opt/homebrew/opt/postgresql@15/lib -lpq

# Explicit Linux build (uses refactored version with shared modules)
linux: src/db_interpose_pg_linux.c $(OBJECTS)
	gcc -shared -fPIC -o db_interpose_pg.so src/db_interpose_pg_linux.c $(OBJECTS) \
		-I/usr/include/postgresql -Iinclude -Isrc \
		-lpq -lsqlite3 -ldl -lpthread

# Object rules
# SQL Translator module compilation rules
src/sql_translator.o: src/sql_translator.c include/sql_translator.h src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_helpers.o: src/sql_tr_helpers.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_placeholders.o: src/sql_tr_placeholders.c include/sql_translator.h src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_functions.o: src/sql_tr_functions.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_query.o: src/sql_tr_query.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_groupby.o: src/sql_tr_groupby.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_types.o: src/sql_tr_types.c include/sql_translator.h src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_quotes.o: src/sql_tr_quotes.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_keywords.o: src/sql_tr_keywords.c include/sql_translator.h src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/sql_tr_upsert.o: src/sql_tr_upsert.c src/sql_translator_internal.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_config.o: src/pg_config.c src/pg_config.h src/pg_types.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_logging.o: src/pg_logging.c src/pg_logging.h src/pg_types.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_client.o: src/pg_client.c src/pg_client.h src/pg_types.h src/pg_logging.h src/pg_config.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_statement.o: src/pg_statement.c src/pg_statement.h src/pg_types.h src/pg_logging.h src/pg_client.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/fishhook.o: src/fishhook.c include/fishhook.h
	$(CC) -c -O2 -Iinclude -o $@ $<

# Clean build artifacts
clean:
	rm -f db_interpose_pg.dylib db_interpose_pg.so $(OBJECTS) $(PG_MODULES)

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
	ENV_PG_LOG_LEVEL=$${ENV_PG_LOG_LEVEL:-DEBUG} \
	ENV_PG_LOG_FILE=$${ENV_PG_LOG_FILE:-/tmp/plex_redirect_pg.log} \
	"$(PLEX_BIN)" >> $${ENV_PG_LOG_FILE:-/tmp/plex_redirect_pg.log} 2>&1 &
	@echo "Plex started. Log: $(ENV_PG_LOG_FILE)"
else
	@echo "Run target only supported on macOS"
endif

stop:
	@pkill -9 -f "Plex Media Server" 2>/dev/null || true
	@pkill -9 -f "Plex Plug-in" 2>/dev/null || true
	@echo "Plex stopped"

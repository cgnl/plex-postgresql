# plex-postgresql Makefile
# Supports both macOS (DYLD_INTERPOSE) and Linux (LD_PRELOAD)

UNAME_S := $(shell uname -s)

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

OBJECTS = src/sql_translator.o src/pg_config.o src/pg_logging.o src/pg_client.o src/pg_statement.o src/fishhook.o

.PHONY: all clean install test macos linux run stop

all: $(TARGET)

# Build the shim library (auto-detect platform)
$(TARGET): $(SOURCE) $(OBJECTS)
	$(CC) $(SHARED_FLAGS) -o $@ $< $(OBJECTS) $(CFLAGS) $(LDFLAGS)

# Explicit macOS build - use dynamic_lookup instead of linking sqlite3
macos: src/db_interpose_pg.c src/fishhook.c $(OBJECTS)
	clang -dynamiclib -flat_namespace -undefined dynamic_lookup -o db_interpose_pg.dylib $< src/fishhook.c $(OBJECTS) \
		-I/opt/homebrew/opt/postgresql@15/include -Iinclude -Isrc \
		-L/opt/homebrew/opt/postgresql@15/lib -lpq

# Explicit Linux build
linux: src/db_interpose_pg_linux.c src/sql_translator.c include/sql_translator.h $(OBJECTS)
	gcc -shared -fPIC -o db_interpose_pg.so src/db_interpose_pg_linux.c src/fishhook.c $(OBJECTS) \
		-Iinclude -Isrc -lsqlite3 -ldl -lpthread

# Object rules
src/sql_translator.o: src/sql_translator.c include/sql_translator.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_config.o: src/pg_config.c src/pg_config.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_logging.o: src/pg_logging.c src/pg_logging.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_client.o: src/pg_client.c src/pg_client.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/pg_statement.o: src/pg_statement.c src/pg_statement.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

src/fishhook.o: src/fishhook.c include/fishhook.h
	$(CC) -c -O2 -Iinclude -o $@ $<

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
	@DYLD_INSERT_LIBRARIES="$(CURDIR)/$(TARGET)" \
		DYLD_PRINT_INTERPOSING=1 \
		PLEX_PG_HOST=$${PLEX_PG_HOST:-localhost} \
		PLEX_PG_PORT=$${PLEX_PG_PORT:-5432} \
		PLEX_PG_DATABASE=$${PLEX_PG_DATABASE:-plex} \
		PLEX_PG_USER=$${PLEX_PG_USER:-plex} \
		PLEX_PG_PASSWORD=$${PLEX_PG_PASSWORD:-plex} \
		PLEX_PG_SCHEMA=$${PLEX_PG_SCHEMA:-plex} \
		"/Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server" &
	@echo "Plex started. Log: /tmp/plex_redirect_pg.log"
else
	@echo "Run target only supported on macOS"
endif

stop:
	@pkill -9 -f "Plex Media Server" 2>/dev/null || true
	@pkill -9 -f "Plex Plug-in" 2>/dev/null || true
	@echo "Plex stopped"

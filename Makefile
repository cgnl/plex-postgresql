# plex-postgresql Makefile
# Supports both macOS (DYLD_INTERPOSE) and Linux (LD_PRELOAD)

UNAME_S := $(shell uname -s)

# Compiler settings
ifeq ($(UNAME_S),Darwin)
    # macOS with Homebrew PostgreSQL
    CC = clang
    PG_INCLUDE = /opt/homebrew/opt/postgresql@15/include
    PG_LIB = /opt/homebrew/opt/postgresql@15/lib
    CFLAGS = -Wall -Wextra -O2 -I$(PG_INCLUDE) -Iinclude
    LDFLAGS = -L$(PG_LIB) -lpq -lsqlite3
    TARGET = db_interpose_pg.dylib
    SOURCE = src/db_interpose_pg.c
    SHARED_FLAGS = -dynamiclib -flat_namespace
else
    # Linux
    CC = gcc
    PG_INCLUDE = /usr/include/postgresql
    PG_LIB = /usr/lib
    CFLAGS = -Wall -Wextra -O2 -fPIC -I$(PG_INCLUDE) -Iinclude
    LDFLAGS = -lpq -lsqlite3 -ldl -lpthread
    TARGET = db_interpose_pg.so
    SOURCE = src/db_interpose_pg_linux.c
    SHARED_FLAGS = -shared
endif

OBJECTS = src/sql_translator.o

.PHONY: all clean install test macos linux run stop

all: $(TARGET)

# Build the shim library (auto-detect platform)
$(TARGET): $(SOURCE) $(OBJECTS)
	$(CC) $(SHARED_FLAGS) -o $@ $< $(OBJECTS) $(CFLAGS) $(LDFLAGS)

# Explicit macOS build
macos: src/db_interpose_pg.c $(OBJECTS)
	clang -dynamiclib -flat_namespace -o db_interpose_pg.dylib $< $(OBJECTS) \
		-I/opt/homebrew/opt/postgresql@15/include -Iinclude \
		-L/opt/homebrew/opt/postgresql@15/lib -lpq -lsqlite3

# Explicit Linux build (uses dlopen for libpq - no link-time dependency)
linux: src/db_interpose_pg_linux.c src/sql_translator.c include/sql_translator.h
	gcc -c -fPIC -o src/sql_translator_linux.o src/sql_translator.c -Iinclude -Wall -Wextra -O2
	gcc -shared -fPIC -o db_interpose_pg.so src/db_interpose_pg_linux.c src/sql_translator_linux.o \
		-Iinclude -lsqlite3 -ldl -lpthread

# Build SQL translator object
src/sql_translator.o: src/sql_translator.c include/sql_translator.h
	$(CC) -c -fPIC -o $@ $< $(CFLAGS)

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
	ldconfig /usr/local/lib/plex-postgresql 2>/dev/null || true
	@echo "Installed to /usr/local/lib/plex-postgresql/"
endif

# Test the shim
test: $(TARGET)
	@echo "Testing shim library..."
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

# Run Plex with PostgreSQL shim (macOS only)
run: $(TARGET)
ifeq ($(UNAME_S),Darwin)
	@echo "Starting Plex Media Server with PostgreSQL shim..."
	@pkill -f "Plex Media Server" 2>/dev/null || true
	@sleep 2
	@DYLD_INSERT_LIBRARIES="$(CURDIR)/$(TARGET)" \
		DYLD_FORCE_FLAT_NAMESPACE=1 \
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

# Stop Plex
stop:
	@pkill -9 -f "Plex Media Server" 2>/dev/null || true
	@pkill -9 -f "Plex Plug-in" 2>/dev/null || true
	@echo "Plex stopped"

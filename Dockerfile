# Dockerfile for plex-postgresql
# Build with Alpine 3.15 which has musl 1.2.2 - same as Plex's bundled musl!

FROM alpine:3.15 AS builder

# Install build dependencies
RUN apk add --no-cache \
    build-base \
    sqlite-dev \
    linux-headers \
    curl \
    perl

# Verify musl version matches Plex (1.2.2)
RUN /lib/ld-musl-*.so.1 --version 2>&1 | head -2

WORKDIR /build

# Download and build PostgreSQL with minimal features (just libpq)
RUN curl -L https://ftp.postgresql.org/pub/source/v15.10/postgresql-15.10.tar.gz | tar xz
RUN cd postgresql-15.10 && \
    # Configure WITHOUT OpenSSL to avoid ENGINE symbol conflicts
    ./configure --prefix=/usr/local/pgsql \
        --without-readline \
        --without-zlib \
        --without-openssl \
        --without-icu && \
    # Build and install include files first
    cd src/include && make install && \
    # Build and install libpq
    cd ../interfaces/libpq && make && make install && \
    # Build pg_config for headers
    cd ../../bin/pg_config && make && make install

# Copy source files
COPY src/ src/
COPY include/ include/

# Build shim with musl 1.2.2 (same as Plex) - include debug symbols
# Use rpath to find libpq in our lib directory
RUN gcc -shared -fPIC -g -o db_interpose_pg.so \
    src/db_interpose_pg_linux.c \
    src/sql_translator.c src/sql_tr_helpers.c src/sql_tr_placeholders.c \
    src/sql_tr_functions.c src/sql_tr_query.c src/sql_tr_groupby.c \
    src/sql_tr_types.c src/sql_tr_quotes.c src/sql_tr_keywords.c \
    src/sql_tr_upsert.c src/pg_config.c src/pg_logging.c \
    src/pg_client.c src/pg_statement.c \
    -I/usr/local/pgsql/include -I/usr/include -Iinclude -Isrc \
    -L/usr/local/pgsql/lib -lpq \
    -ldl -lpthread \
    -Wl,-rpath,/usr/local/lib/plex-postgresql

# Check dependencies
RUN echo "=== Shim dependencies ===" && (LD_LIBRARY_PATH=/usr/local/pgsql/lib ldd db_interpose_pg.so || true)

# Gather libraries
RUN mkdir -p /libs && \
    cp db_interpose_pg.so /libs/ && \
    cp /usr/local/pgsql/lib/libpq.so.5* /libs/ && \
    ls -la /libs/

# Runtime stage
FROM linuxserver/plex:latest

# Install PostgreSQL client for health checks, sqlite3 for schema fixes, gdb for debugging
RUN apt-get update && apt-get install -y --no-install-recommends \
    postgresql-client \
    sqlite3 \
    gdb \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /usr/local/lib/plex-postgresql

# Create symlinks for musl compatibility
# Our shim was built with Alpine which expects libc.musl-aarch64.so.1
# but Plex bundles musl as libc.so
RUN ln -sf /usr/lib/plexmediaserver/lib/libc.so /usr/local/lib/plex-postgresql/libc.musl-aarch64.so.1

COPY --from=builder /libs/*.so* /usr/local/lib/plex-postgresql/

COPY schema/plex_schema.sql /usr/local/lib/plex-postgresql/
COPY schema/sqlite_schema.sql /usr/local/lib/plex-postgresql/

COPY scripts/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

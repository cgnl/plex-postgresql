# Dockerfile for plex-postgresql
# Builds the LD_PRELOAD shim and runs with linuxserver/plex
# Uses multi-stage build to get musl-based libpq from Alpine

# Stage 1: Get musl-based libpq and its dependencies from Alpine
FROM alpine:3.20 AS alpine-libs
RUN apk add --no-cache libpq openssl

# Stage 2: Build and run with linuxserver/plex
FROM linuxserver/plex:latest

# Install build dependencies and PostgreSQL client
RUN apt-get update && apt-get install -y \
    build-essential \
    libsqlite3-dev \
    postgresql-client \
    && rm -rf /var/lib/apt/lists/*

# Copy musl-based libraries from Alpine stage
# libpq and its SSL dependencies for compatibility with Plex's musl runtime
COPY --from=alpine-libs /usr/lib/libpq.so.5* /usr/local/lib/plex-postgresql/
COPY --from=alpine-libs /lib/libssl.so.3* /usr/local/lib/plex-postgresql/
COPY --from=alpine-libs /lib/libcrypto.so.3* /usr/local/lib/plex-postgresql/

# Create build directory
WORKDIR /build

# Copy source files
COPY src/ src/
COPY include/ include/
COPY Makefile .

# Build the shim (no libpq linking - uses dlopen at runtime)
RUN make linux

# Install the shim
RUN mkdir -p /usr/local/lib/plex-postgresql && \
    cp db_interpose_pg.so /usr/local/lib/plex-postgresql/

# Replace Plex's bundled SQLite with our shim
# The shim loads the original via dlopen and forwards all calls
RUN mv /usr/lib/plexmediaserver/lib/libsqlite3.so /usr/lib/plexmediaserver/lib/libsqlite3_original.so && \
    cp /usr/local/lib/plex-postgresql/db_interpose_pg.so /usr/lib/plexmediaserver/lib/libsqlite3.so

# Copy schema file for initialization
COPY schema/plex_schema.sql /usr/local/lib/plex-postgresql/

# Copy and setup custom entrypoint
COPY scripts/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Use custom entrypoint that initializes PostgreSQL before starting Plex
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

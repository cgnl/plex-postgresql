# syntax=docker/dockerfile:1.5
# Dockerfile for plex-postgresql
# Build with Alpine 3.15 which has musl 1.2.2 - same as Plex's bundled musl!

FROM alpine:3.15 AS builder

ARG PLEX_PG_SANITIZE
ENV PLEX_PG_SANITIZE=${PLEX_PG_SANITIZE}

# Install build dependencies – includes sanitizer runtime libraries
RUN apk add --no-cache \
    build-base \
    libasan \
    libtsan \
    sqlite-dev \
    linux-headers \
    curl \
    perl

# Verify musl version matches Plex (1.2.2)
RUN /lib/ld-musl-*.so.1 --version 2>&1 | head -2

# ... rest of builder stage unchanged ...

WORKDIR /build

# Install Rust toolchain
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV RUSTUP_TOOLCHAIN=stable
ENV CARGO_TARGET_DIR=/build/target
ENV PATH="/usr/local/cargo/bin:${PATH}"
RUN --mount=type=cache,target=/usr/local/rustup,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal && \
    /usr/local/cargo/bin/rustup default stable

# Copy source files
COPY include/ include/
COPY rust/ rust/
COPY Makefile Makefile
COPY VERSION VERSION
COPY scripts/docker-build-shim.sh scripts/docker-build-shim.sh

# Build PostgreSQL/libpq, Rust core, shim, and collect runtime libs in /libs
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    --mount=type=cache,target=/build/.cache,sharing=locked \
    rm -rf /usr/local/cargo/registry/src/index.crates.io-* && \
    sh scripts/docker-build-shim.sh

# Runtime stage
FROM linuxserver/plex:latest

# Install PostgreSQL client for health checks, sqlite3 for schema fixes,
# python3 for data migration, gdb for debugging, and conditional sanitizer runtime libraries
ARG PLEX_PG_SANITIZE
RUN apt-get update && apt-get install -y --no-install-recommends \
    postgresql-client \
    sqlite3 \
    python3 \
    gdb \
    $(if [ "$PLEX_PG_SANITIZE" = "address" ]; then echo "libasan8"; fi) \
    $(if [ "$PLEX_PG_SANITIZE" = "thread" ]; then echo "libtsan2"; fi) \
    && rm -rf /var/lib/apt/lists/*

# NOTE: Do NOT set LANG/LC_ALL/CHARSET here — Plex's bundled musl+boost::locale
# handles locale internally. Setting these can interfere with exception handling.

RUN mkdir -p /usr/local/lib/plex-postgresql

# Create symlinks for musl compatibility (architecture-specific)
# Our shim was built with Alpine which expects libc.musl-{arch}.so.1
# but Plex bundles musl as libc.so
RUN ARCH=$(uname -m) && \
    echo "Creating musl symlink for architecture: $ARCH" && \
    if [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then \
        MUSL_ARCH="aarch64"; \
    elif [ "$ARCH" = "x86_64" ]; then \
        MUSL_ARCH="x86_64"; \
    else \
        echo "Warning: Unknown architecture $ARCH, using $ARCH as-is"; \
        MUSL_ARCH="$ARCH"; \
    fi && \
    ln -sf /usr/lib/plexmediaserver/lib/libc.so /usr/local/lib/plex-postgresql/libc.musl-${MUSL_ARCH}.so.1 && \
    echo "Created symlink: libc.musl-${MUSL_ARCH}.so.1 -> /usr/lib/plexmediaserver/lib/libc.so"

COPY --from=builder /libs/*.so* /usr/local/lib/plex-postgresql/
COPY --from=builder /libs/subreaper /usr/local/bin/subreaper

COPY schema/plex_schema.sql /usr/local/lib/plex-postgresql/
COPY schema/sqlite_schema.sql /usr/local/lib/plex-postgresql/
COPY schema/sqlite_column_types.sql /usr/local/lib/plex-postgresql/
COPY schema/pg_compat_functions.sql /usr/local/lib/plex-postgresql/
COPY scripts/migrate_lib.sh /usr/local/lib/plex-postgresql/
COPY scripts/migrate_table.py /usr/local/lib/plex-postgresql/
COPY scripts/seed_shadow_table_from_pg.py /usr/local/lib/plex-postgresql/
COPY scripts/doctor.sh /usr/local/lib/plex-postgresql/

# Copy the initialization script for s6-overlay
# This will run BEFORE Plex starts as part of the init sequence
COPY scripts/docker-entrypoint.sh /usr/local/lib/plex-postgresql/docker-entrypoint.sh
RUN chmod +x /usr/local/lib/plex-postgresql/docker-entrypoint.sh

# Create s6-overlay init script to run our initialization
RUN mkdir -p /etc/s6-overlay/s6-rc.d/init-plex-postgresql && \
    echo "oneshot" > /etc/s6-overlay/s6-rc.d/init-plex-postgresql/type && \
    echo "/usr/local/lib/plex-postgresql/docker-entrypoint.sh" > /etc/s6-overlay/s6-rc.d/init-plex-postgresql/up && \
    chmod +x /etc/s6-overlay/s6-rc.d/init-plex-postgresql/up && \
    mkdir -p /etc/s6-overlay/s6-rc.d/user/contents.d && \
    touch /etc/s6-overlay/s6-rc.d/user/contents.d/init-plex-postgresql && \
    mkdir -p /etc/s6-overlay/s6-rc.d/svc-plex/dependencies.d && \
    touch /etc/s6-overlay/s6-rc.d/svc-plex/dependencies.d/init-plex-postgresql

# Fix claim script: inject LD_PRELOAD into the temporary Plex start during claim
# The base image's init-plex-claim starts Plex without our shim, which crashes
# because the SQLite shadow DB has no schema. We patch it to use the shim.
RUN if [ -f /etc/s6-overlay/s6-rc.d/init-plex-claim/run ]; then \
        sed -i 's|LD_LIBRARY_PATH=/usr/lib/plexmediaserver:/usr/lib/plexmediaserver/lib|LD_PRELOAD=/usr/local/lib/plex-postgresql/db_interpose_pg.so LD_LIBRARY_PATH=/usr/lib/plexmediaserver:/usr/lib/plexmediaserver/lib|' \
            /etc/s6-overlay/s6-rc.d/init-plex-claim/run && \
        mkdir -p /etc/s6-overlay/s6-rc.d/init-plex-claim/dependencies.d && \
        touch /etc/s6-overlay/s6-rc.d/init-plex-claim/dependencies.d/init-plex-postgresql && \
        echo "Patched init-plex-claim for PostgreSQL shim"; \
    fi

# s6 finish script — defense-in-depth for the BindAddrInUseException crash loop.
RUN printf '#!/bin/bash\n# Exit code 125 tells s6-supervise not to restart the service.\n# See: https://skarnet.org/software/s6/s6-supervise.html\nexit_code=${1:-0}\nif [ "${exit_code}" = "0" ]; then\n  deadline=30\n  elapsed=0\n  while [ $elapsed -lt $deadline ]; do\n    if pgrep -x "Plex Media Server" >/dev/null 2>&1; then\n      echo "[plex-pg] PMS re-exec child still running (${elapsed}s), suppressing restart"\n      sleep 1\n      elapsed=$((elapsed+1))\n    else\n      break\n    fi\n  done\n  if pgrep -x "Plex Media Server" >/dev/null 2>&1; then\n    echo "[plex-pg] PMS child still alive after ${deadline}s — suppressing restart, s6 will retry"\n    exit 125\n  fi\nfi\n' \
        > /etc/s6-overlay/s6-rc.d/svc-plex/finish && \
    chmod +x /etc/s6-overlay/s6-rc.d/svc-plex/finish && \
    printf '30000\n' > /etc/s6-overlay/s6-rc.d/svc-plex/finish-timeout

# Inject shim env into the upstream svc-plex run script and wrap PMS with subreaper
RUN sed -i '/export PLEX_MEDIA_SERVER_INFO_PLATFORM_VERSION/a\
arch="$(uname -m)"\
\nif [[ "$arch" == "aarch64" || "$arch" == "arm64" ]]; then\
\n    export OPENSSL_armcap="${PLEX_PG_OPENSSL_ARMCAP:-0}"\
\nfi\
\nexport LD_LIBRARY_PATH="/usr/local/lib/plex-postgresql:/usr/lib/plexmediaserver/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"\
\nexport LD_PRELOAD="/usr/local/lib/plex-postgresql/db_interpose_pg.so"\
' /etc/s6-overlay/s6-rc.d/svc-plex/run && \
    sed -i 's|"/usr/lib/plexmediaserver/Plex Media Server"|/usr/local/bin/subreaper "/usr/lib/plexmediaserver/Plex Media Server"|g' \
        /etc/s6-overlay/s6-rc.d/svc-plex/run && \
    cat /etc/s6-overlay/s6-rc.d/svc-plex/run

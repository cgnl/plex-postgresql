#!/usr/bin/env bash
# Runs the ignored metadata-descriptor runtime regression against a real
# PostgreSQL fixture and a musl shim built from this exact source revision.
# The regression drives the real shared shim through a killable TCP proxy to
# prove descriptors survive empty results, a PostgreSQL outage and recovery,
# without leaking server-side prepared statements.
#
# Mirrors scripts/run-runtime-e2e.sh fixture ownership rules (no skips):
# an explicit external loopback fixture with runtime_e2e_ identity is required.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
shim=
while (($#)); do
    case "$1" in
        --shim) shim=${2:?--shim requires a path}; shift 2 ;;
        *) echo "usage: $0 [--shim PATH]" >&2; exit 2 ;;
    esac
done
for variable in $(env | sed -n 's/^\(PLEX_PG_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$variable"; done
for variable in $(env | sed -n 's/^\(PG[A-Z0-9_]*\)=.*/\1/p'); do unset "$variable"; done
unset LD_PRELOAD DYLD_INSERT_LIBRARIES
fixture=$(mktemp -d "/tmp/metadata-descriptor-e2e.XXXXXXXX")
touch "$fixture/.metadata-descriptor-e2e-fixture"
external_fixture_ready=0
database="metadata_descriptor_${$}_${RANDOM}"
role="$database"
password="fixture_${RANDOM}_${RANDOM}"
cleanup() {
    status=$?
    trap - EXIT
    if [[ $external_fixture_ready == 1 ]]; then
        psql -X -h "$admin_host" -p "$admin_port" -U "$admin_user" -d "$admin_database" -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS $database WITH (FORCE)" -c "DROP ROLE IF EXISTS $role" >/dev/null 2>&1 || echo "WARNING: fixture cleanup failed: $database/$role" >&2
    fi
    if [[ $status != 0 && -f "$fixture/shim.log" ]]; then tail -n 80 "$fixture/shim.log" >&2; fi
    rm -rf "$fixture"
    exit "$status"
}
trap cleanup EXIT
if [[ ${RUNTIME_E2E_EXTERNAL_FIXTURE:-0} != 1 ]]; then
    echo "explicit fixture required; set RUNTIME_E2E_EXTERNAL_FIXTURE=1 and RUNTIME_E2E_ADMIN_* (no skips)" >&2
    exit 1
fi
admin_host=${RUNTIME_E2E_ADMIN_HOST:?explicit fixture admin host required}
[[ "$admin_host" == 127.0.0.1 ]] || { echo "external fixture must be loopback" >&2; exit 2; }
admin_port=${RUNTIME_E2E_ADMIN_PORT:?explicit fixture admin port required}
admin_user=${RUNTIME_E2E_ADMIN_USER:?explicit fixture admin user required}
admin_database=${RUNTIME_E2E_ADMIN_DATABASE:?explicit fixture admin database required}
export PGPASSWORD=${RUNTIME_E2E_ADMIN_PASSWORD:?explicit fixture admin password required}
[[ "$admin_port" =~ ^[0-9]+$ ]] || { echo "invalid fixture port" >&2; exit 2; }
[[ "$admin_user" == runtime_e2e_* && "$admin_database" == runtime_e2e_* ]] || { echo "external fixture admin identity must use runtime_e2e_ names" >&2; exit 2; }
external_fixture_ready=1
psql -X -h "$admin_host" -p "$admin_port" -U "$admin_user" -d "$admin_database" -v ON_ERROR_STOP=1 \
    -c "CREATE ROLE $role LOGIN PASSWORD '$password'" \
    -c "CREATE DATABASE $database OWNER $role" \
    -c "COMMENT ON DATABASE $database IS 'metadata-descriptor-runtime disposable fixture'" >/dev/null
unset PGPASSWORD
mkdir -p /tmp/metadata-descriptor
if [[ -z "$shim" ]]; then
    [[ $(uname -s) == Linux ]] || { echo "unsupported platform: the regression requires a musl runner (no skips)" >&2; exit 1; }
    cargo build --manifest-path "$repo/rust/plex-pg-core/Cargo.toml" --release --lib --features interpose
    make -C "$repo" -B db_interpose_pg.so LDFLAGS='-Wl,--no-as-needed -lpq -lsqlite3 -ldl -lpthread'
    shim="$repo/db_interpose_pg.so"
fi
[[ -f "$shim" ]] || { echo "missing shared artifact: $shim" >&2; exit 1; }
shim=$(cd "$(dirname "$shim")" && pwd)/$(basename "$shim")
sqlite_lib=${METADATA_DESCRIPTOR_RUNTIME_SQLITE:-}
if [[ -z "$sqlite_lib" ]]; then
    for candidate in /usr/lib/libsqlite3.so.0 /usr/lib/x86_64-linux-gnu/libsqlite3.so.0; do
        if [[ -e "$candidate" ]]; then sqlite_lib=$candidate; break; fi
    done
fi
[[ -n "$sqlite_lib" && -e "$sqlite_lib" ]] || { echo "runtime SQLite library not found; set METADATA_DESCRIPTOR_RUNTIME_SQLITE" >&2; exit 1; }
export METADATA_DESCRIPTOR_UPSTREAM_HOST="$admin_host"
export METADATA_DESCRIPTOR_UPSTREAM_PORT="$admin_port"
export METADATA_DESCRIPTOR_RUNTIME_SQLITE="$sqlite_lib"
export METADATA_DESCRIPTOR_RUNTIME_SHIM="$shim"
export PLEX_PG_HOST="$admin_host" PLEX_PG_PORT="$admin_port"
export PLEX_PG_DATABASE="$database" PLEX_PG_USER="$role" PLEX_PG_PASSWORD="$password" PLEX_PG_SCHEMA=public
export PLEX_PG_LOG_FILE="$fixture/shim.log"
export PLEX_PG_RETRY_DELAYS=10,20,50
(
    if [[ $(rustc --print cfg) == *'target_env="musl"'* ]]; then
        if [[ -n ${CARGO_ENCODED_RUSTFLAGS:-} ]]; then
            export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}"$'\x1f-C\x1ftarget-feature=-crt-static'
        else
            export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=-crt-static"
        fi
    fi
    cargo test --manifest-path "$repo/rust/plex-pg-core/Cargo.toml" --release --test metadata_descriptor_runtime -- --ignored --exact metadata_descriptor_is_atomic_across_empty_results_and_postgres_outage --nocapture
)

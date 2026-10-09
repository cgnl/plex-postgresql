#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
shim=
reconnect=()
while (($#)); do
    case "$1" in
        --shim) shim=${2:?--shim requires a path}; shift 2 ;;
        --reconnect) reconnect=(--reconnect); shift ;;
        *) echo "usage: $0 [--shim PATH] [--reconnect]" >&2; exit 2 ;;
    esac
done
for variable in $(env | sed -n 's/^\(PLEX_PG_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$variable"; done
for variable in $(env | sed -n 's/^\(PG[A-Z0-9_]*\)=.*/\1/p'); do unset "$variable"; done
unset LD_PRELOAD DYLD_INSERT_LIBRARIES
fixture=$(mktemp -d "/tmp/runtime-e2e.XXXXXXXX")
touch "$fixture/.runtime-e2e-fixture"
pg_bin=${RUNTIME_E2E_PG_BIN:-}
if [[ -z "$pg_bin" ]]; then
    for candidate in /opt/homebrew/opt/postgresql@15/bin /usr/lib/postgresql/18/bin /usr/lib/postgresql/15/bin; do
        if [[ -x "$candidate/psql" ]]; then pg_bin=$candidate; break; fi
    done
fi
if [[ -n "$pg_bin" ]]; then export PATH="$pg_bin:$PATH"; fi
cluster_started=0
external_fixture_ready=0
database="runtime_e2e_${$}_${RANDOM}"
role="$database"
password="fixture_${RANDOM}_${RANDOM}"
cleanup() {
    status=$?
    trap - EXIT
    if [[ $cluster_started == 1 ]]; then
        pg_ctl -D "$fixture/pgdata" -m immediate -w stop >/dev/null 2>&1 || true
    elif [[ $external_fixture_ready == 1 ]]; then
        psql -X -h "$admin_host" -p "$admin_port" -U "$admin_user" -d "$admin_database" -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS $database WITH (FORCE)" -c "DROP ROLE IF EXISTS $role" >/dev/null 2>&1 || echo "WARNING: fixture cleanup failed: $database/$role" >&2
    fi
    if [[ $status != 0 && -f "$fixture/postgres.log" ]]; then tail -n 60 "$fixture/postgres.log" >&2; fi
    if [[ $status != 0 && -f "$fixture/shim.log" ]]; then tail -n 80 "$fixture/shim.log" >&2; fi
    rm -rf "$fixture"
    exit "$status"
}
trap cleanup EXIT
if [[ ${RUNTIME_E2E_EXTERNAL_FIXTURE:-0} == 1 ]]; then
    admin_host=${RUNTIME_E2E_ADMIN_HOST:?explicit fixture admin host required}
    [[ "$admin_host" == 127.0.0.1 ]] || { echo "external fixture must be loopback" >&2; exit 2; }
    admin_port=${RUNTIME_E2E_ADMIN_PORT:?explicit fixture admin port required}
    admin_user=${RUNTIME_E2E_ADMIN_USER:?explicit fixture admin user required}
    admin_database=${RUNTIME_E2E_ADMIN_DATABASE:?explicit fixture admin database required}
    export PGPASSWORD=${RUNTIME_E2E_ADMIN_PASSWORD:?explicit fixture admin password required}
else
    command -v initdb >/dev/null || { echo "initdb required; set RUNTIME_E2E_PG_BIN (no skips)" >&2; exit 1; }
    initdb -D "$fixture/pgdata" -U runtime_e2e_admin --auth=trust --no-locale >/dev/null
    mkdir "$fixture/socket"
    cluster_started=1
    pg_ctl -D "$fixture/pgdata" -l "$fixture/postgres.log" -o "-k $fixture/socket -p 55439 -c listen_addresses=''" -w start >/dev/null
    admin_host="$fixture/socket"
    admin_port=55439
    admin_user=runtime_e2e_admin
    admin_database=postgres
fi
[[ "$admin_port" =~ ^[0-9]+$ ]] || { echo "invalid fixture port" >&2; exit 2; }
if [[ ${RUNTIME_E2E_EXTERNAL_FIXTURE:-0} == 1 ]]; then
    [[ "$admin_user" == runtime_e2e_* && "$admin_database" == runtime_e2e_* ]] || { echo "external fixture admin identity must use runtime_e2e_ names" >&2; exit 2; }
    external_fixture_ready=1
fi
psql -X -h "$admin_host" -p "$admin_port" -U "$admin_user" -d "$admin_database" -v ON_ERROR_STOP=1 \
    -c "CREATE ROLE $role LOGIN PASSWORD '$password'" \
    -c "CREATE DATABASE $database OWNER $role" \
    -c "COMMENT ON DATABASE $database IS 'runtime-e2e disposable fixture'" >/dev/null
unset PGPASSWORD
export RUNTIME_E2E_ISOLATED=1 RUNTIME_E2E_DIR="$fixture"
export PLEX_PG_HOST="$admin_host" PLEX_PG_PORT="$admin_port" PLEX_PG_DATABASE="$database"
export PLEX_PG_USER="$role" PLEX_PG_PASSWORD="$password" PLEX_PG_SCHEMA=runtime_e2e
export PLEX_PG_LOG_FILE="$fixture/shim.log"
export PLEX_PG_RETRY_DELAYS=10,20,50
if [[ -z "$shim" ]]; then
    cargo build --manifest-path "$repo/rust/plex-pg-core/Cargo.toml" --release --lib --features interpose
    case "$(uname -s)" in
        Darwin) artifact=db_interpose_pg.dylib ;;
        Linux) artifact=db_interpose_pg.so ;;
        *) echo "unsupported platform (no skips)" >&2; exit 1 ;;
    esac
    if [[ "$artifact" == db_interpose_pg.so ]]; then
        make -C "$repo" -B "$artifact" LDFLAGS='-Wl,--no-as-needed -lpq -lsqlite3 -ldl -lpthread'
    else
        make -C "$repo" -B "$artifact"
    fi
    shim="$repo/$artifact"
fi
[[ -f "$shim" ]] || { echo "missing shared artifact: $shim" >&2; exit 1; }
shim=$(cd "$(dirname "$shim")" && pwd)/$(basename "$shim")
(
    if [[ $(rustc --print cfg) == *'target_env="musl"'* ]]; then
        if [[ -n ${CARGO_ENCODED_RUSTFLAGS:-} ]]; then
            export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}"$'\x1f-C\x1ftarget-feature=-crt-static'
        else
            export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=-crt-static"
        fi
    fi
    cargo build --manifest-path "$repo/rust/plex-pg-core/Cargo.toml" --release --bin runtime_e2e
)
cp "$repo/rust/plex-pg-core/target/release/runtime_e2e" "$fixture/Plex Media Server"
export RUNTIME_E2E_SHIM="$shim"
"$fixture/Plex Media Server" --shim-env "${reconnect[@]}"

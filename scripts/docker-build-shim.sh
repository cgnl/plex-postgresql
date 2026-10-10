#!/bin/sh
# Shared Alpine/musl builder for plex-postgresql Docker images.
# Builds PostgreSQL libpq (without OpenSSL), Rust core, shim .so, and /libs payload.

set -eu

WITH_NOOP=0
if [ "${1:-}" = "--with-noop" ]; then
    WITH_NOOP=1
fi

SANITIZE="${PLEX_PG_SANITIZE:-}"
SANITIZE_FLAGS=""
SANITIZE_LDFLAGS=""
if [ -n "${SANITIZE}" ] && [ "${SANITIZE}" != "0" ]; then
    echo "=== Sanitizers enabled: ${SANITIZE} ==="
    SANITIZE_FLAGS="-O1 -g -fno-omit-frame-pointer -fno-optimize-sibling-calls -fsanitize=${SANITIZE}"
    SANITIZE_LDFLAGS="-fsanitize=${SANITIZE}"
fi

echo "=== Building PostgreSQL/libpq (musl) ==="
cd /build
PG_VERSION="15.10"
PG_TARBALL="postgresql-${PG_VERSION}.tar.gz"
PG_URL="https://ftp.postgresql.org/pub/source/v${PG_VERSION}/${PG_TARBALL}"
PG_CACHE_DIR="/build/.cache"
PG_CACHE_PATH="${PG_CACHE_DIR}/${PG_TARBALL}"
mkdir -p "${PG_CACHE_DIR}"
if [ ! -s "${PG_CACHE_PATH}" ]; then
    echo "Downloading ${PG_TARBALL}..."
    curl -fL --retry 3 --retry-delay 2 "${PG_URL}" -o "${PG_CACHE_PATH}"
else
    echo "Using cached ${PG_TARBALL}"
fi
tar xzf "${PG_CACHE_PATH}"

cd "/build/postgresql-${PG_VERSION}"
ARCH="$(uname -m)"
if [ "$ARCH" = "x86_64" ]; then
    PG_CFLAGS='-O0 -mno-sse4.2'
else
    PG_CFLAGS='-O2'
fi

CFLAGS="$PG_CFLAGS" ac_cv_func_getaddrinfo=yes ./configure --prefix=/usr/local/pgsql \
    --without-readline \
    --without-zlib \
    --without-openssl \
    --without-icu

cd src/include && make install
cd ../interfaces/libpq && make && make install
cd ../../bin/pg_config && make && make install

echo "=== Building Rust core ==="
cd /build/rust/plex-pg-core
: "${CARGO_TARGET_DIR:=/build/target}"
cargo +stable build --release --lib --features interpose

echo "=== Building shim .so ==="
cd /build
ARCH="$(uname -m)"
echo "Building for architecture: $ARCH"
if [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then
    ARCH_FLAGS="-mno-outline-atomics"
else
    ARCH_FLAGS=""
fi

LIBPLEX_PG_CORE_A="${CARGO_TARGET_DIR:-/build/target}/release/libplex_pg_core.a"
if [ ! -f "${LIBPLEX_PG_CORE_A}" ]; then
    echo "ERROR: missing static lib ${LIBPLEX_PG_CORE_A}"
    exit 1
fi

gcc -shared -fPIC -fno-stack-protector \
    -std=c11 -D_GNU_SOURCE $ARCH_FLAGS $SANITIZE_FLAGS \
    -o db_interpose_pg.so \
    -Wl,--whole-archive "${LIBPLEX_PG_CORE_A}" -Wl,--no-whole-archive \
    -Iinclude -I/usr/local/pgsql/include -I/usr/include \
    -L/usr/local/pgsql/lib -lpq \
    -lstdc++ \
    -ldl -lpthread \
    $SANITIZE_LDFLAGS \
    -Wl,-rpath,/usr/local/lib/plex-postgresql \
    -Wl,-rpath,/usr/lib/plexmediaserver/lib

echo "=== Shim dependencies ==="
LD_LIBRARY_PATH=/usr/local/pgsql/lib ldd db_interpose_pg.so || true

echo "=== Collecting runtime libs ==="
mkdir -p /libs
cp db_interpose_pg.so /libs/
cp /usr/local/pgsql/lib/libpq.so.5* /libs/
cp /usr/lib/libgcc_s.so.1 /libs/
if [ -n "${SANITIZE_FLAGS}" ]; then
    for lib in asan ubsan; do
        lib_path="$(gcc -print-file-name=lib${lib}.so || true)"
        if [ -n "$lib_path" ] && [ "$lib_path" != "lib${lib}.so" ] && [ -f "$lib_path" ]; then
            cp -L "${lib_path}"* /libs/
        else
            echo "WARNING: lib${lib}.so not found for sanitizer runtime"
        fi
    done
fi

echo "=== Building subreaper wrapper ==="
cat > subreaper.c << 'SUBREAPER_EOF'
/* subreaper: wrap a child process with PR_SET_CHILD_SUBREAPER so that
 * orphaned grandchildren (e.g. PMS re-exec via vfork) are reparented
 * here instead of PID 1. We wait for ALL descendants, not just the
 * direct child, so s6 sees us alive the entire time. */
#include <sys/prctl.h>
#include <sys/wait.h>
#include <signal.h>
#include <unistd.h>
#include <stdio.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
static volatile pid_t child_pid = 0;
static volatile int got_sigterm = 0;
static int exit_diagnostics = 0;
static unsigned int exit_records = 0;
static int exit_records_truncated = 0;
static void report_exit(pid_t p,int st,int direct){
  if(!exit_diagnostics) return;
  if(exit_records>=64&&!direct){
    if(!exit_records_truncated){
      fprintf(stderr,"plex-pg-subreaper exit-records-truncated limit=64\n");
      exit_records_truncated=1;
    }
    return;
  }
  int core=0;
#ifdef WCOREDUMP
  if(WIFSIGNALED(st)) core=WCOREDUMP(st)?1:0;
#endif
  fprintf(stderr,"plex-pg-subreaper child-exit pid=%ld relation=%s status=0x%x exited=%d exit_code=%d signaled=%d signal=%d core=%d\n",
    (long)p,direct?"direct":"adopted",(unsigned int)st,
    WIFEXITED(st)?1:0,WIFEXITED(st)?WEXITSTATUS(st):-1,
    WIFSIGNALED(st)?1:0,WIFSIGNALED(st)?WTERMSIG(st):0,core);
  exit_records++;
}
void fwd(int s){
  if(s==SIGTERM||s==SIGINT) got_sigterm=1;
  /* Forward to all processes in our process group */
  kill(0,s);
}
int main(int c,char**v){
  if(c<2){fprintf(stderr,"usage: subreaper cmd [args...]\n");return 1;}
  const char *diagnostics=getenv("PLEX_PG_REAPER_DIAGNOSTICS");
  exit_diagnostics=diagnostics&&strcmp(diagnostics,"1")==0;
  prctl(PR_SET_CHILD_SUBREAPER,1,0,0,0);
  signal(SIGTERM,fwd);signal(SIGINT,fwd);signal(SIGHUP,fwd);
  child_pid=fork();
  if(child_pid==0){execvp(v[1],v+1);perror("exec");_exit(127);}
  if(child_pid<0){perror("fork");return 1;}
  int st,exit_code=0;pid_t p;
  /* Wait for ALL children, not just the direct child.
   * When PMS vfork+exec's a new copy and the parent exits, the new
   * PMS process is reparented to us (subreaper). We keep waiting
   * until there are no more children (ECHILD). */
  while((p=wait(&st))>0||(p<0&&errno==EINTR)){
    if(p<=0) continue;
    report_exit(p,st,p==child_pid);
    if(p==child_pid){
      /* Record direct child exit code, but keep waiting for grandchildren */
      exit_code=WIFEXITED(st)?WEXITSTATUS(st):128+WTERMSIG(st);
      child_pid=0; /* no longer forward signals to dead child */
    }
  }
  return exit_code;
}
SUBREAPER_EOF
gcc -static -O2 -o subreaper subreaper.c
cp subreaper /libs/

if [ "$WITH_NOOP" = "1" ]; then
    echo "=== Building static noop binary for CrashUploader replacement ==="
    printf '#include <unistd.h>\nint main(void){_exit(0);}\n' > noop.c
    gcc -static -o noop noop.c
    cp noop /libs/
fi

ls -la /libs/

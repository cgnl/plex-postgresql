//! Explicit real-PostgreSQL/shared-shim test; run with --ignored --nocapture.
//! Use a musl runner for a musl shim. The public repr(C) connection type is
//! used only to inspect its libpq handle and mark an actual single-row stream.
use libc::{c_char, c_int, c_void};
use plex_pg_core::ffi_types::PgConnection;
use postgres::{Config, NoTls};
use std::ffi::{CStr, CString};
use std::sync::{atomic::Ordering, mpsc};
use std::thread;
use std::time::Duration;

type Handle = *mut c_void;
#[derive(Clone, Copy)]
struct Api {
    init: unsafe extern "C" fn(c_int, c_int, c_int),
    get: unsafe extern "C" fn(*const c_char) -> Handle,
    cleanup: unsafe extern "C" fn(),
    clear_stream: unsafe extern "C" fn(Handle) -> c_int,
    exec: unsafe extern "C" fn(Handle, *const c_char) -> Handle,
    status: unsafe extern "C" fn(Handle) -> c_int,
    txn: unsafe extern "C" fn(Handle) -> c_int,
    clear: unsafe extern "C" fn(Handle),
    send: unsafe extern "C" fn(Handle, *const c_char) -> c_int,
    single: unsafe extern "C" fn(Handle) -> c_int,
    result: unsafe extern "C" fn(Handle) -> Handle,
    trace: unsafe extern "C" fn(Handle, *mut libc::FILE),
    untrace: unsafe extern "C" fn(Handle),
}

unsafe fn symbol<T: Copy>(library: Handle, name: &str) -> T {
    let name = CString::new(name).unwrap();
    let address = libc::dlsym(library, name.as_ptr());
    assert!(!address.is_null(), "missing symbol {name:?}");
    std::mem::transmute_copy(&address)
}

impl Api {
    unsafe fn load(path: &str) -> Self {
        let path = CString::new(path).unwrap();
        let library = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if library.is_null() {
            panic!(
                "dlopen: {}",
                CStr::from_ptr(libc::dlerror()).to_string_lossy()
            );
        }
        let api = Self {
            init: symbol(library, "rust_pool_init"),
            get: symbol(library, "rust_pool_get_connection"),
            cleanup: symbol(library, "rust_pool_cleanup"),
            clear_stream: symbol(library, "rust_pool_clear_streaming_active"),
            exec: symbol(library, "PQexec"),
            status: symbol(library, "PQresultStatus"),
            txn: symbol(library, "PQtransactionStatus"),
            clear: symbol(library, "PQclear"),
            send: symbol(library, "PQsendQuery"),
            single: symbol(library, "PQsetSingleRowMode"),
            result: symbol(library, "PQgetResult"),
            trace: symbol(library, "PQtrace"),
            untrace: symbol(library, "PQuntrace"),
        };
        let mut info: libc::Dl_info = std::mem::zeroed();
        assert_ne!(libc::dladdr(api.get as *const c_void, &mut info), 0);
        let actual = CStr::from_ptr(info.dli_fname).to_string_lossy();
        assert_eq!(
            std::fs::canonicalize(actual.as_ref()).unwrap(),
            std::fs::canonicalize(path.to_str().unwrap()).unwrap()
        );
        println!(
            "ARTIFACT {} ABI {}/{}",
            actual,
            std::env::consts::ARCH,
            if cfg!(target_env = "musl") {
                "musl"
            } else {
                "non-musl"
            }
        );
        // Keep the library loaded until all owner TLS destructors have run.
        api
    }

    unsafe fn acquire(self) -> Handle {
        (self.get)(c"/tmp/owner-runtime/com.plexapp.plugins.library.db".as_ptr())
    }

    unsafe fn pq(conn: Handle) -> Handle {
        assert!(!conn.is_null(), "shared pool returned null");
        (*(conn as *mut PgConnection)).conn.cast()
    }

    unsafe fn query(self, conn: Handle, sql: &str, expected_status: i32) {
        let sql = CString::new(sql).unwrap();
        let result = (self.exec)(Self::pq(conn), sql.as_ptr());
        assert!(!result.is_null());
        let status = (self.status)(result);
        (self.clear)(result);
        assert_eq!(status, expected_status, "SQL {sql:?}");
    }
}

fn expire_owner_idle_period() {
    // Production maintenance uses integer seconds and strictly > 10 seconds.
    thread::sleep(Duration::from_secs(11));
}

unsafe fn protocol_file() -> *mut libc::FILE {
    let directory = std::env::var("TMPDIR").expect("explicit writable fixture TMPDIR required");
    let mut path = CString::new(format!("{directory}/pool-owner-protocol-XXXXXX"))
        .unwrap()
        .into_bytes_with_nul();
    let fd = libc::mkstemp(path.as_mut_ptr().cast());
    assert!(
        fd >= 0,
        "cannot create fixture trace file: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(libc::unlink(path.as_ptr().cast()), 0);
    let file = libc::fdopen(fd, c"w+".as_ptr());
    if file.is_null() {
        libc::close(fd);
    }
    assert!(!file.is_null());
    file
}

#[test]
#[ignore = "requires explicit disposable PostgreSQL database and matching shared shim"]
fn dead_owner_transactions_and_live_stream_are_not_leaked() {
    let database = std::env::var("PLEX_PG_DATABASE").expect("explicit fixture database required");
    assert!(
        database.starts_with("owner_runtime_"),
        "disposable fixture required"
    );
    let mut observer = Config::new()
        .host(&std::env::var("PLEX_PG_HOST").unwrap())
        .port(std::env::var("PLEX_PG_PORT").unwrap().parse().unwrap())
        .user(&std::env::var("PLEX_PG_USER").unwrap())
        .password(std::env::var("PLEX_PG_PASSWORD").unwrap())
        .dbname(&database)
        .connect(NoTls)
        .unwrap();
    let marker: Option<String> = observer.query_one(
        "SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname=current_database()", &[]
    ).unwrap().get(0);
    assert_eq!(
        marker.as_deref(),
        Some("pool-owner-runtime disposable fixture")
    );
    observer
        .batch_execute("CREATE TABLE owner_writes (id integer PRIMARY KEY)")
        .unwrap();
    println!(
        "SERVER {}",
        observer
            .query_one("SHOW server_version", &[])
            .unwrap()
            .get::<_, String>(0)
    );
    let api = unsafe { Api::load(&std::env::var("POOL_OWNER_RUNTIME_SHIM").unwrap()) };

    for (value, failed) in [(1, false), (2, true)] {
        unsafe { (api.init)(1, 1, 10) };
        // libpq's actual protocol trace distinguishes explicit ROLLBACK from
        // merely resetting/disconnecting the transaction-bearing backend.
        let protocol = unsafe { protocol_file() };
        let protocol_ptr = protocol as usize;
        let abandoned = thread::spawn(move || unsafe {
            let conn = api.acquire();
            (api.trace)(Api::pq(conn), protocol_ptr as *mut libc::FILE);
            api.query(conn, "BEGIN", 1);
            api.query(
                conn,
                &format!("INSERT INTO owner_writes VALUES ({value})"),
                1,
            );
            if failed {
                api.query(conn, "SELECT 1 / 0", 7);
            }
            assert_eq!((api.txn)(Api::pq(conn)), if failed { 3 } else { 2 });
            conn as usize // Deliberately no release/rollback: real owner exits.
        })
        .join()
        .unwrap();
        let pending: i64 = observer.query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND state LIKE 'idle in transaction%'", &[]
        ).unwrap().get(0);
        assert_eq!(
            pending, 1,
            "abandoned transaction must actually remain open"
        );
        expire_owner_idle_period();
        thread::spawn(move || unsafe {
            let reused = api.acquire();
            assert_eq!(
                reused as usize, abandoned,
                "must exercise phase2 reuse, not a fresh allocation"
            );
            assert_eq!(
                (api.txn)(Api::pq(reused)),
                0,
                "new owner inherited transaction state"
            );
            api.query(reused, "INSERT INTO owner_writes VALUES (100)", 1);
            api.query(reused, "DELETE FROM owner_writes WHERE id=100", 1);
            (api.untrace)(Api::pq(reused));
        })
        .join()
        .unwrap();
        let trace = unsafe {
            assert_eq!(libc::fflush(protocol), 0);
            assert_eq!(libc::fseek(protocol, 0, libc::SEEK_END), 0);
            let length = libc::ftell(protocol);
            assert!(length > 0 && length < 100_000);
            assert_eq!(libc::fseek(protocol, 0, libc::SEEK_SET), 0);
            let mut bytes = vec![0_u8; length as usize];
            assert_eq!(
                libc::fread(bytes.as_mut_ptr().cast(), 1, bytes.len(), protocol),
                bytes.len()
            );
            assert_eq!(libc::fclose(protocol), 0);
            String::from_utf8(bytes).unwrap()
        };
        assert!(
            trace.contains("ROLLBACK"),
            "reclamation did not send explicit rollback before reset: {trace}"
        );
        let count: i64 = observer
            .query_one("SELECT count(*) FROM owner_writes", &[])
            .unwrap()
            .get(0);
        assert_eq!(
            count, 0,
            "abandoned writes were committed during reclamation"
        );
        unsafe { (api.cleanup)() };
        println!(
            "PASS abandoned {} rollback and same-slot reuse",
            if failed { "INERROR" } else { "INTRANS" }
        );
    }

    unsafe { (api.init)(1, 1, 10) };
    let (ready_tx, ready_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let live = thread::spawn(move || unsafe {
        let conn = api.acquire();
        api.query(conn, "BEGIN", 1);
        ready_tx.send(()).unwrap();
        finish_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(api.acquire(), conn, "live owner lost its own connection");
        assert_eq!((api.txn)(Api::pq(conn)), 2);
        api.query(conn, "ROLLBACK", 1);
    });
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    expire_owner_idle_period();
    thread::spawn(move || unsafe {
        assert!(api.acquire().is_null(), "live owner was reclaimed");
    })
    .join()
    .unwrap();
    finish_tx.send(()).unwrap();
    live.join().unwrap();
    unsafe { (api.cleanup)() };
    println!("PASS expired-idle live owner remains reserved");

    unsafe { (api.init)(1, 1, 10) };
    let stream = thread::spawn(move || unsafe {
        let conn = api.acquire();
        let pq = Api::pq(conn);
        assert_eq!((api.send)(pq, c"SELECT generate_series(1, 4)".as_ptr()), 1);
        assert_eq!((api.single)(pq), 1);
        let first = (api.result)(pq);
        assert!(!first.is_null());
        assert_eq!(
            (api.status)(first),
            9,
            "requires a real single-tuple result"
        );
        (api.clear)(first);
        (*(conn as *mut PgConnection))
            .streaming_active
            .store(1, Ordering::Release);
        conn as usize // Deliberately abandon a real libpq stream.
    })
    .join()
    .unwrap();
    expire_owner_idle_period();
    thread::spawn(move || unsafe {
        assert!(
            api.acquire().is_null(),
            "dead streaming owner was reclaimed"
        );
    })
    .join()
    .unwrap();
    unsafe {
        let conn = stream as Handle;
        assert_eq!(
            (*(conn as *mut PgConnection))
                .streaming_active
                .load(Ordering::Acquire),
            1
        );
        let mut tuples = 0;
        loop {
            let result = (api.result)(Api::pq(conn));
            if result.is_null() {
                break;
            }
            let status = (api.status)(result);
            (api.clear)(result);
            assert!(status == 9 || status == 2);
            if status == 9 {
                tuples += 1;
            }
        }
        assert_eq!(
            tuples, 3,
            "stream must remain intact during failed reclamation"
        );
        assert_eq!((api.clear_stream)(conn), 1);
        assert_eq!(
            (*(conn as *mut PgConnection))
                .streaming_active
                .load(Ordering::Acquire),
            0
        );
    }
    thread::spawn(move || unsafe {
        let reused = api.acquire();
        assert_eq!(reused as usize, stream);
        api.query(reused, "SELECT 1", 2);
    })
    .join()
    .unwrap();
    unsafe { (api.cleanup)() };
    println!("PASS dead streaming owner reserved until actual stream drained");
}

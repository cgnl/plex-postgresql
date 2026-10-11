//! Explicit real-PostgreSQL/shared-shim regression for prepared descriptors.
//!
//! Run this ignored test with a musl test binary and a shim built from the
//! same source revision. `METADATA_DESCRIPTOR_RUNTIME_SQLITE` must name the
//! SQLite library used by that shim. The upstream database must be disposable
//! and carry the marker checked below.

use libc::{c_char, c_int, c_void};
use postgres::{Config, NoTls};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

type Handle = *mut c_void;

#[derive(Clone, Copy)]
struct Api {
    open: unsafe extern "C" fn(*const c_char, *mut Handle) -> c_int,
    close: unsafe extern "C" fn(Handle) -> c_int,
    prepare: unsafe extern "C" fn(
        Handle,
        *const c_char,
        c_int,
        *mut Handle,
        *mut *const c_char,
    ) -> c_int,
    step: unsafe extern "C" fn(Handle) -> c_int,
    reset: unsafe extern "C" fn(Handle) -> c_int,
    finalize: unsafe extern "C" fn(Handle) -> c_int,
    column_count: unsafe extern "C" fn(Handle) -> c_int,
    column_name: unsafe extern "C" fn(Handle, c_int) -> *const c_char,
    column_decltype: unsafe extern "C" fn(Handle, c_int) -> *const c_char,
    column_int64: unsafe extern "C" fn(Handle, c_int) -> i64,
    errmsg: unsafe extern "C" fn(Handle) -> *const c_char,
}

unsafe fn symbol<T: Copy>(library: Handle, name: &str) -> T {
    let name = CString::new(name).unwrap();
    let address = libc::dlsym(library, name.as_ptr());
    assert!(!address.is_null(), "missing symbol {name:?}");
    std::mem::transmute_copy(&address)
}

impl Api {
    unsafe fn load(sqlite_path: &str, shim_path: &str) -> Self {
        let sqlite_path = CString::new(sqlite_path).unwrap();
        let sqlite = libc::dlopen(sqlite_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        assert!(!sqlite.is_null(), "SQLite dlopen failed: {}", dlerror());

        let shim_path_c = CString::new(shim_path).unwrap();
        let shim = libc::dlopen(shim_path_c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        assert!(!shim.is_null(), "shim dlopen failed: {}", dlerror());
        let api = Self {
            open: symbol(shim, "sqlite3_open"),
            close: symbol(shim, "sqlite3_close"),
            prepare: symbol(shim, "sqlite3_prepare_v2"),
            step: symbol(shim, "sqlite3_step"),
            reset: symbol(shim, "sqlite3_reset"),
            finalize: symbol(shim, "sqlite3_finalize"),
            column_count: symbol(shim, "sqlite3_column_count"),
            column_name: symbol(shim, "sqlite3_column_name"),
            column_decltype: symbol(shim, "sqlite3_column_decltype"),
            column_int64: symbol(shim, "sqlite3_column_int64"),
            errmsg: symbol(shim, "sqlite3_errmsg"),
        };
        let mut info: libc::Dl_info = std::mem::zeroed();
        assert_ne!(libc::dladdr(api.prepare as *const c_void, &mut info), 0);
        let actual = CStr::from_ptr(info.dli_fname).to_string_lossy();
        assert_eq!(
            std::fs::canonicalize(actual.as_ref()).unwrap(),
            std::fs::canonicalize(shim_path).unwrap(),
            "sqlite3_prepare_v2 did not resolve to the candidate shim"
        );
        api
    }

    unsafe fn prepare_sql(self, db: Handle, sql: &CStr) -> (c_int, Handle) {
        let mut stmt = std::ptr::null_mut();
        let mut tail = std::ptr::null();
        let rc = (self.prepare)(db, sql.as_ptr(), -1, &mut stmt, &mut tail);
        (rc, stmt)
    }

    unsafe fn prepare_query(self, db: Handle) -> (c_int, Handle) {
        self.prepare_sql(
            db,
            c"select metadata_type,count(*) from metadata_items group by metadata_type",
        )
    }

    unsafe fn error(self, db: Handle) -> String {
        let error = (self.errmsg)(db);
        if error.is_null() {
            "<null sqlite3_errmsg>".to_string()
        } else {
            CStr::from_ptr(error).to_string_lossy().into_owned()
        }
    }
}

unsafe fn dlerror() -> String {
    let error = libc::dlerror();
    if error.is_null() {
        "<no dlerror>".to_string()
    } else {
        CStr::from_ptr(error).to_string_lossy().into_owned()
    }
}

struct ProxyState {
    online: bool,
    streams: Vec<TcpStream>,
}

struct PgProxy {
    address: std::net::SocketAddr,
    state: Arc<Mutex<ProxyState>>,
    running: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PgProxy {
    fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(ProxyState {
            online: true,
            streams: Vec::new(),
        }));
        let running = Arc::new(AtomicBool::new(true));
        let worker_state = Arc::clone(&state);
        let worker_running = Arc::clone(&running);
        let worker = thread::spawn(move || {
            while worker_running.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let mut state = worker_state.lock().unwrap();
                        if !state.online {
                            continue;
                        }
                        let server = TcpStream::connect(&upstream).unwrap();
                        client.set_nodelay(true).unwrap();
                        server.set_nodelay(true).unwrap();
                        state.streams.push(client.try_clone().unwrap());
                        state.streams.push(server.try_clone().unwrap());
                        let client_read = client.try_clone().unwrap();
                        let server_read = server.try_clone().unwrap();
                        thread::spawn(move || pipe(client_read, server));
                        thread::spawn(move || pipe(server_read, client));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("proxy accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            state,
            running,
            worker: Some(worker),
        }
    }

    fn set_online(&self, online: bool) {
        let mut state = self.state.lock().unwrap();
        state.online = online;
        if !online {
            for stream in state.streams.drain(..) {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
}

impl Drop for PgProxy {
    fn drop(&mut self) {
        self.set_online(false);
        self.running.store(false, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn pipe(mut reader: TcpStream, mut writer: TcpStream) {
    let _ = io::copy(&mut reader, &mut writer);
    let _ = writer.shutdown(Shutdown::Write);
}

unsafe fn assert_descriptor(api: Api, stmt: Handle) {
    assert_eq!((api.column_count)(stmt), 2);
    for (index, expected_name, expected_type) in [
        (0, "metadata_type", "INTEGER"),
        (1, "count", "dt_integer(8)"),
    ] {
        let name = (api.column_name)(stmt, index);
        let decltype = (api.column_decltype)(stmt, index);
        assert!(!name.is_null(), "column {index} has no name");
        assert!(!decltype.is_null(), "column {index} has no declared type");
        assert_eq!(CStr::from_ptr(name).to_str().unwrap(), expected_name);
        assert_eq!(CStr::from_ptr(decltype).to_str().unwrap(), expected_type);
    }
}

unsafe fn fetch_groups(api: Api, stmt: Handle) -> BTreeMap<i64, i64> {
    let mut rows = BTreeMap::new();
    loop {
        match (api.step)(stmt) {
            100 => {
                rows.insert((api.column_int64)(stmt, 0), (api.column_int64)(stmt, 1));
            }
            101 => return rows,
            rc => panic!("sqlite3_step failed with {rc}"),
        }
    }
}

#[cfg(target_env = "musl")]
fn require_musl_runner() {}

#[cfg(not(target_env = "musl"))]
fn require_musl_runner() {
    panic!("this regression requires a musl runner");
}

#[test]
#[ignore = "requires an explicit disposable PostgreSQL database and matching musl shim"]
fn metadata_descriptor_is_atomic_across_empty_results_and_postgres_outage() {
    require_musl_runner();
    let upstream_host = std::env::var("METADATA_DESCRIPTOR_UPSTREAM_HOST").unwrap();
    let upstream_port: u16 = std::env::var("METADATA_DESCRIPTOR_UPSTREAM_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let database = std::env::var("PLEX_PG_DATABASE").unwrap();
    assert!(
        database.starts_with("metadata_descriptor_"),
        "disposable metadata_descriptor_* database required"
    );
    let user = std::env::var("PLEX_PG_USER").unwrap();
    let password = std::env::var("PLEX_PG_PASSWORD").unwrap();
    let mut observer = Config::new()
        .host(&upstream_host)
        .port(upstream_port)
        .user(&user)
        .password(&password)
        .dbname(&database)
        .connect(NoTls)
        .unwrap();
    let marker: Option<String> = observer
        .query_one(
            "SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname=current_database()",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(
        marker.as_deref(),
        Some("metadata-descriptor-runtime disposable fixture")
    );
    observer
        .batch_execute(
            "CREATE TABLE metadata_items (id bigint PRIMARY KEY, metadata_type integer NOT NULL);\
             INSERT INTO metadata_items VALUES (1, 1), (2, 1), (3, 4);",
        )
        .unwrap();

    let proxy = PgProxy::start(format!("{upstream_host}:{upstream_port}"));
    std::env::set_var("PLEX_PG_HOST", proxy.address.ip().to_string());
    std::env::set_var("PLEX_PG_PORT", proxy.address.port().to_string());
    let api = unsafe {
        Api::load(
            &std::env::var("METADATA_DESCRIPTOR_RUNTIME_SQLITE").unwrap(),
            &std::env::var("METADATA_DESCRIPTOR_RUNTIME_SHIM").unwrap(),
        )
    };

    unsafe {
        let mut db = std::ptr::null_mut();
        assert_eq!(
            (api.open)(
                c"/tmp/metadata-descriptor/com.plexapp.plugins.library.db".as_ptr(),
                &mut db
            ),
            0
        );
        assert!(!db.is_null());

        let (rc, stmt) = api.prepare_query(db);
        assert_eq!(rc, 0, "initial prepare: {}", api.error(db));
        assert!(!stmt.is_null());
        assert_descriptor(api, stmt);
        assert_eq!(fetch_groups(api, stmt), BTreeMap::from([(1, 2), (4, 1)]));

        observer
            .batch_execute("DELETE FROM metadata_items")
            .unwrap();
        assert_eq!((api.reset)(stmt), 0);
        assert_descriptor(api, stmt);
        assert!(fetch_groups(api, stmt).is_empty());
        assert_descriptor(api, stmt);
        assert_eq!((api.finalize)(stmt), 0);

        let latency_started = Instant::now();
        for _ in 0..20 {
            let (latency_rc, latency_stmt) = api.prepare_query(db);
            assert_eq!(latency_rc, 0, "latency prepare: {}", api.error(db));
            assert_descriptor(api, latency_stmt);
            assert_eq!((api.finalize)(latency_stmt), 0);
        }
        let latency = latency_started.elapsed();
        assert!(
            latency < Duration::from_secs(5),
            "20 local descriptor prepares took {latency:?}"
        );

        proxy.set_online(false);
        for attempt in 0..8 {
            let (outage_rc, outage_stmt) = api.prepare_query(db);
            assert_ne!(
                outage_rc, 0,
                "outage prepare {attempt} published SQLITE_OK with zero columns"
            );
            assert!(
                outage_stmt.is_null(),
                "failed prepare {attempt} published a partial statement: {}",
                api.error(db)
            );
        }

        observer
            .batch_execute("INSERT INTO metadata_items VALUES (10, 2), (11, 2), (12, 7)")
            .unwrap();
        proxy.set_online(true);
        let (recovery_rc, recovered) = api.prepare_query(db);
        assert_eq!(recovery_rc, 0, "recovery prepare: {}", api.error(db));
        assert!(!recovered.is_null());
        assert_descriptor(api, recovered);
        assert_eq!(
            fetch_groups(api, recovered),
            BTreeMap::from([(2, 2), (7, 1)])
        );
        assert_eq!((api.finalize)(recovered), 0);

        let (leak_rc, leak_stmt) = api.prepare_sql(
            db,
            c"select count(*) from pg_prepared_statements where name like 'plex_descriptor_%'",
        );
        assert_eq!(leak_rc, 0, "prepared-statement audit: {}", api.error(db));
        assert_eq!((api.step)(leak_stmt), 100);
        assert_eq!(
            (api.column_int64)(leak_stmt, 0),
            0,
            "transient descriptor statements leaked on the server"
        );
        assert_eq!((api.step)(leak_stmt), 101);
        assert_eq!((api.finalize)(leak_stmt), 0);
        assert_eq!((api.close)(db), 0);
    }
}

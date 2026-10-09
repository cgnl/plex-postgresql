use libc::{c_char, c_int, c_void};
use postgres::{Client, Config, NoTls};
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::ptr;

type Handle = *mut c_void;
type Result<T> = std::result::Result<T, String>;
const ROW: i32 = 100;
const DONE: i32 = 101;
static DESTRUCTOR_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static DESTRUCTOR_POINTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" fn owned_text_destructor(pointer: *mut c_void) {
    if pointer as usize == DESTRUCTOR_POINTER.load(std::sync::atomic::Ordering::SeqCst) {
        DESTRUCTOR_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        libc::free(pointer);
    } else {
        // Preserve failure evidence without freeing an allocation that was not
        // transferred by this fixture.
        DESTRUCTOR_CALLS.store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
    }
}

struct Api {
    exec: unsafe extern "C" fn(
        Handle,
        *const c_char,
        Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        *mut c_void,
        *mut *mut c_char,
    ) -> c_int,
    free: unsafe extern "C" fn(*mut c_void),
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
    finalize: unsafe extern "C" fn(Handle) -> c_int,
    reset: unsafe extern "C" fn(Handle) -> c_int,
    clear: unsafe extern "C" fn(Handle) -> c_int,
    bind_int: unsafe extern "C" fn(Handle, c_int, i64) -> c_int,
    bind_null: unsafe extern "C" fn(Handle, c_int) -> c_int,
    bind_blob: unsafe extern "C" fn(
        Handle,
        c_int,
        *const c_void,
        c_int,
        Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int,
    bind_text: unsafe extern "C" fn(
        Handle,
        c_int,
        *const c_char,
        c_int,
        Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int,
    bind_text64: unsafe extern "C" fn(
        Handle,
        c_int,
        *const c_char,
        u64,
        Option<unsafe extern "C" fn(*mut c_void)>,
        u8,
    ) -> c_int,
    column_count: unsafe extern "C" fn(Handle) -> c_int,
    column_name: unsafe extern "C" fn(Handle, c_int) -> *const c_char,
    column_decltype: unsafe extern "C" fn(Handle, c_int) -> *const c_char,
    column_text: unsafe extern "C" fn(Handle, c_int) -> *const u8,
    column_int: unsafe extern "C" fn(Handle, c_int) -> i64,
    column_int32: unsafe extern "C" fn(Handle, c_int) -> c_int,
    column_double: unsafe extern "C" fn(Handle, c_int) -> f64,
    column_type: unsafe extern "C" fn(Handle, c_int) -> c_int,
    column_blob: unsafe extern "C" fn(Handle, c_int) -> *const c_void,
    column_bytes: unsafe extern "C" fn(Handle, c_int) -> c_int,
    rowid: unsafe extern "C" fn(Handle) -> i64,
    errmsg: unsafe extern "C" fn(Handle) -> *const c_char,
    errcode: unsafe extern "C" fn(Handle) -> c_int,
    extended: unsafe extern "C" fn(Handle) -> c_int,
}

unsafe fn symbol<T: Copy>(library: Handle, name: &str) -> Result<T> {
    let name = CString::new(name).map_err(|error| error.to_string())?;
    let address = libc::dlsym(library, name.as_ptr());
    if address.is_null() {
        return Err(format!(
            "missing shared shim export {}",
            name.to_string_lossy()
        ));
    }
    Ok(std::mem::transmute_copy(&address))
}

impl Api {
    unsafe fn load(path: &str) -> Result<Self> {
        let path = CString::new(path).map_err(|error| error.to_string())?;
        let library = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        if library.is_null() {
            let error = libc::dlerror();
            return Err(if error.is_null() {
                "dlopen failed".into()
            } else {
                CStr::from_ptr(error).to_string_lossy().into_owned()
            });
        }
        let prefix = if cfg!(target_os = "macos") { "my_" } else { "" };
        macro_rules! load {
            ($name:literal) => {
                symbol(library, &format!("{prefix}sqlite3_{}", $name))?
            };
        }
        let api = Self {
            exec: load!("exec"),
            free: load!("free"),
            open: load!("open"),
            close: load!("close"),
            prepare: load!("prepare_v2"),
            step: load!("step"),
            finalize: load!("finalize"),
            reset: load!("reset"),
            clear: load!("clear_bindings"),
            bind_int: load!("bind_int64"),
            bind_null: load!("bind_null"),
            bind_blob: load!("bind_blob"),
            bind_text: load!("bind_text"),
            bind_text64: load!("bind_text64"),
            column_count: load!("column_count"),
            column_name: load!("column_name"),
            column_decltype: load!("column_decltype"),
            column_text: load!("column_text"),
            column_int: load!("column_int64"),
            column_int32: load!("column_int"),
            column_double: load!("column_double"),
            column_type: load!("column_type"),
            column_blob: load!("column_blob"),
            column_bytes: load!("column_bytes"),
            rowid: load!("last_insert_rowid"),
            errmsg: load!("errmsg"),
            errcode: load!("errcode"),
            extended: load!("extended_errcode"),
        };
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(api.open as *const c_void, &mut info) == 0 || info.dli_fname.is_null() {
            return Err("cannot verify sqlite3_open shared artifact provenance".into());
        }
        let actual = PathBuf::from(CStr::from_ptr(info.dli_fname).to_string_lossy().as_ref());
        if actual.canonicalize().map_err(|error| error.to_string())?
            != PathBuf::from(path.to_string_lossy().as_ref())
                .canonicalize()
                .map_err(|error| error.to_string())?
        {
            return Err(format!(
                "sqlite3_open resolved outside selected shim: {}",
                actual.display()
            ));
        }
        println!("ARTIFACT {} ({}sqlite3_* ABI)", actual.display(), prefix);
        Ok(api)
    }

    fn error(&self, db: Handle) -> String {
        unsafe {
            let message = (self.errmsg)(db);
            if message.is_null() {
                "null errmsg".into()
            } else {
                CStr::from_ptr(message).to_string_lossy().into_owned()
            }
        }
    }

    fn check(&self, db: Handle, code: i32, expected: i32, context: &str) -> Result<()> {
        if code == expected {
            Ok(())
        } else {
            Err(format!(
                "{context}: rc={code}, expected={expected}, errmsg={}",
                self.error(db)
            ))
        }
    }

    fn open(&self, path: &str) -> Result<Db<'_>> {
        let path = CString::new(path).map_err(|error| error.to_string())?;
        let mut handle = ptr::null_mut();
        let code = unsafe { (self.open)(path.as_ptr(), &mut handle) };
        if code != 0 || handle.is_null() {
            let message = if handle.is_null() {
                "null handle".into()
            } else {
                self.error(handle)
            };
            if !handle.is_null() {
                unsafe {
                    (self.close)(handle);
                }
            }
            return Err(format!("sqlite3_open: rc={code}: {message}"));
        }
        Ok(Db { api: self, handle })
    }
}

struct Db<'a> {
    api: &'a Api,
    handle: Handle,
}
impl Drop for Db<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.handle);
        }
    }
}
struct Stmt<'a> {
    db: &'a Db<'a>,
    handle: Handle,
}
impl Drop for Stmt<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.db.api.finalize)(self.handle);
        }
    }
}
impl Db<'_> {
    fn prepare(&self, sql: &str) -> Result<Stmt<'_>> {
        let sql = CString::new(sql).map_err(|error| error.to_string())?;
        let mut handle = ptr::null_mut();
        let code = unsafe {
            (self.api.prepare)(self.handle, sql.as_ptr(), -1, &mut handle, ptr::null_mut())
        };
        self.api.check(
            self.handle,
            code,
            0,
            &format!("prepare {}", sql.to_string_lossy()),
        )?;
        if handle.is_null() {
            return Err("prepare returned null statement".into());
        }
        Ok(Stmt { db: self, handle })
    }
    fn exec(&self, sql: &str) -> Result<()> {
        let stmt = self.prepare(sql)?;
        self.api.check(
            self.handle,
            unsafe { (self.api.step)(stmt.handle) },
            DONE,
            sql,
        )
    }
    fn exec_abi(&self, sql: &str) -> Result<()> {
        let sql = CString::new(sql).map_err(|error| error.to_string())?;
        let mut message = ptr::null_mut();
        let code = unsafe {
            (self.api.exec)(
                self.handle,
                sql.as_ptr(),
                None,
                ptr::null_mut(),
                &mut message,
            )
        };
        let detail = if message.is_null() {
            self.api.error(self.handle)
        } else {
            let detail = unsafe { CStr::from_ptr(message).to_string_lossy().into_owned() };
            unsafe { (self.api.free)(message.cast()) };
            detail
        };
        require(code == 0, &format!("sqlite3_exec rc={code}: {detail}"))
    }
    fn scalar(&self, sql: &str) -> Result<i64> {
        let stmt = self.prepare(sql)?;
        self.api.check(
            self.handle,
            unsafe { (self.api.step)(stmt.handle) },
            ROW,
            sql,
        )?;
        Ok(unsafe { (self.api.column_int)(stmt.handle, 0) })
    }
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| format!("missing explicit fixture config: {name}"))
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--shim-env") {
        require(
            args.len() == 2 || (args.len() == 3 && args[2] == "--reconnect"),
            "usage: runtime_e2e --shim-env [--reconnect]",
        )?;
        args[1] = "--shim".into();
        args.insert(2, env("RUNTIME_E2E_SHIM")?);
    }
    require(
        args.len() == 3 || (args.len() == 4 && args[3] == "--reconnect"),
        "usage: runtime_e2e --shim ABSOLUTE_PATH [--reconnect]",
    )?;
    require(args[1] == "--shim", "--shim required")?;
    require(
        PathBuf::from(&args[2]).is_absolute(),
        "--shim requires an absolute artifact path",
    )?;
    require(
        !std::env::vars_os().any(|(name, _)| name.to_string_lossy().starts_with("PG")),
        "inherited libpq PG* settings forbidden; use the fixture script",
    )?;
    require(
        env("RUNTIME_E2E_ISOLATED")? == "1",
        "isolated fixture attestation required",
    )?;
    let host = env("PLEX_PG_HOST")?;
    require(
        host == "127.0.0.1" || host.starts_with("/"),
        "fixture must use loopback or a local Unix socket",
    )?;
    let database = env("PLEX_PG_DATABASE")?;
    let user = env("PLEX_PG_USER")?;
    let schema = env("PLEX_PG_SCHEMA")?;
    require(
        database.starts_with("runtime_e2e_")
            && user.starts_with("runtime_e2e_")
            && schema == "runtime_e2e",
        "refusing non-fixture database/user/schema",
    )?;
    require(
        !database
            .contains(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            && !user
                .contains(|character: char| !character.is_ascii_alphanumeric() && character != '_'),
        "invalid fixture identifiers",
    )?;
    let password = env("PLEX_PG_PASSWORD")?;
    require(
        !password.is_empty()
            && password
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "fixture password must be explicit and conninfo-safe",
    )?;
    let port: u16 = env("PLEX_PG_PORT")?
        .parse()
        .map_err(|_| "invalid fixture port")?;
    let root = PathBuf::from(env("RUNTIME_E2E_DIR")?)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    require(
        root.join(".runtime-e2e-fixture").is_file(),
        "missing temporary fixture directory marker",
    )?;
    let mut config = Config::new();
    config
        .host(&host)
        .port(port)
        .dbname(&database)
        .user(&user)
        .password(&password)
        .connect_timeout(std::time::Duration::from_secs(5));
    let mut observer = config.connect(NoTls).map_err(|error| error.to_string())?;
    let identity = observer.query_one("SELECT current_database(), current_user, shobj_description(oid, 'pg_database') FROM pg_database WHERE datname=current_database()", &[]).map_err(|error| error.to_string())?;
    require(
        identity.get::<_, String>(0) == database
            && identity.get::<_, String>(1) == user
            && identity.get::<_, Option<String>>(2).as_deref()
                == Some("runtime-e2e disposable fixture"),
        "database fixture marker or identity mismatch",
    )?;
    observer.batch_execute("CREATE SCHEMA runtime_e2e; SET search_path TO runtime_e2e; CREATE TABLE runtime_items (id BIGSERIAL PRIMARY KEY, unique_value BIGINT NOT NULL UNIQUE, nullable_value BIGINT, integer_value BIGINT, blob_value BYTEA, flag BOOLEAN); CREATE TABLE runtime_child (id BIGSERIAL PRIMARY KEY, parent_id BIGINT REFERENCES runtime_items(id));").map_err(|error| error.to_string())?;
    let api = unsafe { Api::load(&args[2])? };
    let support = root.join("support");
    std::fs::create_dir_all(support.join("Plex Media Server")).map_err(|e| e.to_string())?;
    std::fs::write(support.join("Plex Media Server/Preferences.xml"),
        "<Preferences AnonymousMachineIdentifier=\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" MachineIdentifier=\"53cfd87b-f8b2-4db2-af2d-6aaa373b2b34\" />").map_err(|e| e.to_string())?;
    std::env::set_var("PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR", &support);
    let path = root.join("com.plexapp.plugins.library.db");
    let path = path.to_str().ok_or("invalid fixture path")?;
    let first = api.open(path)?;
    let second = api.open(path)?;
    type Case = (
        &'static str,
        fn(&Api, &Db<'_>, &Db<'_>, &mut Client) -> Result<()>,
    );
    let cases: [Case; 20] = [
        (
            "native_uuid_binding_ownership",
            |api, first, _, observer| {
                observer.batch_execute("CREATE TABLE runtime_e2e.devices (id BIGINT PRIMARY KEY, identifier UUID, name TEXT); INSERT INTO runtime_e2e.devices VALUES (15192, '53cfd87b-f8b2-4db2-af2d-6aaa373b2b34', 'Owned fixture')").map_err(|e| e.to_string())?;
                let empty = CString::new("").unwrap();
                for wide in [false, true] {
                    for custom in [false, true] {
                        let stmt = first.prepare("SELECT id FROM devices WHERE identifier=?")?;
                        for _ in 0..4 {
                            DESTRUCTOR_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
                            let pointer = if custom {
                                let ptr = unsafe { libc::malloc(1) }.cast::<c_char>();
                                require(!ptr.is_null(), "callback fixture allocation failed")?;
                                unsafe {
                                    *ptr = 0;
                                }
                                DESTRUCTOR_POINTER
                                    .store(ptr as usize, std::sync::atomic::Ordering::SeqCst);
                                ptr.cast_const()
                            } else {
                                empty.as_ptr()
                            };
                            let destructor = custom.then_some(
                                owned_text_destructor as unsafe extern "C" fn(*mut c_void),
                            );
                            let rc = unsafe {
                                if wide {
                                    (api.bind_text64)(stmt.handle, 1, pointer, 0, destructor, 1)
                                } else {
                                    (api.bind_text)(stmt.handle, 1, pointer, 0, destructor)
                                }
                            };
                            api.check(first.handle, rc, 0, "bind replacement UUID")?;
                            api.check(
                                first.handle,
                                unsafe { (api.step)(stmt.handle) },
                                ROW,
                                "step UUID replacement",
                            )?;
                            require(
                                unsafe { (api.column_int)(stmt.handle, 0) } == 15192,
                                "UUID replacement did not preserve server identity",
                            )?;
                            api.check(
                                first.handle,
                                unsafe { (api.step)(stmt.handle) },
                                DONE,
                                "exhaust UUID query",
                            )?;
                            api.check(
                                first.handle,
                                unsafe { (api.reset)(stmt.handle) },
                                0,
                                "reset UUID",
                            )?;
                            api.check(
                                first.handle,
                                unsafe { (api.clear)(stmt.handle) },
                                0,
                                "clear UUID",
                            )?;
                            if custom {
                                require(
                                    DESTRUCTOR_CALLS.load(std::sync::atomic::Ordering::SeqCst) == 1,
                                    "original UUID destructor did not run once",
                                )?;
                            }
                        }
                    }
                }
                Ok(())
            },
        ),
        (
            "plugin_join_metadata_lifetimes",
            |api, first, _, observer| {
                observer.batch_execute("CREATE TABLE runtime_e2e.plugins (id INTEGER PRIMARY KEY, identifier TEXT, framework_version INTEGER, access_count INTEGER, installed_at BIGINT, accessed_at BIGINT, modified_at BIGINT); CREATE TABLE runtime_e2e.plugin_prefixes (id INTEGER PRIMARY KEY, plugin_id INTEGER, name TEXT, prefix TEXT, art_url TEXT, thumb_url TEXT, titlebar_url TEXT, share INTEGER, has_store_services INTEGER, prefs INTEGER); INSERT INTO runtime_e2e.plugins VALUES (1,'plugin-with-prefix',2,3,1790000000,1790000001,1790000002),(2,'plugin-with-null-prefix',2,4,1790000003,1790000004,1790000005); INSERT INTO runtime_e2e.plugin_prefixes VALUES (11,1,'Fixture','/fixture',NULL,'thumb',NULL,1,0,1)").map_err(|e| e.to_string())?;
                let sql = "SELECT plugins.id AS plugins_id, plugins.identifier AS plugins_identifier, plugins.framework_version AS plugins_framework_version, plugins.access_count AS plugins_access_count, plugins.installed_at AS plugins_installed_at, plugins.accessed_at AS plugins_accessed_at, plugins.modified_at AS plugins_modified_at, plugin_prefixes.id AS plugin_prefixes_id, plugin_prefixes.name AS plugin_prefixes_name, plugin_prefixes.plugin_id AS plugin_prefixes_plugin_id, plugin_prefixes.prefix AS plugin_prefixes_prefix, plugin_prefixes.art_url AS plugin_prefixes_art_url, plugin_prefixes.thumb_url AS plugin_prefixes_thumb_url, plugin_prefixes.titlebar_url AS plugin_prefixes_titlebar_url, plugin_prefixes.share AS plugin_prefixes_share, plugin_prefixes.has_store_services AS plugin_prefixes_has_store_services, plugin_prefixes.prefs AS plugin_prefixes_prefs FROM plugins LEFT JOIN plugin_prefixes ON plugin_prefixes.plugin_id=plugins.id ORDER BY plugins.id";
                for _ in 0..20 {
                    let stmt = first.prepare(sql)?;
                    require(
                        unsafe { (api.column_count)(stmt.handle) } == 17,
                        "plugin metadata before step has wrong column count",
                    )?;
                    let name = unsafe { (api.column_name)(stmt.handle, 16) };
                    require(
                        !name.is_null()
                            && unsafe { CStr::from_ptr(name) }.to_bytes()
                                == b"plugin_prefixes_prefs",
                        "plugin metadata name mismatch",
                    )?;
                    let _ = unsafe { (api.column_decltype)(stmt.handle, 16) };
                    for _ in 0..3 {
                        api.check(
                            first.handle,
                            unsafe { (api.step)(stmt.handle) },
                            ROW,
                            "plugin joined row",
                        )?;
                        require(
                            unsafe { (api.column_type)(stmt.handle, 16) } == 1
                                && unsafe { (api.column_int)(stmt.handle, 16) } == 1,
                            "plugin boolean prefs ABI mismatch",
                        )?;
                        require(
                            unsafe { (api.column_int)(stmt.handle, 4) } == 1790000000,
                            "plugin datetime/int64 ABI mismatch",
                        )?;
                        require(
                            unsafe { (api.column_type)(stmt.handle, 11) } == 5,
                            "plugin NULL artwork ABI mismatch",
                        )?;
                        let text = unsafe { (api.column_text)(stmt.handle, 1) };
                        require(
                            !text.is_null()
                                && unsafe { CStr::from_ptr(text.cast()) }.to_bytes()
                                    == b"plugin-with-prefix",
                            "plugin identifier lifetime mismatch",
                        )?;
                        api.check(
                            first.handle,
                            unsafe { (api.step)(stmt.handle) },
                            ROW,
                            "plugin NULL-prefix row",
                        )?;
                        for column in 7..17 {
                            require(
                                unsafe { (api.column_type)(stmt.handle, column) } == 5,
                                "unmatched plugin prefix did not remain SQL NULL",
                            )?;
                        }
                        api.check(
                            first.handle,
                            unsafe { (api.step)(stmt.handle) },
                            DONE,
                            "exhaust plugin join",
                        )?;
                        api.check(
                            first.handle,
                            unsafe { (api.reset)(stmt.handle) },
                            0,
                            "reset plugin join",
                        )?;
                    }
                }
                Ok(())
            },
        ),
        (
            "sqlite_natural_constraint_parity",
            |_, first, _, observer| {
                observer.batch_execute("CREATE TABLE runtime_e2e.metadata_item_settings (id BIGSERIAL PRIMARY KEY, account_id INTEGER, guid TEXT, view_count INTEGER); CREATE TABLE runtime_e2e.statistics_bandwidth (id BIGSERIAL PRIMARY KEY, account_id INTEGER, device_id INTEGER, timespan INTEGER, at BIGINT, lan INTEGER)").map_err(|e| e.to_string())?;
                first.exec("INSERT INTO metadata_item_settings (account_id,guid,view_count) VALUES (7,'same',1)")?;
                first.exec("INSERT INTO metadata_item_settings (account_id,guid,view_count) VALUES (7,'same',2)")?;
                require(first.scalar("SELECT count(*) FROM metadata_item_settings WHERE account_id=7 AND guid='same'")? == 2, "plain INSERT silently replaced source-valid rows")?;
                first.exec("INSERT INTO statistics_bandwidth (account_id,device_id,timespan,at,lan) VALUES (7,8,9,10,1)")?;
                first.exec("INSERT INTO statistics_bandwidth (account_id,device_id,timespan,at,lan) VALUES (7,8,9,10,1)")?;
                require(
                    first.scalar("SELECT count(*) FROM statistics_bandwidth")? == 2,
                    "statistics INSERT incorrectly became UPSERT",
                )?;
                require(first.exec("INSERT INTO metadata_item_settings (id,account_id,guid) VALUES (1,8,'other')").is_err(), "genuine duplicate PK did not raise constraint error")?;
                Ok(())
            },
        ),
        ("native_rowid_edge_cases", |api, first, _, _| {
            let db = api.open(":memory:")?;
            require(
                unsafe { (api.rowid)(db.handle) } == 0,
                "unmanaged handle leaked another rowid",
            )?;
            db.exec("CREATE TABLE unmanaged_items (id INTEGER PRIMARY KEY, value TEXT UNIQUE)")?;
            db.exec("INSERT INTO unmanaged_items VALUES (88,'first')")?;
            require(
                unsafe { (api.rowid)(db.handle) } == 88,
                "original SQLite rowid fallback missing",
            )?;
            require(
                db.exec("INSERT INTO unmanaged_items VALUES (89,'first')")
                    .is_err(),
                "SQLite UNIQUE failure unexpectedly succeeded",
            )?;
            require(
                unsafe { (api.rowid)(db.handle) } == 88,
                "failed unmanaged write changed rowid",
            )?;
            let previous = unsafe { (api.rowid)(first.handle) };
            first.exec("INSERT OR IGNORE INTO metadata_item_settings (id,account_id,guid) VALUES (1,8,'ignored')")?;
            require(
                unsafe { (api.rowid)(first.handle) } == previous,
                "ignored managed insert changed rowid",
            )?;
            Ok(())
        }),
        (
            "replace_guard_transactions_and_concurrency",
            |api, first, _, observer| {
                observer.batch_execute("ALTER TABLE runtime_e2e.metadata_item_settings ADD CONSTRAINT fixture_nonnegative_views CHECK (view_count>=0)").map_err(|e| e.to_string())?;
                first.exec("BEGIN")?;
                first.exec("INSERT INTO metadata_item_settings (id,account_id,guid,view_count) VALUES (100,9,'earlier-caller-work',1)")?;
                first.exec("INSERT OR REPLACE INTO metadata_item_settings (id,account_id,guid,view_count) VALUES (1,7,'replacement',8)")?;
                require(
                    unsafe { (api.rowid)(first.handle) } == 1,
                    "replacement rowid not published after savepoint release",
                )?;
                require(first.exec("INSERT OR REPLACE INTO metadata_item_settings (id,account_id,guid,view_count) VALUES (1,7,'invalid',-1)").is_err(),"replacement ignored genuine CHECK failure")?;
                require(
                    first.scalar("SELECT view_count FROM metadata_item_settings WHERE id=1")? == 8,
                    "failed replacement erased prior row",
                )?;
                require(
                    first.scalar("SELECT count(*) FROM metadata_item_settings WHERE id=100")? == 1,
                    "replacement failure discarded earlier caller work",
                )?;
                first.exec("COMMIT")?;
                first.exec_abi("REPLACE INTO metadata_item_settings (id,account_id,guid,view_count) VALUES (1000,9,'direct-replace',7)")?;
                require(
                    unsafe { (api.rowid)(first.handle) } == 1000,
                    "direct replacement rowid incorrect",
                )?;
                for id in [0, -1] {
                    first.exec(&format!("INSERT OR REPLACE INTO metadata_item_settings (id,account_id,guid,view_count) VALUES ({id},9,'signed-regular',7)"))?;
                    require(
                        unsafe { (api.rowid)(first.handle) } == id,
                        "regular replacement did not publish signed rowid",
                    )?;
                }
                let signed=first.prepare("INSERT OR REPLACE INTO metadata_item_settings (id,account_id,guid,view_count) VALUES (?,9,'signed-cached',7)")?;
                for id in [88, 0, -1] {
                    api.check(
                        first.handle,
                        unsafe { (api.bind_int)(signed.handle, 1, id) },
                        0,
                        "bind signed cached rowid",
                    )?;
                    api.check(
                        first.handle,
                        unsafe { (api.step)(signed.handle) },
                        DONE,
                        "step signed cached rowid",
                    )?;
                    require(
                        unsafe { (api.rowid)(first.handle) } == id,
                        "cached replacement did not publish signed rowid",
                    )?;
                    api.check(
                        first.handle,
                        unsafe { (api.reset)(signed.handle) },
                        0,
                        "reset signed replacement",
                    )?;
                }
                for id in [88, 0, -1] {
                    first.exec_abi(&format!("REPLACE INTO metadata_item_settings (id,account_id,guid,view_count) VALUES ({id},9,'signed-direct',7)"))?;
                    require(
                        unsafe { (api.rowid)(first.handle) } == id,
                        "direct replacement did not publish signed rowid",
                    )?;
                }
                let path =
                    PathBuf::from(env("RUNTIME_E2E_DIR")?).join("com.plexapp.plugins.library.db");
                std::thread::scope(|scope| -> Result<()> {
                    let handles: Vec<_> = (0..4).map(|client| {
                    let path = &path;
                    scope.spawn(move || -> Result<()> {
                        let db=api.open(path.to_str().ok_or("invalid concurrent fixture path")?)?;
                        let stmt=db.prepare("INSERT OR REPLACE INTO metadata_item_settings (id,account_id,guid,view_count) VALUES (1,7,?,?)")?;
                        for iteration in 0..10 {
                            let guid=CString::new(format!("client-{client}")).unwrap();
                            api.check(db.handle,unsafe { (api.bind_text)(stmt.handle,1,guid.as_ptr(),guid.as_bytes().len() as i32,None) },0,"bind cached replacement")?;
                            api.check(db.handle,unsafe { (api.bind_int)(stmt.handle,2,iteration) },0,"bind cached replacement count")?;
                            api.check(db.handle,unsafe { (api.step)(stmt.handle) },DONE,"concurrent cached replacement")?;
                            require(unsafe { (api.rowid)(db.handle) }==1,"concurrent replacement leaked rowid")?;
                            api.check(db.handle,unsafe { (api.reset)(stmt.handle) },0,"reset concurrent replacement")?;
                        }
                        Ok(())
                    })
                }).collect();
                    for handle in handles {
                        handle
                            .join()
                            .map_err(|_| "concurrent replacement thread panicked")??;
                    }
                    Ok(())
                })?;
                require(
                    first.scalar("SELECT count(*) FROM metadata_item_settings WHERE id=1")? == 1,
                    "concurrent replacement lost or duplicated row",
                )?;
                let before = first.scalar("SELECT count(*) FROM metadata_item_settings")?;
                require(first.exec("INSERT OR REPLACE INTO metadata_item_settings (id,guid) VALUES (2000,'a'),(2001,'b')").is_err(),"unsupported replacement silently wrote shadow SQLite")?;
                require(
                    first.scalar("SELECT count(*) FROM metadata_item_settings")? == before,
                    "rejected replacement mutated PostgreSQL",
                )?;
                Ok(())
            },
        ),
        ("fts_rebuild_preserves_metadata", |_, first, _, observer| {
            first.exec("CREATE VIRTUAL TABLE temp.tokenizer USING fts4(title)")?;
            first.exec("DROP TABLE temp.tokenizer")?;
            // spellfix1 is unavailable in the shadow SQLite. Skipped index
            // maintenance must prepare successfully without a real table.
            first.exec("DELETE FROM spellfix_metadata_titles")?;
            first.exec("INSERT INTO spellfix_metadata_titles (word) VALUES ('Keep')")?;
            observer.batch_execute("CREATE TABLE runtime_e2e.metadata_items (id BIGINT PRIMARY KEY, title TEXT, title_sort TEXT, original_title TEXT); INSERT INTO runtime_e2e.metadata_items VALUES (456, 'Keep me', 'Keep me', 'Original'); CREATE VIEW runtime_e2e.fts4_metadata_titles_icu AS SELECT id AS rowid, title, to_tsvector('simple',coalesce(title,'') || ' ' || coalesce(title_sort,'') || ' ' || coalesce(original_title,'')) AS title_fts, title_sort, original_title FROM runtime_e2e.metadata_items").map_err(|error| error.to_string())?;
            first.exec("BEGIN")?;
            first.exec("DELETE FROM fts4_metadata_titles_icu")?;
            first.exec("INSERT INTO fts4_metadata_titles_icu (rowid, title, title_sort, original_title) SELECT id, title, title_sort, original_title FROM metadata_items")?;
            first.exec("COMMIT")?;
            first.exec_abi("DELETE FROM fts4_metadata_titles_icu")?;
            first.exec_abi("INSERT INTO fts4_metadata_titles_icu (rowid, title, title_sort, original_title) SELECT id, title, title_sort, original_title FROM metadata_items")?;
            require(
                first.scalar("SELECT rowid FROM fts4_metadata_titles_icu")? == 456,
                "FTS rebuild deleted or duplicated source metadata",
            )?;
            require(
                first.scalar(
                    "SELECT rowid FROM fts4_metadata_titles_icu WHERE title MATCH 'Keep'",
                )? == 456,
                "FTS search must still read PostgreSQL metadata after rebuild",
            )?;
            first.exec("UPDATE metadata_items SET title='Updated' WHERE id=456")?;
            require(
                observer
                    .query_one(
                        "SELECT title FROM runtime_e2e.metadata_items WHERE id=456",
                        &[],
                    )
                    .map_err(|error| error.to_string())?
                    .get::<_, String>(0)
                    == "Updated",
                "normal metadata writes must still reach PostgreSQL",
            )
        }),
        ("prepared_fts_search", |api, first, _, _| {
            for sql in [
                "SELECT rowid FROM fts4_metadata_titles_icu WHERE title_sort MATCH ?",
                "SELECT rowid FROM fts4_metadata_titles_icu WHERE title_sort MATCH ? AND rowid=456",
                "SELECT rowid FROM fts4_metadata_titles_icu WHERE fts4_metadata_titles_icu MATCH ?",
            ] {
                let stmt = first.prepare(sql)?;
                let query = CString::new("Keep*").unwrap();
                api.check(
                    first.handle,
                    unsafe { (api.bind_text)(stmt.handle, 1, query.as_ptr(), 5, None) },
                    0,
                    "bind FTS prefix",
                )?;
                api.check(
                    first.handle,
                    unsafe { (api.step)(stmt.handle) },
                    ROW,
                    "step prepared FTS",
                )?;
                require(
                    unsafe { (api.column_int)(stmt.handle, 0) } == 456,
                    "prepared FTS prefix lost source match",
                )?;
            }
            require(
                first.scalar("SELECT rowid FROM fts4_metadata_titles_icu WHERE title_sort MATCH 'Keep*' AND rowid=456")? == 456,
                "literal FTS prefix swallowed trailing filter",
            )
        }),
        ("sqlite_fts_internals", |_, first, _, observer| {
            first.exec("CREATE VIRTUAL TABLE fts4_metadata_titles USING fts4(title)")?;
            require(
                first.scalar("SELECT COUNT(*) FROM 'main'.'fts4_metadata_titles_content'")? == 0,
                "unexpected SQLite FTS content rows",
            )?;
            require(
                observer
                    .query_one(
                        "SELECT to_regclass('runtime_e2e.fts4_metadata_titles_content') IS NULL",
                        &[],
                    )
                    .map_err(|error| error.to_string())?
                    .get::<_, bool>(0),
                "SQLite FTS backing table leaked to PostgreSQL",
            )?;
            observer.batch_execute("CREATE TABLE runtime_e2e.fts4_metadata_titles (id BIGINT); INSERT INTO runtime_e2e.fts4_metadata_titles VALUES (123)").map_err(|error| error.to_string())?;
            require(
                first.scalar("SELECT id FROM fts4_metadata_titles")? == 123,
                "logical library FTS query incorrectly bypassed PostgreSQL",
            )
        }),
        ("sqlite_rtree_internals", |_, first, _, observer| {
            first.exec("CREATE VIRTUAL TABLE locations USING rtree(id, lat_min, lat_max, lon_min, lon_max)")?;
            require(
                first.scalar(
                    "SELECT length(data) FROM \"main\".\"locations_node\" WHERE nodeno=1",
                )? > 0,
                "SQLite RTree root node missing",
            )?;
            require(
                first.scalar("SELECT COUNT(*) FROM locations_parent")? == 0,
                "unexpected RTree parent rows",
            )?;
            require(
                first.scalar("SELECT COUNT(*) FROM locations_rowid")? == 0,
                "unexpected RTree rowid rows",
            )?;
            require(
                observer
                    .query_one(
                        "SELECT to_regclass('runtime_e2e.locations_node') IS NULL",
                        &[],
                    )
                    .map_err(|error| error.to_string())?
                    .get::<_, bool>(0),
                "SQLite RTree internals leaked to PostgreSQL",
            )
        }),
        ("normal_library_open", |_, first, _, observer| {
            require(
                first.scalar("SELECT COUNT(*) FROM runtime_items")? == 0,
                "shim did not see PostgreSQL-only fixture table",
            )?;
            require(
                observer
                    .query_one("SELECT COUNT(*) FROM runtime_e2e.runtime_items", &[])
                    .map_err(|error| error.to_string())?
                    .get::<_, i64>(0)
                    == 0,
                "fixture unexpectedly populated",
            )
        }),
        ("prepared_cached_writes", |api, first, _, observer| {
            for _ in 0..2 {
                let stmt = first.prepare("INSERT INTO runtime_items (unique_value) VALUES (?)")?;
                for _ in 0..3 {
                    let key = observer
                        .query_one(
                            "SELECT COALESCE(MAX(unique_value),0)+1 FROM runtime_e2e.runtime_items",
                            &[],
                        )
                        .map_err(|error| error.to_string())?
                        .get::<_, i64>(0);
                    api.check(
                        first.handle,
                        unsafe { (api.bind_int)(stmt.handle, 1, key) },
                        0,
                        "bind cached write",
                    )?;
                    api.check(
                        first.handle,
                        unsafe { (api.step)(stmt.handle) },
                        DONE,
                        "step cached write",
                    )?;
                    require(
                        unsafe { (api.rowid)(first.handle) } > 0,
                        "missing inserted rowid",
                    )?;
                    api.check(
                        first.handle,
                        unsafe { (api.reset)(stmt.handle) },
                        0,
                        "reset cached write",
                    )?;
                    api.check(
                        first.handle,
                        unsafe { (api.clear)(stmt.handle) },
                        0,
                        "clear cached write",
                    )?;
                }
            }
            require(
                observer
                    .query_one("SELECT COUNT(*) FROM runtime_e2e.runtime_items", &[])
                    .map_err(|error| error.to_string())?
                    .get::<_, i64>(0)
                    == 6,
                "prepared writes not persisted to PostgreSQL",
            )
        }),
        ("real_constraints_and_errors", |api, first, _, _| {
            let mut errors = Vec::new();
            for (sql, expected) in [
                ("INSERT INTO runtime_items (unique_value) VALUES (1)", 2067),
                (
                    "INSERT INTO runtime_items (unique_value) VALUES (NULL)",
                    1299,
                ),
                ("INSERT INTO runtime_child (parent_id) VALUES (999999)", 787),
            ] {
                let stmt = first.prepare(sql)?;
                let code = unsafe { (api.step)(stmt.handle) };
                let primary = unsafe { (api.errcode)(first.handle) };
                let extended = unsafe { (api.extended)(first.handle) };
                let message = api.error(first.handle);
                if code & 255 != 19
                    || primary != 19
                    || extended != expected
                    || message.is_empty()
                    || message == "not an error"
                {
                    errors.push(format!("{sql}: rc={code}, primary={primary}, extended={extended} (expected {expected}), errmsg={message}"));
                }
            }
            require(
                first.scalar("SELECT COUNT(*) FROM runtime_items")? == 6,
                "connection unusable after errors",
            )?;
            require(errors.is_empty(), &errors.join("; "))
        }),
        ("two_handle_rowid_isolation", |api, first, second, _| {
            first.exec("INSERT INTO runtime_items (unique_value) VALUES (101)")?;
            let first_id = unsafe { (api.rowid)(first.handle) };
            second.exec("INSERT INTO runtime_items (unique_value) VALUES (102)")?;
            let second_id = unsafe { (api.rowid)(second.handle) };
            require(
                second_id != first_id && first_id > 0 && second_id > 0,
                "insert identities not distinct",
            )?;
            require(
                unsafe { (api.rowid)(first.handle) } == first_id,
                "second handle overwrote first handle rowid",
            )?;
            require(
                first.scalar("SELECT last_insert_rowid()")? == first_id
                    && second.scalar("SELECT last_insert_rowid()")? == second_id,
                "SQL rowid isolation failed",
            )
        }),
        (
            "transaction_multiple_writes_rollback",
            |_, first, second, observer| {
                first.exec("BEGIN")?;
                first.exec("INSERT INTO runtime_items (unique_value) VALUES (201)")?;
                first.exec("INSERT INTO runtime_items (unique_value) VALUES (202)")?;
                require(
                    second.scalar(
                        "SELECT COUNT(*) FROM runtime_items WHERE unique_value IN (201,202)",
                    )? == 0,
                    "uncommitted writes leaked to another handle",
                )?;
                first.exec("ROLLBACK")?;
                require(observer.query_one("SELECT COUNT(*) FROM runtime_e2e.runtime_items WHERE unique_value IN (201,202)", &[]).map_err(|error| error.to_string())?.get::<_, i64>(0) == 0, "rollback writes persisted")
            },
        ),
        ("savepoint", |_, first, _, observer| {
            first.exec("BEGIN")?;
            first.exec("INSERT INTO runtime_items (unique_value) VALUES (301)")?;
            first.exec("SAVEPOINT runtime_point")?;
            first.exec("INSERT INTO runtime_items (unique_value) VALUES (302)")?;
            first.exec("ROLLBACK TO runtime_point")?;
            first.exec("RELEASE runtime_point")?;
            first.exec("COMMIT")?;
            let count = observer
                .query_one(
                    "SELECT COUNT(*) FROM runtime_e2e.runtime_items WHERE unique_value=302",
                    &[],
                )
                .map_err(|error| error.to_string())?
                .get::<_, i64>(0);
            require(
                count == 0
                    && first.scalar("SELECT COUNT(*) FROM runtime_items WHERE unique_value=301")?
                        == 1,
                "rollback/savepoint persistence incorrect",
            )
        }),
        ("aborted_commit_rejected", |_, first, _, _| {
            first.exec("BEGIN")?;
            require(
                first
                    .exec("INSERT INTO runtime_items (unique_value) VALUES (1)")
                    .is_err(),
                "duplicate write unexpectedly succeeded",
            )?;
            require(
                first.exec("COMMIT").is_err(),
                "aborted transaction reported successful commit",
            )?;
            first.exec("ROLLBACK")
        }),
        ("null_int64_blob_boolean", |api, first, _, observer| {
            let blob = [0_u8, 1, 127, 128, 255];
            let number = 8_000_000_000_000_123_i64;
            let stmt = first.prepare("INSERT INTO runtime_items (unique_value, nullable_value, integer_value, blob_value, flag) VALUES (401, ?, ?, ?, ?)")?;
            api.check(
                first.handle,
                unsafe { (api.bind_null)(stmt.handle, 1) },
                0,
                "bind NULL",
            )?;
            api.check(
                first.handle,
                unsafe { (api.bind_int)(stmt.handle, 2, number) },
                0,
                "bind int64",
            )?;
            api.check(
                first.handle,
                unsafe {
                    (api.bind_blob)(
                        stmt.handle,
                        3,
                        blob.as_ptr().cast(),
                        blob.len() as i32,
                        None,
                    )
                },
                0,
                "bind BLOB",
            )?;
            api.check(
                first.handle,
                unsafe { (api.bind_int)(stmt.handle, 4, 1) },
                0,
                "bind boolean",
            )?;
            api.check(
                first.handle,
                unsafe { (api.step)(stmt.handle) },
                DONE,
                "type write",
            )?;
            let read = first.prepare("SELECT nullable_value, integer_value, blob_value, flag FROM runtime_items WHERE unique_value=401")?;
            api.check(
                first.handle,
                unsafe { (api.step)(read.handle) },
                ROW,
                "type read",
            )?;
            let mut errors = Vec::new();
            if unsafe { (api.column_type)(read.handle, 0) } != 5 {
                errors.push("NULL storage class incorrect");
            }
            if unsafe { (api.column_type)(read.handle, 1) } != 1
                || unsafe { (api.column_int)(read.handle, 1) } != number
            {
                errors.push("int64 roundtrip incorrect");
            }
            if unsafe { (api.column_type)(read.handle, 2) } != 4 {
                errors.push("BLOB storage class incorrect");
            }
            let length = unsafe { (api.column_bytes)(read.handle, 2) };
            let data = unsafe { (api.column_blob)(read.handle, 2) };
            if length != blob.len() as i32 || data.is_null() {
                errors.push("BLOB length/pointer incorrect");
            } else if unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length as usize) }
                != blob
            {
                errors.push("BLOB bytes changed");
            }
            if unsafe { (api.column_int)(read.handle, 3) } != 1 {
                errors.push("boolean roundtrip incorrect");
            }
            drop(read);
            let numeric = first.prepare("SELECT -123456789 AS integer_value, 9223372036854775700 AS big_value, 12345.625 AS real_value")?;
            api.check(
                first.handle,
                unsafe { (api.step)(numeric.handle) },
                ROW,
                "numeric scalar read",
            )?;
            for _ in 0..8 {
                if unsafe { (api.column_int32)(numeric.handle, 0) } != -123456789
                    || unsafe { (api.column_int)(numeric.handle, 1) } != 9223372036854775700
                    || unsafe { (api.column_double)(numeric.handle, 2) } != 12345.625
                {
                    errors.push("numeric scalar accessors changed values");
                    break;
                }
            }
            let persisted = observer.query_one("SELECT nullable_value, integer_value, blob_value, flag FROM runtime_e2e.runtime_items WHERE unique_value=401", &[]).map_err(|error| error.to_string())?;
            require(
                persisted.get::<_, Option<i64>>(0).is_none()
                    && persisted.get::<_, i64>(1) == number
                    && persisted.get::<_, Vec<u8>>(2) == blob
                    && persisted.get::<_, bool>(3),
                "PostgreSQL stored types differ",
            )?;
            require(errors.is_empty(), &errors.join("; "))
        }),
        ("select_result_exhaustion", |api, first, _, _| {
            let stmt = first.prepare("SELECT COUNT(*) FROM runtime_items")?;
            api.check(
                first.handle,
                unsafe { (api.step)(stmt.handle) },
                ROW,
                "first SELECT step",
            )?;
            api.check(
                first.handle,
                unsafe { (api.step)(stmt.handle) },
                DONE,
                "SELECT result exhaustion",
            )
        }),
        ("maintenance_noop", |_, first, _, observer| {
            let before: i64 = observer
                .query_one("SELECT COUNT(*) FROM runtime_e2e.runtime_items", &[])
                .map_err(|error| error.to_string())?
                .get(0);
            first.exec("VACUUM")?;
            first.exec_abi("VACUUM")?;
            first.exec("REINDEX")?;
            first.exec("PRAGMA optimize")?;
            let after: i64 = observer
                .query_one("SELECT COUNT(*) FROM runtime_e2e.runtime_items", &[])
                .map_err(|error| error.to_string())?
                .get(0);
            require(
                before == after,
                "maintenance changed PostgreSQL library data",
            )
        }),
        (
            "sqlite3_exec_transactions_and_errors",
            |api, first, _, observer| {
                first.exec_abi("BEGIN")?;
                first.exec_abi("INSERT INTO runtime_items (unique_value) VALUES (701)")?;
                first.exec_abi("ROLLBACK")?;
                require(
                    observer
                        .query_one(
                            "SELECT COUNT(*) FROM runtime_e2e.runtime_items WHERE unique_value=701",
                            &[],
                        )
                        .map_err(|error| error.to_string())?
                        .get::<_, i64>(0)
                        == 0,
                    "sqlite3_exec rollback persisted a write",
                )?;
                require(
                    first
                        .exec_abi("INSERT INTO runtime_items (unique_value) VALUES (1)")
                        .is_err(),
                    "sqlite3_exec hid a constraint failure",
                )?;
                require(
                    unsafe { (api.extended)(first.handle) } == 2067,
                    "sqlite3_exec lost extended constraint error",
                )?;
                first.exec_abi("INSERT INTO runtime_items (unique_value) VALUES (702)")?;
                require(
                    observer
                        .query_one(
                            "SELECT COUNT(*) FROM runtime_e2e.runtime_items WHERE unique_value=702",
                            &[],
                        )
                        .map_err(|error| error.to_string())?
                        .get::<_, i64>(0)
                        == 1,
                    "sqlite3_exec write did not persist",
                )
            },
        ),
    ];
    let mut failures = 0;
    for (name, case) in cases {
        match case(&api, &first, &second, &mut observer) {
            Ok(()) => println!("PASS {name}"),
            Err(error) => {
                failures += 1;
                eprintln!("FAIL {name}: {error}");
                let _ = first.exec("ROLLBACK");
                let _ = second.exec("ROLLBACK");
            }
        }
    }
    if args.len() == 4 {
        let reconnect = (|| -> Result<()> {
            let victims = observer.query("SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND usename=current_user AND pid<>pg_backend_pid()", &[]).map_err(|error| error.to_string())?;
            require(
                !victims.is_empty(),
                "no real shim PostgreSQL backends to reconnect",
            )?;
            for victim in victims {
                observer
                    .query_one(
                        "SELECT pg_terminate_backend($1)",
                        &[&victim.get::<_, i32>(0)],
                    )
                    .map_err(|error| error.to_string())?;
            }
            require(
                first.scalar("SELECT COUNT(*) FROM runtime_items")? > 0,
                "reconnect did not recover reads",
            )?;
            first.exec("INSERT INTO runtime_items (unique_value) VALUES (501)")?;
            require(
                observer
                    .query_one(
                        "SELECT COUNT(*) FROM runtime_e2e.runtime_items WHERE unique_value=501",
                        &[],
                    )
                    .map_err(|error| error.to_string())?
                    .get::<_, i64>(0)
                    == 1,
                "reconnect write not persisted",
            )?;
            observer.batch_execute("CREATE VIEW runtime_e2e.runtime_stream AS SELECT generate_series(1, 1000000)::bigint AS id").map_err(|error| error.to_string())?;
            let stream = first.prepare("SELECT id FROM runtime_stream")?;
            api.check(
                first.handle,
                unsafe { (api.step)(stream.handle) },
                ROW,
                "stream before disconnect",
            )?;
            let victims = observer.query("SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND usename=current_user AND pid<>pg_backend_pid() AND query LIKE '%runtime_stream%'", &[]).map_err(|error| error.to_string())?;
            require(
                !victims.is_empty(),
                "no real streaming backend to interrupt",
            )?;
            for victim in victims {
                observer
                    .query_one(
                        "SELECT pg_terminate_backend($1)",
                        &[&victim.get::<_, i32>(0)],
                    )
                    .map_err(|error| error.to_string())?;
            }
            let mut code = ROW;
            let mut delivered = 1;
            while code == ROW && delivered <= 1_000_000 {
                code = unsafe { (api.step)(stream.handle) };
                delivered += 1;
            }
            require(
                code != ROW && code != DONE,
                "interrupted stream reported successful exhaustion or replayed rows",
            )?;
            require(
                unsafe { (api.step)(stream.handle) } == code,
                "interrupted stream lost terminal error",
            )?;
            require(
                !api.error(first.handle).is_empty(),
                "interrupted stream lacks diagnostics",
            )?;
            drop(stream);
            first.exec("BEGIN")?;
            first.exec("INSERT INTO runtime_items (unique_value) VALUES (601)")?;
            let victims = observer.query("SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND usename=current_user AND pid<>pg_backend_pid()", &[]).map_err(|error| error.to_string())?;
            for victim in victims {
                observer
                    .query_one(
                        "SELECT pg_terminate_backend($1)",
                        &[&victim.get::<_, i32>(0)],
                    )
                    .map_err(|error| error.to_string())?;
            }
            require(
                first
                    .exec("INSERT INTO runtime_items (unique_value) VALUES (602)")
                    .is_err(),
                "interrupted transaction silently resumed in autocommit",
            )?;
            require(
                observer.query_one("SELECT COUNT(*) FROM runtime_e2e.runtime_items WHERE unique_value IN (601,602)", &[]).map_err(|error| error.to_string())?.get::<_, i64>(0) == 0,
                "interrupted transaction persisted partial writes",
            )
        })();
        match reconnect {
            Ok(()) => println!("PASS reconnect"),
            Err(error) => {
                failures += 1;
                eprintln!("FAIL reconnect: {error}");
            }
        }
    }
    require(
        failures == 0,
        &format!("{failures} runtime E2E case(s) failed"),
    )?;
    println!("PASS runtime_e2e: real shared shim + isolated PostgreSQL; zero skips");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("runtime_e2e: {error}");
        std::process::exit(1);
    }
}

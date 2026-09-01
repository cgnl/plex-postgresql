#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

#[allow(unused_imports)]
use crate::c_abi;
use crate::db_interpose_common;
use crate::db_interpose_common::stderr_ptr;
use crate::env_utils;
use crate::exception_what::pg_exception_install_terminate_logger;
#[allow(unused_imports)]
use crate::ffi_types::{sqlite3, sqlite3_stmt, sqlite3_value};
use crate::runtime_common::{handle_exception_with_tls, log_shim_unloading, shim_init_common};

type SigactionFn =
    unsafe extern "C" fn(c_int, *const libc::sigaction, *mut libc::sigaction) -> c_int;
type CxaThrowFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, Option<unsafe extern "C" fn(*mut c_void)>) -> !;
/// Pass-through hook for create_simple_converter (ASCII path handled at the
/// create_simple_codecvt level by the AArch64 asm hook below).
type CreateSimpleConverterFn = unsafe extern "C" fn(*mut u8) -> *mut c_void;

static mut ORIG_SIGACTION: Option<SigactionFn> = None;
static mut ORIG_CXA_THROW: Option<CxaThrowFn> = None;
static mut ORIG_CREATE_SIMPLE_CONVERTER: Option<CreateSimpleConverterFn> = None;

/// Function-pointer statics for the AArch64 global_asm hook below.
/// #[no_mangle] makes them addressable by their exact name from assembler.
#[no_mangle]
pub static mut SHIM_CREATE_UTF8_CODECVT_PTR: usize = 0;
#[no_mangle]
pub static mut SHIM_CREATE_SIMPLE_CODECVT_PTR: usize = 0;

static FORCE_IGNORE_SIGCHLD: AtomicI32 = AtomicI32::new(1);
static INTERCEPT_SIGACTION: AtomicI32 = AtomicI32::new(1);
static SIGNAL_LOG_ENABLED_CACHED: AtomicI32 = AtomicI32::new(-1);
static EXCEPTION_CATCHER_ENABLED_CACHED: AtomicI32 = AtomicI32::new(-1);

pub(crate) fn disable_postfork_signal_overrides_fast() {
    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
}

fn signal_log_enabled() -> bool {
    let cached = SIGNAL_LOG_ENABLED_CACHED.load(Ordering::Acquire);
    if cached != -1 {
        return cached != 0;
    }
    let enabled = env_utils::env_truthy(b"PLEX_PG_ENABLE_SIGNAL_LOG\0");
    SIGNAL_LOG_ENABLED_CACHED.store(if enabled { 1 } else { 0 }, Ordering::Release);
    enabled
}

fn exception_catcher_enabled() -> bool {
    let cached = EXCEPTION_CATCHER_ENABLED_CACHED.load(Ordering::Acquire);
    if cached != -1 {
        return cached != 0;
    }
    let enabled = env_utils::env_truthy(b"PLEX_PG_ENABLE_EXCEPTION_CATCHER\0");
    EXCEPTION_CATCHER_ENABLED_CACHED.store(if enabled { 1 } else { 0 }, Ordering::Release);
    enabled
}

/// Eagerly resolve all interposition hooks that live in this module.
/// Called once from `shim_init()` before any other thread can call the
/// interposed wrappers, eliminating the data race on lazy init.
#[allow(static_mut_refs)]
unsafe fn resolve_interposition_hooks() {
    // sigaction
    let sym = libc::dlsym(libc::RTLD_NEXT, b"sigaction\0".as_ptr() as *const c_char);
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(ORIG_SIGACTION),
            Some(std::mem::transmute::<*mut c_void, SigactionFn>(sym)),
        );
    }

    // __cxa_throw
    let sym = libc::dlsym(libc::RTLD_NEXT, b"__cxa_throw\0".as_ptr() as *const c_char);
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(ORIG_CXA_THROW),
            Some(std::mem::transmute::<*mut c_void, CxaThrowFn>(sym)),
        );
    }

    // create_simple_converter — pass-through hook (ASCII handled at codecvt level)
    let sym = libc::dlsym(
        libc::RTLD_NEXT,
        b"_ZN5boost6locale4util23create_simple_converterERKNSt3__212basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEE\0".as_ptr() as *const c_char
    );
    if !sym.is_null() {
        ptr::write(
            ptr::addr_of_mut!(ORIG_CREATE_SIMPLE_CONVERTER),
            Some(std::mem::transmute::<*mut c_void, CreateSimpleConverterFn>(sym)),
        );
    }

    // create_simple_codecvt — original target for the asm hook pass-through path.
    // Stored as a raw usize so the AArch64 assembler can read it directly.
    let sym = libc::dlsym(
        libc::RTLD_NEXT,
        b"_ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE\0".as_ptr() as *const c_char
    );
    if !sym.is_null() {
        ptr::write(ptr::addr_of_mut!(SHIM_CREATE_SIMPLE_CODECVT_PTR), sym as usize);
    }

    // create_utf8_codecvt — ASCII redirect target for the asm hook.
    let sym = libc::dlsym(
        libc::RTLD_NEXT,
        b"_ZN5boost6locale4util19create_utf8_codecvtERKNSt3__26localeENS0_12char_facet_tE\0".as_ptr() as *const c_char
    );
    if !sym.is_null() {
        ptr::write(ptr::addr_of_mut!(SHIM_CREATE_UTF8_CODECVT_PTR), sym as usize);
    }
}

#[allow(static_mut_refs)]
unsafe fn read_cxa_throw() -> Option<CxaThrowFn> {
    ptr::read(ptr::addr_of!(ORIG_CXA_THROW))
}

#[allow(static_mut_refs)]
unsafe fn read_sigaction() -> Option<SigactionFn> {
    ptr::read(ptr::addr_of!(ORIG_SIGACTION))
}

fn setup_exception_catcher_if_enabled() {
    if !exception_catcher_enabled() {
        return;
    }
    unsafe {
        if read_cxa_throw().is_some() {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Exception catcher enabled (__cxa_throw interposed)\n\0".as_ptr()
                    as *const c_char,
            );
            pg_exception_install_terminate_logger();
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Exception terminate logger requested (see [EXC_TERMINATE])\n\0"
                    .as_ptr() as *const c_char,
            );
        } else {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] WARNING: failed to resolve __cxa_throw\n\0".as_ptr() as *const c_char,
            );
        }
        let _ = libc::fflush(stderr_ptr());
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn patch_uuid_parser() {
    let mut base_addr: usize = 0;

    unsafe extern "C" fn phdr_callback(
        info: *mut libc::dl_phdr_info,
        _size: usize,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        let info = &*info;
        let name = if info.dlpi_name.is_null() {
            ""
        } else {
            let s = std::ffi::CStr::from_ptr(info.dlpi_name);
            s.to_str().unwrap_or("")
        };

        let counter = *(data as *mut usize);

        let _ = libc::fprintf(
            stderr_ptr(),
            b"[SHIM_INIT] [UUID_PATCH] Phdr entry[%zu]: name='%s', addr=0x%zx\n\0".as_ptr() as *const c_char,
            counter,
            CString::new(name).unwrap_or_default().as_ptr(),
            info.dlpi_addr as usize,
        );
        let _ = libc::fflush(stderr_ptr());

        if counter == 0 {
            *(data as *mut usize) = info.dlpi_addr as usize;
            return 1;
        }

        *(data as *mut usize) = counter + 1;
        0
    }

    let mut callback_data: usize = 0;
    libc::dl_iterate_phdr(Some(phdr_callback), &mut callback_data as *mut usize as *mut libc::c_void);
    base_addr = callback_data;

    if base_addr == 0 {
        let _ = libc::fprintf(
            stderr_ptr(),
            b"[SHIM_INIT] [UUID_PATCH] WARNING: Failed to find main executable base address\n\0".as_ptr() as *const c_char,
        );
        let _ = libc::fflush(stderr_ptr());
        return;
    }

    let target_addr = base_addr + 0x104a6cc;

    let _ = libc::fprintf(
        stderr_ptr(),
        b"[SHIM_INIT] [UUID_PATCH] Main executable base: 0x%zx, target_addr: 0x%zx\n\0".as_ptr() as *const c_char,
        base_addr,
        target_addr,
    );
    let _ = libc::fflush(stderr_ptr());

    if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
        let maps_c = CString::new(maps).unwrap_or_default();
        let _ = libc::fprintf(
            stderr_ptr(),
            b"[SHIM_INIT] [UUID_PATCH] Mappings:\n%s\n\0".as_ptr() as *const c_char,
            maps_c.as_ptr(),
        );
        let _ = libc::fflush(stderr_ptr());
    }

    let patch: [u8; 12] = [
        0x00, 0x00, 0x80, 0xd2,
        0x01, 0x00, 0x80, 0xd2,
        0xc0, 0x03, 0x5f, 0xd6,
    ];

    // Method 1: Try writing via /proc/self/mem (bypasses W^X and page protections)
    let mut written_via_proc_mem = false;
    use std::io::{Seek, SeekFrom, Write};
    if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open("/proc/self/mem") {
        if file.seek(SeekFrom::Start(target_addr as u64)).is_ok() {
            if file.write_all(&patch).is_ok() {
                let _ = file.flush();
                written_via_proc_mem = true;
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] [UUID_PATCH] Wrote patch successfully via /proc/self/mem\n\0".as_ptr() as *const c_char,
                );
                let _ = libc::fflush(stderr_ptr());
            }
        }
    }

    // Method 2: Fallback to mprotect if /proc/self/mem failed
    if !written_via_proc_mem {
        let page_size = libc::sysconf(libc::_SC_PAGESIZE) as usize;
        if page_size == 0 {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] [UUID_PATCH] WARNING: sysconf page size is 0\n\0".as_ptr() as *const c_char,
            );
            let _ = libc::fflush(stderr_ptr());
            return;
        }

        let page_start = target_addr & !(page_size - 1);
        
        let mut protect_res = libc::mprotect(
            page_start as *mut libc::c_void,
            page_size,
            libc::PROT_READ | libc::PROT_WRITE,
        );

        if protect_res != 0 {
            protect_res = libc::mprotect(
                page_start as *mut libc::c_void,
                page_size,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            );
        }

        if protect_res != 0 {
            let err = std::io::Error::last_os_error();
            let err_msg = err.to_string();
            let err_c = CString::new(err_msg).unwrap_or_default();
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] [UUID_PATCH] WARNING: mprotect failed: raw_os_error=%d, msg='%s'\n\0".as_ptr() as *const c_char,
                err.raw_os_error().unwrap_or(0),
                err_c.as_ptr(),
            );
            let _ = libc::fflush(stderr_ptr());
            return;
        }

        std::ptr::copy_nonoverlapping(patch.as_ptr(), target_addr as *mut u8, patch.len());

        let restore_res = libc::mprotect(
            page_start as *mut libc::c_void,
            page_size,
            libc::PROT_READ | libc::PROT_EXEC,
        );

        if restore_res != 0 {
            let err = std::io::Error::last_os_error();
            let err_msg = err.to_string();
            let err_c = CString::new(err_msg).unwrap_or_default();
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] [UUID_PATCH] WARNING: mprotect restore failed: raw_os_error=%d, msg='%s'\n\0".as_ptr() as *const c_char,
                err.raw_os_error().unwrap_or(0),
                err_c.as_ptr(),
            );
            let _ = libc::fflush(stderr_ptr());
        }
    }

    // Flush instruction cache
    extern "C" {
        fn __clear_cache(start: *mut libc::c_void, end: *mut libc::c_void);
    }
    __clear_cache(target_addr as *mut libc::c_void, (target_addr + patch.len()) as *mut libc::c_void);

    let _ = libc::fprintf(
        stderr_ptr(),
        b"[SHIM_INIT] [UUID_PATCH] Successfully patched UUID parser in memory!\n\0".as_ptr() as *const c_char,
    );
    let _ = libc::fflush(stderr_ptr());
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn patch_uuid_parser() {}

#[no_mangle]
/// # Safety
/// This is an ABI-level interposition hook for C++ exceptions.
/// Callers must follow the platform C++ ABI for `__cxa_throw`.
pub unsafe extern "C" fn __cxa_throw(
    thrown_exception: *mut c_void,
    tinfo: *mut c_void,
    dest: Option<unsafe extern "C" fn(*mut c_void)>,
) -> ! {
    let orig = match read_cxa_throw() {
        Some(f) => f,
        None => libc::abort(),
    };

    if !exception_catcher_enabled() {
        orig(thrown_exception, tinfo, dest);
    }

    let (handled, _should_call_original) = handle_exception_with_tls(thrown_exception, tinfo);

    if handled == 0 {
        orig(thrown_exception, tinfo, dest);
    }

    orig(thrown_exception, tinfo, dest);
}

#[cfg(target_env = "musl")]
unsafe fn install_signal_handler(signum: c_int) {
    let handler: extern "C" fn(c_int) = db_interpose_common::common_signal_handler;
    libc::signal(signum, handler as libc::sighandler_t);
}

#[cfg(not(target_env = "musl"))]
unsafe fn install_signal_handler(signum: c_int) {
    let handler: extern "C" fn(c_int) = db_interpose_common::common_signal_handler;
    libc::signal(signum, handler as libc::sighandler_t);
}

#[no_mangle]
/// # Safety
/// This is an ABI-level interposition hook for `sigaction`. The caller must
/// provide valid pointers (or NULL where allowed by the libc API).
pub unsafe extern "C" fn sigaction(
    signum: c_int,
    act: *const libc::sigaction,
    oldact: *mut libc::sigaction,
) -> c_int {
    let Some(orig) = read_sigaction() else {
        return -1;
    };

    if INTERCEPT_SIGACTION.load(Ordering::Relaxed) == 0 {
        return orig(signum, act, oldact);
    }

    if FORCE_IGNORE_SIGCHLD.load(Ordering::Relaxed) != 0
        && signum == libc::SIGCHLD
        && !act.is_null()
    {
        if !oldact.is_null() {
            orig(libc::SIGCHLD, ptr::null(), oldact);
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_NOCLDSTOP;
        return orig(libc::SIGCHLD, &sa, ptr::null_mut());
    }

    if signal_log_enabled()
        && !act.is_null()
        && (signum == libc::SIGSEGV
            || signum == libc::SIGABRT
            || signum == libc::SIGFPE
            || signum == libc::SIGILL
            || {
                #[cfg(target_os = "linux")]
                {
                    signum == libc::SIGBUS
                }
                #[cfg(not(target_os = "linux"))]
                {
                    false
                }
            })
    {
        if !oldact.is_null() {
            orig(signum, ptr::null(), oldact);
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction =
            db_interpose_common::common_signal_handler as extern "C" fn(c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        return orig(signum, &sa, ptr::null_mut());
    }

    orig(signum, act, oldact)
}

static mut REAL_SQLITE_HANDLE: *mut c_void = ptr::null_mut();

unsafe fn load_original_functions() {
    let sqlite_paths: [&[u8]; 3] = [
        b"/usr/local/lib/plex-postgresql/libsqlite3_real.so\0",
        b"/usr/lib/plexmediaserver/lib/libsqlite3.so.original\0",
        b"/usr/lib/plexmediaserver/lib/libsqlite3.so\0",
    ];

    let mut handle: *mut c_void = ptr::null_mut();
    for path in sqlite_paths.iter() {
        handle = libc::dlopen(
            path.as_ptr() as *const c_char,
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if !handle.is_null() {
            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Loaded real SQLite from %s\n\0".as_ptr() as *const c_char,
                path.as_ptr() as *const c_char,
            );
            ptr::write(ptr::addr_of_mut!(REAL_SQLITE_HANDLE), handle);
            break;
        }
    }

    if handle.is_null() {
        let _ = libc::fprintf(
            stderr_ptr(),
            b"[SHIM_INIT] Loading original SQLite functions via RTLD_NEXT...\n\0".as_ptr()
                as *const c_char,
        );
        handle = libc::RTLD_NEXT;
    }

    db_interpose_common::common_load_sqlite_symbols(handle);
    let _ = libc::fprintf(
        stderr_ptr(),
        b"[SHIM_INIT] Original SQLite functions loaded\n\0".as_ptr() as *const c_char,
    );
}

#[no_mangle]
pub extern "C" fn ensure_real_sqlite_loaded() {
    unsafe {
        if ptr::read(ptr::addr_of!(db_interpose_common::shim_sqlite3_prepare_v2)).is_some() {
            return;
        }
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_prepare_v2),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_prepare_v2)),
        );
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_errmsg),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_errmsg)),
        );
        ptr::write(
            ptr::addr_of_mut!(db_interpose_common::shim_sqlite3_errcode),
            ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_errcode)),
        );
    }
}

unsafe extern "C" fn shim_init() {
    // Eagerly resolve all interposition hooks before any other thread can
    // call the interposed wrappers.  This eliminates data races on the
    // lazy-init pattern that was previously used.
    resolve_interposition_hooks();
    crate::pms_child_env::init_child_env_hooks();
    crate::pms_net_compat::init_net_compat_hooks();

    shim_init_common(
        "Linux",
        || {
            // Process name filtering: skip non-server/scanner processes.
            if let Ok(cmdline) = std::fs::read("/proc/self/cmdline") {
                let mut base = cmdline.as_slice();
                if let Some(pos) = cmdline.iter().rposition(|&b| b == b'/') {
                    base = &cmdline[pos + 1..];
                }
                if let Some(pos) = base.iter().position(|&b| b == 0) {
                    base = &base[..pos];
                }
                let base_str = std::str::from_utf8(base).unwrap_or_default();
                if !base_str.contains("Plex Media Server")
                    && !base_str.contains("Plex Media Scanner")
                {
                    crate::pms_child_env::maybe_reexec_current_process_without_shim(
                        base_str, &cmdline,
                    );
                    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
                    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
                    db_interpose_common::SHIM_PASSTHROUGH_ONLY.store(1, Ordering::Release);
                    load_original_functions();
                    db_interpose_common::SHIM_INITIALIZED.store(1, Ordering::Release);
                    let base_c = CString::new(base_str).unwrap_or_default();
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] Not Plex Server/Scanner ('%s'), skipping entirely (PID %d)\n\0"
                            .as_ptr() as *const c_char,
                        base_c.as_ptr(),
                        libc::getpid(),
                    );
                    let _ = libc::fflush(stderr_ptr());
                    return false;
                }

                if env_utils::env_truthy(b"PLEX_PG_DISABLE_SIGCHLD_IGNORE\0") {
                    FORCE_IGNORE_SIGCHLD.store(0, Ordering::Relaxed);
                }
                if env_utils::env_truthy(b"PLEX_PG_FORCE_SIGCHLD_IGNORE\0") {
                    FORCE_IGNORE_SIGCHLD.store(1, Ordering::Relaxed);
                }
                if env_utils::env_truthy(b"PLEX_PG_DISABLE_SIGACTION_INTERCEPT\0") {
                    INTERCEPT_SIGACTION.store(0, Ordering::Relaxed);
                }
            }

            db_interpose_common::common_check_fork();

            let _ = libc::fprintf(
                stderr_ptr(),
                b"[SHIM_INIT] Fork safety: using PID-based detection (no pthread_atfork)\n\0"
                    .as_ptr() as *const c_char,
            );
            let _ = libc::fflush(stderr_ptr());

            load_original_functions();

            if ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_open)).is_none()
                || ptr::read(ptr::addr_of!(db_interpose_common::orig_sqlite3_prepare_v2)).is_none()
            {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] SQLite not found in this process, skipping initialization\n\0"
                        .as_ptr() as *const c_char,
                );
                let _ = libc::fflush(stderr_ptr());
                return false;
            }

            true
        },
        || {},
        || {
            setup_exception_catcher_if_enabled();
            unsafe { patch_uuid_parser(); }
            crate::pms_child_env::configure_from_env();
            crate::pms_child_env::scrub_current_process_preload();
            crate::pms_process_compat::configure_from_env();
            crate::pms_net_compat::configure_from_env();

            if env_utils::env_truthy(b"PLEX_PG_ENABLE_SIGNAL_LOG\0") {
                install_signal_handler(libc::SIGSEGV);
                install_signal_handler(libc::SIGABRT);
                install_signal_handler(libc::SIGFPE);
                install_signal_handler(libc::SIGILL);
                #[cfg(target_os = "linux")]
                {
                    install_signal_handler(libc::SIGBUS);
                }
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] Signal logging ENABLED via PLEX_PG_ENABLE_SIGNAL_LOG (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
                let _ = libc::fflush(stderr_ptr());
            }

            if FORCE_IGNORE_SIGCHLD.load(Ordering::Relaxed) != 0 {
                if let Some(orig) = read_sigaction() {
                    let mut sa: libc::sigaction = std::mem::zeroed();
                    sa.sa_sigaction = libc::SIG_IGN;
                    libc::sigemptyset(&mut sa.sa_mask);
                    sa.sa_flags = libc::SA_NOCLDSTOP;
                    orig(libc::SIGCHLD, &sa, ptr::null_mut());
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] SIGCHLD forced to SIG_IGN (PID %d)\n\0".as_ptr()
                            as *const c_char,
                        libc::getpid(),
                    );
                } else {
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] WARNING: could not resolve sigaction; SIGCHLD policy unchanged (PID %d)\n\0"
                            .as_ptr() as *const c_char,
                        libc::getpid(),
                    );
                }
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] SIGCHLD force-ignore disabled via PLEX_PG_DISABLE_SIGCHLD_IGNORE (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
            }

            if INTERCEPT_SIGACTION.load(Ordering::Relaxed) != 0 {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] sigaction interpose ENABLED (PID %d)\n\0".as_ptr()
                        as *const c_char,
                    libc::getpid(),
                );
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] sigaction interpose DISABLED via PLEX_PG_DISABLE_SIGACTION_INTERCEPT (PID %d)\n\0"
                        .as_ptr() as *const c_char,
                    libc::getpid(),
                );
            }
            let _ = libc::fflush(stderr_ptr());
        },
        || {
            if !env_utils::env_truthy(b"PLEX_PG_NO_INIT_DELAY\0") {
                let delay_ms = env_utils::env_string("PLEX_PG_INIT_DELAY_MS")
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(200);
                if delay_ms > 0 {
                    let _ = libc::fprintf(
                        stderr_ptr(),
                        b"[SHIM_INIT] Waiting %d ms for symbol resolution (PID %d)...\n\0".as_ptr()
                            as *const c_char,
                        delay_ms,
                        libc::getpid(),
                    );
                    let _ = libc::fflush(stderr_ptr());
                    libc::usleep((delay_ms as u32) * 1000);
                }
            } else {
                let _ = libc::fprintf(
                    stderr_ptr(),
                    b"[SHIM_INIT] Init delay DISABLED via PLEX_PG_NO_INIT_DELAY\n\0".as_ptr()
                        as *const c_char,
                );
                let _ = libc::fflush(stderr_ptr());
            }
        },
    );
}

unsafe extern "C" fn shim_cleanup() {
    if db_interpose_common::SHIM_INITIALIZED.load(Ordering::Acquire) == 0 {
        return;
    }
    log_shim_unloading("Linux");
    db_interpose_common::common_shim_cleanup();
}

extern "C" fn shim_init_wrapper() {
    unsafe { shim_init() }
}

extern "C" fn shim_cleanup_wrapper() {
    unsafe { shim_cleanup() }
}

#[used]
#[cfg_attr(target_os = "linux", link_section = ".init_array")]
static INIT: extern "C" fn() = shim_init_wrapper;

#[used]
#[cfg_attr(target_os = "linux", link_section = ".fini_array")]
static FINI: extern "C" fn() = shim_cleanup_wrapper;

// ────────────────────────────────────────────────────────────────────────────
// ────────────────────────────────────────────────────────────────────────────
// AArch64 assembly hook for boost::locale::util::create_simple_codecvt.
//
// Why assembly? On AArch64 the Itanium C++ ABI returns std::locale (a
// non-trivially copyable 8-byte struct) via the x8 "indirect result"
// register (SRET), NOT in x0. Plex's Boost saves x8 on entry, so any Rust
// wrapper that clobbers x8 before forwarding the call writes the new locale
// object to a garbage address → SIGSEGV in locale::operator=.
//
// This hook:
//   - Saves x8 + all args on the stack without touching x8 mid-flight.
//   - Detects ASCII in x1 (the string ptr) and redirects to create_utf8_codecvt
//     (x0=locale, x1=facet, x8 restored from stack).
//   - Falls through to the original create_simple_codecvt otherwise.
// ────────────────────────────────────────────────────────────────────────────
#[cfg(all(feature = "interpose", target_arch = "aarch64"))]
std::arch::global_asm!(
    // Export the symbol so LD_PRELOAD interposition takes effect.
    ".global _ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE",
    ".type   _ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE, %function",
    "_ZN5boost6locale4util21create_simple_codecvtERKNSt3__26localeERKNS2_12basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEENS0_12char_facet_tE:",
    // ABI on entry:
    //   x8  = SRET pointer (output locale buffer allocated by caller)
    //   x0  = const std::locale& in    (input locale)
    //   x1  = const std::string& encoding
    //   x2  = char_facet_t type
    //   x30 = return address
    //
    // Stack frame layout (48 bytes, 16-byte aligned):
    //   [sp+0]  x29 (frame ptr)    [sp+8]  x30 (lr)
    //   [sp+16] x8  (SRET ptr)
    //   [sp+24] x0  (locale ptr)
    //   [sp+32] x1  (string ptr)   [sp+40] x2  (facet)
    "stp  x29, x30, [sp, #-48]!",
    "mov  x29, sp",
    "str  x8,  [sp, #16]",
    "str  x0,  [sp, #24]",
    "stp  x1,  x2,  [sp, #32]",
    // Check: is *x1 == 'A','S','C','I','I','\0'?
    "ldrb w9,  [x1]",
    "cmp  w9,  #65",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #1]",
    "cmp  w9,  #83",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #2]",
    "cmp  w9,  #67",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #3]",
    "cmp  w9,  #73",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #4]",
    "cmp  w9,  #73",
    "b.ne .Lshim_csc_orig",
    "ldrb w9,  [x1, #5]",
    "cbnz w9,  .Lshim_csc_orig",
    // ASCII detected — GOT-indirect load of fn ptr, then tail-call
    // create_utf8_codecvt(locale, facet) with x8 = original SRET pointer.
    // We must use the GOT (:got: / :got_lo12:) because SHIM_* are exported
    // symbols; direct ADRP generates R_AARCH64_ADR_PREL_PG_HI21 which the
    // linker rejects in a PIC shared object.
    "adrp x9,  :got:SHIM_CREATE_UTF8_CODECVT_PTR",
    "ldr  x9,  [x9, :got_lo12:SHIM_CREATE_UTF8_CODECVT_PTR]",
    "ldr  x9,  [x9]",                     // x9 = fn-ptr value
    "cbz  x9,  .Lshim_csc_orig",          // if not resolved, fall through
    "ldr  x0,  [sp, #24]",                // locale ptr
    "ldr  x1,  [sp, #40]",                // facet (was x2)
    "ldr  x8,  [sp, #16]",                // restore SRET!
    "ldp  x29, x30, [sp], #48",
    "br   x9",                            // tail call
    ".Lshim_csc_orig:",
    // Not ASCII — tail-call original create_simple_codecvt unchanged.
    "adrp x9,  :got:SHIM_CREATE_SIMPLE_CODECVT_PTR",
    "ldr  x9,  [x9, :got_lo12:SHIM_CREATE_SIMPLE_CODECVT_PTR]",
    "ldr  x9,  [x9]",                     // x9 = fn-ptr value
    "cbz  x9,  .Lshim_csc_abort",
    "ldr  x0,  [sp, #24]",
    "ldr  x1,  [sp, #32]",
    "ldr  x2,  [sp, #40]",
    "ldr  x8,  [sp, #16]",                // restore SRET!
    "ldp  x29, x30, [sp], #48",
    "br   x9",
    ".Lshim_csc_abort:",
    // Resolver didn't run — hard abort.
    "bl   abort",
);

// ────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "interpose")]
mod ld_preload_wrappers {
    use super::*;

    macro_rules! wrap_db_ret {
        ($name:ident, $ret:ty, $my:ident) => {
            #[no_mangle]
            pub extern "C" fn $name(db: *mut sqlite3) -> $ret {
                c_abi::$my(db)
            }
        };
    }

    macro_rules! wrap_stmt_ret {
        ($name:ident, $ret:ty, $my:ident) => {
            #[no_mangle]
            pub extern "C" fn $name(stmt: *mut sqlite3_stmt) -> $ret {
                c_abi::$my(stmt)
            }
        };
    }

    macro_rules! wrap_stmt_idx {
        ($name:ident, $ret:ty, $my:ident) => {
            #[no_mangle]
            pub extern "C" fn $name(stmt: *mut sqlite3_stmt, idx: c_int) -> $ret {
                c_abi::$my(stmt, idx)
            }
        };
    }

    macro_rules! wrap_val_ret {
        ($name:ident, $ret:ty, $my:ident) => {
            #[no_mangle]
            pub extern "C" fn $name(val: *mut sqlite3_value) -> $ret {
                c_abi::$my(val)
            }
        };
    }

    wrap_db_ret!(sqlite3_changes, c_int, my_sqlite3_changes);
    wrap_db_ret!(sqlite3_changes64, i64, my_sqlite3_changes64);
    wrap_db_ret!(sqlite3_last_insert_rowid, i64, my_sqlite3_last_insert_rowid);
    wrap_db_ret!(sqlite3_errmsg, *const c_char, my_sqlite3_errmsg);
    wrap_db_ret!(sqlite3_errcode, c_int, my_sqlite3_errcode);
    wrap_db_ret!(sqlite3_extended_errcode, c_int, my_sqlite3_extended_errcode);

    wrap_stmt_ret!(sqlite3_step, c_int, my_sqlite3_step);
    wrap_stmt_ret!(sqlite3_reset, c_int, my_sqlite3_reset);
    wrap_stmt_ret!(sqlite3_finalize, c_int, my_sqlite3_finalize);
    wrap_stmt_ret!(sqlite3_clear_bindings, c_int, my_sqlite3_clear_bindings);
    wrap_stmt_ret!(sqlite3_column_count, c_int, my_sqlite3_column_count);
    wrap_stmt_ret!(sqlite3_data_count, c_int, my_sqlite3_data_count);
    wrap_stmt_ret!(
        sqlite3_bind_parameter_count,
        c_int,
        my_sqlite3_bind_parameter_count
    );
    wrap_stmt_ret!(sqlite3_stmt_readonly, c_int, my_sqlite3_stmt_readonly);
    wrap_stmt_ret!(sqlite3_stmt_busy, c_int, my_sqlite3_stmt_busy);
    wrap_stmt_ret!(sqlite3_db_handle, *mut sqlite3, my_sqlite3_db_handle);
    wrap_stmt_ret!(sqlite3_expanded_sql, *mut c_char, my_sqlite3_expanded_sql);
    wrap_stmt_ret!(sqlite3_sql, *const c_char, my_sqlite3_sql);

    wrap_stmt_idx!(sqlite3_column_type, c_int, my_sqlite3_column_type);
    wrap_stmt_idx!(sqlite3_column_int, c_int, my_sqlite3_column_int);
    wrap_stmt_idx!(sqlite3_column_int64, i64, my_sqlite3_column_int64);
    wrap_stmt_idx!(sqlite3_column_double, f64, my_sqlite3_column_double);
    wrap_stmt_idx!(sqlite3_column_text, *const u8, my_sqlite3_column_text);
    wrap_stmt_idx!(sqlite3_column_blob, *const c_void, my_sqlite3_column_blob);
    wrap_stmt_idx!(sqlite3_column_bytes, c_int, my_sqlite3_column_bytes);
    wrap_stmt_idx!(sqlite3_column_name, *const c_char, my_sqlite3_column_name);
    wrap_stmt_idx!(
        sqlite3_column_value,
        *mut sqlite3_value,
        my_sqlite3_column_value
    );
    wrap_stmt_idx!(
        sqlite3_bind_parameter_name,
        *const c_char,
        my_sqlite3_bind_parameter_name
    );

    wrap_val_ret!(sqlite3_value_type, c_int, my_sqlite3_value_type);
    wrap_val_ret!(sqlite3_value_text, *const u8, my_sqlite3_value_text);
    wrap_val_ret!(sqlite3_value_int, c_int, my_sqlite3_value_int);
    wrap_val_ret!(sqlite3_value_int64, i64, my_sqlite3_value_int64);
    wrap_val_ret!(sqlite3_value_double, f64, my_sqlite3_value_double);
    wrap_val_ret!(sqlite3_value_bytes, c_int, my_sqlite3_value_bytes);
    wrap_val_ret!(sqlite3_value_blob, *const c_void, my_sqlite3_value_blob);

    #[no_mangle]
    pub extern "C" fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int {
        c_abi::my_sqlite3_open(filename, db)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_open_v2(
        filename: *const c_char,
        db: *mut *mut sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_open_v2(filename, db, flags, vfs)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_close(db: *mut sqlite3) -> c_int {
        c_abi::my_sqlite3_close(db)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_close_v2(db: *mut sqlite3) -> c_int {
        c_abi::my_sqlite3_close_v2(db)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        cb: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int {
        c_abi::my_sqlite3_exec(db, sql, cb, arg, errmsg)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_get_table(
        db: *mut sqlite3,
        sql: *const c_char,
        results: *mut *mut *mut c_char,
        nrow: *mut c_int,
        ncol: *mut c_int,
        errmsg: *mut *mut c_char,
    ) -> c_int {
        c_abi::my_sqlite3_get_table(db, sql, results, nrow, ncol, errmsg)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_prepare(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_prepare(db, sql, n, stmt, tail)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_prepare_v2(db, sql, n, stmt, tail)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_prepare_v3(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        flags: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_prepare_v3(db, sql, n, flags as u32, stmt, tail)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_prepare16_v2(
        db: *mut sqlite3,
        sql: *const c_void,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_void,
    ) -> c_int {
        c_abi::my_sqlite3_prepare16_v2(db, sql, n, stmt, tail)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_int(stmt: *mut sqlite3_stmt, idx: c_int, val: c_int) -> c_int {
        c_abi::my_sqlite3_bind_int(stmt, idx, val)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, idx: c_int, val: i64) -> c_int {
        c_abi::my_sqlite3_bind_int64(stmt, idx, val)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_double(stmt: *mut sqlite3_stmt, idx: c_int, val: f64) -> c_int {
        c_abi::my_sqlite3_bind_double(stmt, idx, val)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, idx: c_int) -> c_int {
        c_abi::my_sqlite3_bind_null(stmt, idx)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_text(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        n: c_int,
        dtor: *mut c_void,
    ) -> c_int {
        c_abi::my_sqlite3_bind_text(stmt, idx, val, n, dtor)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_text64(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        n: u64,
        dtor: *mut c_void,
        enc: u8,
    ) -> c_int {
        c_abi::my_sqlite3_bind_text64(stmt, idx, val, n, dtor, enc)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_blob(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_void,
        n: c_int,
        dtor: *mut c_void,
    ) -> c_int {
        c_abi::my_sqlite3_bind_blob(stmt, idx, val, n, dtor)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_blob64(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_void,
        n: u64,
        dtor: *mut c_void,
    ) -> c_int {
        c_abi::my_sqlite3_bind_blob64(stmt, idx, val, n, dtor)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_value(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const sqlite3_value,
    ) -> c_int {
        c_abi::my_sqlite3_bind_value(stmt, idx, val)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_bind_parameter_index(
        stmt: *mut sqlite3_stmt,
        name: *const c_char,
    ) -> c_int {
        c_abi::my_sqlite3_bind_parameter_index(stmt, name)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_stmt_status(
        stmt: *mut sqlite3_stmt,
        op: c_int,
        reset: c_int,
    ) -> c_int {
        c_abi::my_sqlite3_stmt_status(stmt, op, reset)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_free(ptr: *mut c_void) {
        c_abi::my_sqlite3_free(ptr);
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_malloc(n: c_int) -> *mut c_void {
        c_abi::my_sqlite3_malloc(n)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_create_collation(
        db: *mut sqlite3,
        name: *const c_char,
        enc: c_int,
        arg: *mut c_void,
        cmp: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
        >,
    ) -> c_int {
        c_abi::my_sqlite3_create_collation(db, name, enc, arg, cmp)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_create_collation_v2(
        db: *mut sqlite3,
        name: *const c_char,
        enc: c_int,
        arg: *mut c_void,
        cmp: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
        >,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int {
        c_abi::my_sqlite3_create_collation_v2(db, name, enc, arg, cmp, destroy)
    }

    #[no_mangle]
    pub extern "C" fn sqlite3_column_decltype(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
    ) -> *const c_char {
        c_abi::my_sqlite3_column_decltype(stmt, idx)
    }

    // Simple pass-through: let create_simple_converter proceed normally.
    // The ASCII → UTF-8 redirect is now handled exclusively at the
    // create_simple_codecvt level by the AArch64 global_asm hook above.
    #[no_mangle]
    #[allow(static_mut_refs)]
    pub unsafe extern "C" fn _ZN5boost6locale4util23create_simple_converterERKNSt3__212basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEE(
        s: *mut u8,
    ) -> *mut c_void {
        let orig = match ptr::read(ptr::addr_of!(ORIG_CREATE_SIMPLE_CONVERTER)) {
            Some(f) => f,
            None => {
                let name = b"_ZN5boost6locale4util23create_simple_converterERKNSt3__212basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEE\0";
                let sym = libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const c_char);
                if sym.is_null() {
                    libc::abort();
                }
                let f = std::mem::transmute::<*mut c_void, CreateSimpleConverterFn>(sym);
                ptr::write(ptr::addr_of_mut!(ORIG_CREATE_SIMPLE_CONVERTER), Some(f));
                f
            }
        };
        orig(s)
    }
    // Note: create_simple_codecvt is implemented as a global_asm hook above
    // (AArch64 only) to correctly preserve the x8 SRET pointer while
    // redirecting ASCII charset requests to create_utf8_codecvt.
} // mod ld_preload_wrappers

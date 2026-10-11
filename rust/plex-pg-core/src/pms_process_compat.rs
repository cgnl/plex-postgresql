#![cfg(target_os = "linux")]

use std::mem;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::db_interpose_common;
use crate::db_interpose_common::stderr_ptr;
use crate::env_utils;

type CloneStartFn = unsafe extern "C" fn(*mut libc::c_void) -> c_int;
type CloneFn = unsafe extern "C" fn(
    Option<CloneStartFn>,
    *mut libc::c_void,
    c_int,
    *mut libc::c_void,
    *mut libc::pid_t,
    *mut libc::c_void,
    *mut libc::pid_t,
) -> c_int;
type DaemonFn = unsafe extern "C" fn(c_int, c_int) -> c_int;
type ForkFn = unsafe extern "C" fn() -> libc::pid_t;
type PrctlFn = unsafe extern "C" fn(
    c_int,
    libc::c_ulong,
    libc::c_ulong,
    libc::c_ulong,
    libc::c_ulong,
) -> c_int;
type PthreadSetnameNpFn = unsafe extern "C" fn(libc::pthread_t, *const c_char) -> c_int;
type SetsidFn = unsafe extern "C" fn() -> libc::pid_t;
type SyscallFn = unsafe extern "C" fn(
    libc::c_long,
    libc::c_long,
    libc::c_long,
    libc::c_long,
    libc::c_long,
    libc::c_long,
    libc::c_long,
) -> libc::c_long;

static mut ORIG_DAEMON: Option<DaemonFn> = None;
static mut ORIG_FORK: Option<ForkFn> = None;
static mut ORIG_CLONE: Option<CloneFn> = None;
static mut ORIG_PRCTL: Option<PrctlFn> = None;
static mut ORIG_PTHREAD_SETNAME_NP: Option<PthreadSetnameNpFn> = None;
static mut ORIG_SETSID: Option<SetsidFn> = None;
static mut ORIG_SYSCALL: Option<SyscallFn> = None;

static PROCESS_COMPAT_LOG_BUDGET: AtomicI32 = AtomicI32::new(0);
static PROCESS_COMPAT_INIT_PID: AtomicI32 = AtomicI32::new(0);
static SUPPRESS_DAEMON: AtomicI32 = AtomicI32::new(0);

const DEFAULT_LOG_BUDGET: i32 = 24;
const CLONE_WRAP_MASK: c_int = libc::CLONE_THREAD | libc::CLONE_VM | libc::CLONE_SIGHAND;
const RAW_CLONE_WRAP_MASK: u64 =
    libc::CLONE_THREAD as u64 | libc::CLONE_VM as u64 | libc::CLONE_SIGHAND as u64;

#[repr(C)]
struct CloneContext {
    start_fn: Option<CloneStartFn>,
    arg: *mut libc::c_void,
}

#[derive(Copy, Clone)]
struct CloneAuxArgs {
    parent_tid: *mut libc::pid_t,
    tls: *mut libc::c_void,
    child_tid: *mut libc::pid_t,
}

unsafe fn resolve_symbol<T>(slot: &mut Option<T>, name: &'static [u8]) -> Option<T>
where
    T: Copy,
{
    if let Some(f) = *slot {
        return Some(f);
    }

    let sym = libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const c_char);
    if sym.is_null() {
        return None;
    }

    let f: T = mem::transmute_copy(&sym);
    *slot = Some(f);
    Some(f)
}

unsafe fn resolve_daemon() -> Option<DaemonFn> {
    resolve_symbol(&mut ORIG_DAEMON, b"daemon\0")
}

unsafe fn resolve_clone() -> Option<CloneFn> {
    resolve_symbol(&mut ORIG_CLONE, b"clone\0")
}

unsafe fn resolve_fork() -> Option<ForkFn> {
    resolve_symbol(&mut ORIG_FORK, b"fork\0")
}

unsafe fn resolve_prctl() -> Option<PrctlFn> {
    resolve_symbol(&mut ORIG_PRCTL, b"prctl\0")
}

unsafe fn resolve_pthread_setname_np() -> Option<PthreadSetnameNpFn> {
    resolve_symbol(&mut ORIG_PTHREAD_SETNAME_NP, b"pthread_setname_np\0")
}

unsafe fn resolve_setsid() -> Option<SetsidFn> {
    resolve_symbol(&mut ORIG_SETSID, b"setsid\0")
}

unsafe fn resolve_syscall() -> Option<SyscallFn> {
    resolve_symbol(&mut ORIG_SYSCALL, b"syscall\0")
}

unsafe fn set_errno(err: c_int) {
    *libc::__errno_location() = err;
}

fn logging_enabled() -> bool {
    PROCESS_COMPAT_LOG_BUDGET.load(Ordering::Acquire) > 0
}

fn daemon_suppressed() -> bool {
    SUPPRESS_DAEMON.load(Ordering::Acquire) != 0
}

unsafe fn likely_pms_primary_process() -> bool {
    db_interpose_common::SHIM_PASSTHROUGH_ONLY.load(std::sync::atomic::Ordering::Acquire) == 0
}

fn should_wrap_clone(flags: c_int) -> bool {
    (flags & CLONE_WRAP_MASK) == 0
}

fn sanitize_clone_aux_args(
    flags: c_int,
    parent_tid: *mut libc::pid_t,
    tls: *mut libc::c_void,
    child_tid: *mut libc::pid_t,
) -> CloneAuxArgs {
    CloneAuxArgs {
        parent_tid: if (flags & libc::CLONE_PARENT_SETTID) != 0 {
            parent_tid
        } else {
            ptr::null_mut()
        },
        tls: if (flags & libc::CLONE_SETTLS) != 0 {
            tls
        } else {
            ptr::null_mut()
        },
        child_tid: if (flags & libc::CLONE_CHILD_SETTID) != 0 {
            child_tid
        } else {
            ptr::null_mut()
        },
    }
}

fn raw_clone_flags_process_like(flags: u64) -> bool {
    (flags & RAW_CLONE_WRAP_MASK) == 0
}

unsafe fn maybe_log_event(op: &'static [u8], rc: i64, err: c_int) {
    if !logging_enabled() || libc::getpid() != PROCESS_COMPAT_INIT_PID.load(Ordering::Acquire) {
        return;
    }

    let remaining = PROCESS_COMPAT_LOG_BUDGET.fetch_sub(1, Ordering::Relaxed);
    if remaining <= 0 {
        PROCESS_COMPAT_LOG_BUDGET.store(0, Ordering::Relaxed);
        return;
    }

    let _ = libc::fprintf(
        stderr_ptr(),
        b"[PMS_PROCESS_COMPAT] %s pid=%d ppid=%d rc=%lld errno=%d passthrough=%d initialized=%d\n\0"
            .as_ptr() as *const c_char,
        op.as_ptr() as *const c_char,
        libc::getpid(),
        libc::getppid(),
        rc,
        err,
        db_interpose_common::SHIM_PASSTHROUGH_ONLY.load(std::sync::atomic::Ordering::Acquire),
        db_interpose_common::SHIM_INITIALIZED.load(std::sync::atomic::Ordering::Acquire),
    );
    let _ = libc::fflush(stderr_ptr());
}

unsafe extern "C" fn clone_child_trampoline(arg: *mut libc::c_void) -> c_int {
    if arg.is_null() {
        return 127;
    }

    let ctx = &*arg.cast::<CloneContext>();
    db_interpose_common::linux_handle_fork_child("clone");
    match ctx.start_fn {
        Some(start_fn) => start_fn(ctx.arg),
        None => 127,
    }
}

unsafe fn clone3_flags(args_ptr: *const libc::c_void) -> Option<u64> {
    if args_ptr.is_null() {
        return None;
    }

    Some(ptr::read_unaligned(args_ptr.cast::<u64>()))
}

fn syscall_log_label(number: libc::c_long) -> Option<&'static [u8]> {
    if number == libc::SYS_clone as libc::c_long {
        Some(b"syscall[clone]\0")
    } else if number == libc::SYS_clone3 as libc::c_long {
        Some(b"syscall[clone3]\0")
    } else {
        None
    }
}

unsafe fn handle_syscall_child_fast_path(number: libc::c_long, a1: libc::c_long) {
    if number == libc::SYS_clone as libc::c_long {
        if raw_clone_flags_process_like(a1 as u64) {
            db_interpose_common::linux_handle_fork_child("syscall(SYS_clone)");
        }
    } else if number == libc::SYS_clone3 as libc::c_long {
        if clone3_flags(a1 as *const libc::c_void).is_some_and(raw_clone_flags_process_like) {
            db_interpose_common::linux_handle_fork_child("syscall(SYS_clone3)");
        }
    }
}

pub fn configure_from_env() {
    PROCESS_COMPAT_INIT_PID.store(unsafe { libc::getpid() }, Ordering::Release);
    unsafe {
        let _ = resolve_daemon();
        let _ = resolve_fork();
        let _ = resolve_clone();
        let _ = resolve_prctl();
        let _ = resolve_pthread_setname_np();
        let _ = resolve_setsid();
        let _ = resolve_syscall();
    }
    let suppress = env_utils::env_truthy(b"PLEX_PG_SUPPRESS_DAEMON\0");
    SUPPRESS_DAEMON.store(if suppress { 1 } else { 0 }, Ordering::Release);

    let budget = if suppress || env_utils::env_truthy(b"PLEX_PG_ENABLE_PROCESS_COMPAT_LOG\0") {
        env_utils::env_usize(b"PLEX_PG_PROCESS_COMPAT_LOG_LIMIT\0")
            .map(|v| v.min(i32::MAX as usize) as i32)
            .unwrap_or(DEFAULT_LOG_BUDGET)
            .max(0)
    } else {
        0
    };
    PROCESS_COMPAT_LOG_BUDGET.store(budget, Ordering::Release);

    unsafe {
        let _ = libc::fprintf(
            stderr_ptr(),
            if suppress {
                b"[SHIM_INIT] PMS process compat ENABLED (daemon suppression on; fork lifecycle logging active)\n\0"
                    .as_ptr() as *const c_char
            } else if budget > 0 {
                b"[SHIM_INIT] PMS process compat logging ENABLED (daemon suppression off)\n\0"
                    .as_ptr() as *const c_char
            } else {
                b"[SHIM_INIT] PMS process compat logging DISABLED\n\0".as_ptr() as *const c_char
            },
        );
        let _ = libc::fflush(stderr_ptr());
    }
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `daemon`. Callers must obey libc preconditions.
pub unsafe extern "C" fn daemon(nochdir: c_int, noclose: c_int) -> c_int {
    let Some(orig) = resolve_daemon() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    if daemon_suppressed() && likely_pms_primary_process() {
        if nochdir == 0 {
            let _ = libc::chdir(b"/\0".as_ptr() as *const c_char);
        }
        maybe_log_event(b"daemon[suppressed]\0", 0, 0);
        return 0;
    }

    let rc = orig(nochdir, noclose);
    let err = if rc != 0 {
        *libc::__errno_location()
    } else {
        0
    };
    maybe_log_event(b"daemon\0", i64::from(rc), err);
    rc
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `fork`. Callers must obey libc preconditions.
pub unsafe extern "C" fn fork() -> libc::pid_t {
    let Some(orig) = resolve_fork() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    let rc = orig();
    if rc == 0 {
        db_interpose_common::linux_handle_fork_child("fork");
    } else if rc > 0 {
        maybe_log_event(b"fork\0", i64::from(rc), 0);
    } else if rc < 0 {
        maybe_log_event(b"fork\0", -1, *libc::__errno_location());
    }
    rc
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `clone`. The process-wrapping path only
/// applies to process-like clones; thread-like clones pass through unchanged.
pub unsafe extern "C" fn clone(
    start_fn: Option<CloneStartFn>,
    stack: *mut libc::c_void,
    flags: c_int,
    arg: *mut libc::c_void,
    parent_tid: *mut libc::pid_t,
    tls: *mut libc::c_void,
    child_tid: *mut libc::pid_t,
) -> c_int {
    let Some(orig) = resolve_clone() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    let aux = sanitize_clone_aux_args(flags, parent_tid, tls, child_tid);
    if !should_wrap_clone(flags) || start_fn.is_none() {
        let rc = orig(
            start_fn,
            stack,
            flags,
            arg,
            aux.parent_tid,
            aux.tls,
            aux.child_tid,
        );
        let err = if rc < 0 { *libc::__errno_location() } else { 0 };
        maybe_log_event(b"clone[passthrough]\0", i64::from(rc), err);
        return rc;
    }

    let ctx = Box::new(CloneContext { start_fn, arg });
    let ctx_ptr = Box::into_raw(ctx);
    let rc = orig(
        Some(clone_child_trampoline),
        stack,
        flags,
        ctx_ptr.cast(),
        aux.parent_tid,
        aux.tls,
        aux.child_tid,
    );

    drop(Box::from_raw(ctx_ptr));

    let err = if rc < 0 { *libc::__errno_location() } else { 0 };
    maybe_log_event(b"clone[wrap]\0", i64::from(rc), err);
    rc
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `prctl`. Callers must obey libc preconditions.
pub unsafe extern "C" fn prctl(
    option: c_int,
    arg2: libc::c_ulong,
    arg3: libc::c_ulong,
    arg4: libc::c_ulong,
    arg5: libc::c_ulong,
) -> c_int {
    let Some(orig) = resolve_prctl() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    let rc = orig(option, arg2, arg3, arg4, arg5);
    if rc == 0 && option == libc::PR_SET_NAME {
        maybe_log_event(b"prctl[set-name]\0", 0, 0);
    }
    rc
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `pthread_setname_np`. Callers must obey libc preconditions.
pub unsafe extern "C" fn pthread_setname_np(thread: libc::pthread_t, name: *const c_char) -> c_int {
    let Some(orig) = resolve_pthread_setname_np() else {
        return libc::ENOSYS;
    };

    let rc = orig(thread, name);
    if rc == 0 && libc::pthread_equal(thread, libc::pthread_self()) != 0 {
        maybe_log_event(b"pthread_setname_np\0", 0, 0);
    }
    rc
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `setsid`. Callers must obey libc preconditions.
pub unsafe extern "C" fn setsid() -> libc::pid_t {
    let Some(orig) = resolve_setsid() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    let rc = orig();
    let err = if rc < 0 { *libc::__errno_location() } else { 0 };
    maybe_log_event(b"setsid\0", i64::from(rc), err);
    rc
}

#[no_mangle]
/// # Safety
/// ABI interposition wrapper for `syscall`. This only adds special handling
/// for raw process-creation syscalls and otherwise passes through unchanged.
pub unsafe extern "C" fn syscall(
    number: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
    a6: libc::c_long,
) -> libc::c_long {
    let Some(orig) = resolve_syscall() else {
        set_errno(libc::ENOSYS);
        return -1;
    };

    let rc = orig(number, a1, a2, a3, a4, a5, a6);
    if rc == 0 {
        handle_syscall_child_fast_path(number, a1);
        return rc;
    }

    if let Some(op) = syscall_log_label(number) {
        let err = if rc < 0 { *libc::__errno_location() } else { 0 };
        maybe_log_event(op, rc, err);
    }

    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_wrap_policy_accepts_process_like_clone() {
        assert!(should_wrap_clone(libc::SIGCHLD));
    }

    #[test]
    fn clone_wrap_policy_rejects_thread_clone() {
        assert!(!should_wrap_clone(libc::SIGCHLD | libc::CLONE_THREAD));
    }

    #[test]
    fn clone_wrap_policy_rejects_shared_vm_clone() {
        assert!(!should_wrap_clone(libc::SIGCHLD | libc::CLONE_VM));
    }

    #[test]
    fn sanitize_clone_aux_args_zeroes_unused_slots() {
        let mut parent_tid: libc::pid_t = 0;
        let mut tls = 0u8;
        let mut child_tid: libc::pid_t = 0;
        let aux = sanitize_clone_aux_args(
            libc::SIGCHLD,
            std::ptr::from_mut(&mut parent_tid),
            std::ptr::from_mut(&mut tls).cast::<libc::c_void>(),
            std::ptr::from_mut(&mut child_tid),
        );
        assert!(aux.parent_tid.is_null());
        assert!(aux.tls.is_null());
        assert!(aux.child_tid.is_null());
    }

    #[test]
    fn sanitize_clone_aux_args_preserves_requested_slots() {
        let mut parent_tid_storage: libc::pid_t = 0;
        let mut tls_storage = 0u8;
        let mut child_tid_storage: libc::pid_t = 0;
        let parent_tid = std::ptr::from_mut(&mut parent_tid_storage);
        let tls = std::ptr::from_mut(&mut tls_storage).cast::<libc::c_void>();
        let child_tid = std::ptr::from_mut(&mut child_tid_storage);
        let aux = sanitize_clone_aux_args(
            libc::SIGCHLD
                | libc::CLONE_PARENT_SETTID
                | libc::CLONE_SETTLS
                | libc::CLONE_CHILD_SETTID,
            parent_tid,
            tls,
            child_tid,
        );
        assert_eq!(aux.parent_tid, parent_tid);
        assert_eq!(aux.tls, tls);
        assert_eq!(aux.child_tid, child_tid);
    }
}

use plex_pg_core::pg_logging::rust_logging_write;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

#[test]
fn logging_concurrency_completes() {
    static RUNNING: AtomicBool = AtomicBool::new(true);
    static MSGS: AtomicUsize = AtomicUsize::new(0);

    RUNNING.store(true, Ordering::Release);
    MSGS.store(0, Ordering::Release);

    let (tx, rx) = channel();
    let start = Instant::now();
    let threads = 10;
    let duration = Duration::from_millis(300);

    for tid in 0..threads {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let msg = CString::new(format!("[T{}] log message", tid)).unwrap();
            while RUNNING.load(Ordering::Acquire) && start.elapsed() < duration {
                rust_logging_write(2, msg.as_ptr());
                MSGS.fetch_add(1, Ordering::Relaxed);
            }
            let _ = tx.send(());
        });
    }

    for _ in 0..threads {
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    RUNNING.store(false, Ordering::Release);

    let total = MSGS.load(Ordering::Relaxed);
    assert!(total > 0, "expected some log messages to be written");
}

#[test]
fn column_type_verbose_is_not_log_info() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/db_interpose_column.rs");
    if !path.exists() {
        return;
    }

    let content = std::fs::read_to_string(&path).expect("read db_interpose_column.rs");
    for (idx, line) in content.lines().enumerate() {
        if line.contains("COLUMN_TYPE_VERBOSE") && line.contains("log_info") {
            panic!("COLUMN_TYPE_VERBOSE uses log_info at line {}", idx + 1);
        }
    }
}

#[test]
fn log_error_used_for_real_errors() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/db_interpose_column.rs");
    if !path.exists() {
        return;
    }

    let content = std::fs::read_to_string(&path).expect("read db_interpose_column.rs");
    let verbose_patterns = [
        "TYPE_DEBUG",
        "TYPE_VERBOSE",
        "COLUMN_TYPE_VERBOSE",
        "DECLTYPE_DEBUG",
        "CACHE_HIT",
        "CACHE_MISS",
    ];
    let acceptable = [
        "failed",
        "Failed",
        "error",
        "Error",
        "malloc",
        "alloc",
        "could not",
        "Could not",
        "cannot",
        "Cannot",
        "invalid",
        "Invalid",
    ];

    for (idx, line) in content.lines().enumerate() {
        if !line.contains("log_error") {
            continue;
        }
        let mut verbose_hit = false;
        for pat in &verbose_patterns {
            if line.contains(pat) {
                verbose_hit = true;
                break;
            }
        }
        if !verbose_hit {
            continue;
        }
        let mut ok = false;
        for pat in &acceptable {
            if line.contains(pat) {
                ok = true;
                break;
            }
        }
        if !ok {
            panic!("log_error used for verbose pattern at line {}", idx + 1);
        }
    }
}

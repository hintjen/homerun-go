//! Panic capture.
//!
//! On a phone there is no console to read a panic from, and a panic that
//! unwinds across the FFI boundary is undefined behaviour. So we install a
//! hook that persists what happened somewhere the app can find it, and the
//! extern functions catch unwinds rather than letting them escape.

use std::fs;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static CRASH_DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
/// Last panic message, so the host can surface it after a failed start.
static LAST_PANIC: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn crash_dir() -> &'static Mutex<Option<PathBuf>> {
    CRASH_DIR.get_or_init(|| Mutex::new(None))
}

fn last_panic() -> &'static Mutex<Option<String>> {
    LAST_PANIC.get_or_init(|| Mutex::new(None))
}

/// Point crash reports at this server's data directory.
pub fn set_crash_dir(dir: impl AsRef<Path>) {
    let dir = dir.as_ref().join("crash-reports");
    let _ = fs::create_dir_all(&dir);
    if let Ok(mut guard) = crash_dir().lock() {
        *guard = Some(dir);
    }
}

/// Install the panic hook. Idempotent — the hook is global, and installing
/// twice would chain it and double-report.
pub fn install_hook() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic(info);
        previous(info);
    }));
}

fn describe(info: &PanicHookInfo<'_>) -> String {
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    match info.location() {
        Some(loc) => format!("{payload} (at {}:{})", loc.file(), loc.line()),
        None => payload,
    }
}

fn record_panic(info: &PanicHookInfo<'_>) {
    let message = describe(info);

    if let Ok(mut guard) = last_panic().lock() {
        *guard = Some(message.clone());
    }

    // Best-effort: a panicking process must not panic again while reporting.
    let dir = crash_dir().lock().ok().and_then(|g| g.clone());
    if let Some(dir) = dir {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = fs::write(
            dir.join(format!("panic-{stamp}.txt")),
            format!("{message}\n\nbacktrace:\n{}", std::backtrace::Backtrace::force_capture()),
        );
    }
}

/// Take the last panic message, clearing it.
pub fn take_last_panic() -> Option<String> {
    last_panic().lock().ok().and_then(|mut g| g.take())
}

/// Serialises tests that touch the process-global crash state.
///
/// One server runs per process, so a single crash directory and last-panic
/// slot are right in production — but Rust runs tests in parallel threads,
/// which would otherwise race on them.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        // A failing assertion inside the guard poisons it; later tests
        // should still run rather than cascade.
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_twice_is_a_no_op() {
        install_hook();
        install_hook();
        assert!(INSTALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn a_caught_panic_is_recorded_and_readable() {
        let _guard = test_guard();
        install_hook();
        let _ = take_last_panic();

        let result = std::panic::catch_unwind(|| panic!("engine exploded"));
        assert!(result.is_err());

        let recorded = take_last_panic().expect("panic should have been recorded");
        assert!(recorded.contains("engine exploded"));
        // Location makes a report actionable.
        assert!(recorded.contains("crash.rs"));

        // Taking it clears it, so the next start does not inherit it.
        assert!(take_last_panic().is_none());
    }

    #[test]
    fn crash_reports_are_written_to_the_data_dir() {
        let _guard = test_guard();
        let dir = std::env::temp_dir().join(format!(
            "homerun-crash-test-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        set_crash_dir(&dir);
        install_hook();

        let _ = std::panic::catch_unwind(|| panic!("write me to disk"));

        let reports: Vec<_> = fs::read_dir(dir.join("crash-reports"))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(!reports.is_empty(), "expected a crash report file");

        let body = fs::read_to_string(reports[0].path()).unwrap();
        assert!(body.contains("write me to disk"));

        let _ = fs::remove_dir_all(&dir);
    }
}

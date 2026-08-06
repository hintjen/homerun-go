//! C ABI around the Pumpkin server, shared by the iOS and Android hosts.
//!
//! This crate is ours; the Pumpkin fork should shrink to library-mode
//! patches only (see Cargo.toml). Everything the prototype put *inside*
//! Pumpkin — the extern symbols, log capture, the bind pre-flight, the panic
//! hook — belongs here so it survives rebasing on upstream.
//!
//! # Rules this layer exists to enforce
//!
//! 1. **Never abort the host process.** A server that cannot bind must return
//!    an error, not `process::exit`. On a phone that is the whole app.
//! 2. **Never let a panic cross the FFI boundary** — that is undefined
//!    behaviour. Every extern fn wraps its body in `catch_unwind`.
//! 3. **Capture stdout/stderr.** Neither platform shows them, and the console
//!    is a headline feature.
//! 4. **One server at a time**, enforced here as well as in the hosts. The
//!    engine keeps global state and switches worlds by process CWD.
//!
//! Strings returned to the host are heap-allocated C strings that the caller
//! must release with `homerun_free_string`. Every getter returns JSON so the
//! surface stays narrow and versionable.

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

/// Bumped when the C surface changes shape. Hosts check it at startup.
pub const FFI_ABI_VERSION: u32 = 1;

#[no_mangle]
pub extern "C" fn homerun_abi_version() -> u32 {
    FFI_ABI_VERSION
}

/// Release a string returned by this library. Passing anything else is UB.
///
/// # Safety
/// `ptr` must have come from this library and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn homerun_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    drop(CString::from_raw(ptr));
}

/// Marshal a Rust string out to the caller, or null on allocation failure.
fn out(s: String) -> *mut c_char {
    CString::new(s).map(CString::into_raw).unwrap_or(ptr::null_mut())
}

/// Run `f`, converting a panic into a JSON error rather than unwinding into
/// Swift/Kotlin (which would be undefined behaviour).
fn guarded<F: FnOnce() -> String>(f: F) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(json) => out(json),
        Err(_) => out(
            r#"{"ok":false,"error":"internal server panic — see crash-reports/"}"#
                .to_string(),
        ),
    }
}

unsafe fn borrow<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Start the server rooted at `data_dir`, blocking until it stops.
///
/// Hosts must call this on a dedicated thread with **at least a 16 MB stack**
/// — the default 512 KB overflows inside the engine and kills the process
/// with no panic report.
///
/// Returns JSON: `{"ok":true}` or `{"ok":false,"error":"..."}`.
///
/// # Safety
/// `data_dir` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn homerun_server_start(data_dir: *const c_char) -> *mut c_char {
    let dir = borrow(data_dir).map(str::to_owned);
    guarded(move || match dir {
        None => r#"{"ok":false,"error":"data_dir was null or not UTF-8"}"#.to_string(),
        Some(_dir) => {
            // TODO(P3): pre-flight bind (the engine exits the process on a
            // taken port), install the panic hook that writes crash-reports/,
            // start log capture, chdir, then block on the server.
            r#"{"ok":false,"error":"not implemented"}"#.to_string()
        }
    })
}

/// Ask the running server to stop and save. Returns once it has.
#[no_mangle]
pub extern "C" fn homerun_server_stop() -> *mut c_char {
    guarded(|| r#"{"ok":false,"error":"not implemented"}"#.to_string())
}

/// `{"state":"stopped|starting|running|stopping|crashed"}`
#[no_mangle]
pub extern "C" fn homerun_server_state() -> *mut c_char {
    guarded(|| r#"{"state":"stopped"}"#.to_string())
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

/// `{"running":bool,"uptimeMs":n,"memUsedKb":n,"cpuPercent":f,"port":n}`
#[no_mangle]
pub extern "C" fn homerun_server_stats() -> *mut c_char {
    guarded(|| r#"{"running":false}"#.to_string())
}

/// `{"players":[{"name":"..","uuid":".."}],"max":n}`
#[no_mangle]
pub extern "C" fn homerun_server_players() -> *mut c_char {
    guarded(|| r#"{"players":[],"max":null}"#.to_string())
}

/// Console lines since `cursor`: `{"lines":[".."],"cursor":n}`.
///
/// The buffer is a bounded ring — a host that falls behind loses the oldest
/// lines rather than growing memory without limit.
#[no_mangle]
pub extern "C" fn homerun_server_logs_since(cursor: u64) -> *mut c_char {
    guarded(move || format!(r#"{{"lines":[],"cursor":{cursor}}}"#))
}

/// Dispatch a console command in-process (Pumpkin has no RCON).
///
/// # Safety
/// `command` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn homerun_server_command(command: *const c_char) -> *mut c_char {
    let cmd = borrow(command).map(str::to_owned);
    guarded(move || match cmd {
        None => r#"{"ok":false,"error":"command was null or not UTF-8"}"#.to_string(),
        Some(_cmd) => r#"{"ok":false,"error":"not implemented"}"#.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_is_exposed() {
        assert_eq!(homerun_abi_version(), FFI_ABI_VERSION);
    }

    #[test]
    fn freeing_null_is_a_no_op() {
        unsafe { homerun_free_string(ptr::null_mut()) };
    }

    #[test]
    fn null_input_is_an_error_not_a_crash() {
        let raw = unsafe { homerun_server_start(ptr::null()) };
        let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
        assert!(json.contains("\"ok\":false"));
        unsafe { homerun_free_string(raw) };
    }
}

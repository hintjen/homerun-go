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
//! 3. **Capture console output.** Neither platform shows stdout, and the
//!    console is a headline feature.
//! 4. **One server at a time**, enforced here as well as in the hosts. The
//!    engine keeps global state and switches worlds by process CWD.
//!
//! # Calling convention
//!
//! Every function returns a heap-allocated JSON C string that the caller
//! **must** release with [`homerun_free_string`]. JSON keeps the surface
//! narrow and versionable; a null return means allocation failed.
//!
//! Fallible calls answer `{"ok":true,...}` or `{"ok":false,"error":"..."}`.
//! Error strings are shown to players, so they are written for players.
//!
//! See `docs/ffi.md` for the full contract and host integration notes.

pub mod crash;
pub mod engine;
pub mod log_buffer;
pub mod preflight;
pub mod server;
pub mod state;

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::time::Duration;

use serde_json::json;

/// Bumped when the C surface changes shape. Hosts check it at startup.
pub const FFI_ABI_VERSION: u32 = 1;

/// How long [`homerun_server_stop`] waits for a graceful shutdown. A world
/// save can take a while on a phone; killing early risks losing it.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

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

fn out(s: String) -> *mut c_char {
    CString::new(s).map(CString::into_raw).unwrap_or(ptr::null_mut())
}

/// Run `f`, converting a panic into a JSON error rather than unwinding into
/// Swift/Kotlin (which would be undefined behaviour).
fn guarded<F: FnOnce() -> String>(f: F) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(json) => out(json),
        Err(_) => {
            let detail = crash::take_last_panic()
                .unwrap_or_else(|| "internal server panic".to_string());
            out(json!({ "ok": false, "error": detail }).to_string())
        }
    }
}

unsafe fn borrow<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

fn err(message: impl Into<String>) -> String {
    json!({ "ok": false, "error": message.into() }).to_string()
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
/// Returns `{"ok":true}` on a clean shutdown, or `{"ok":false,"error":"..."}`
/// if it could not start or crashed.
///
/// # Safety
/// `server_id` and `data_dir` must be valid NUL-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn homerun_server_start(
    server_id: *const c_char,
    data_dir: *const c_char,
    port: u16,
) -> *mut c_char {
    let id = borrow(server_id).map(str::to_owned);
    let dir = borrow(data_dir).map(str::to_owned);

    guarded(move || {
        let (Some(id), Some(dir)) = (id, dir) else {
            return err("server_id and data_dir must be valid UTF-8 strings");
        };
        let port = if port == 0 { server::DEFAULT_JAVA_PORT } else { port };

        match server::host().start(&id, &dir, port) {
            Ok(()) => json!({ "ok": true }).to_string(),
            Err(message) => err(message),
        }
    })
}

/// Ask the running server to stop and save. Returns once it has.
#[no_mangle]
pub extern "C" fn homerun_server_stop() -> *mut c_char {
    guarded(|| match server::host().stop(STOP_TIMEOUT) {
        Ok(()) => json!({ "ok": true }).to_string(),
        Err(message) => err(message),
    })
}

/// `{"state":"stopped|starting|running|stopping|crashed"}`
#[no_mangle]
pub extern "C" fn homerun_server_state() -> *mut c_char {
    guarded(|| json!({ "state": server::host().state().wire() }).to_string())
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

/// `{"running":bool,"serverId":str?,"startedAtMs":n?,"port":n?}`
#[no_mangle]
pub extern "C" fn homerun_server_stats() -> *mut c_char {
    guarded(|| {
        let (state, server_id, started_at_ms, port) = server::host().snapshot();
        json!({
            "running": state == state::ServerState::Running,
            "state": state.wire(),
            "serverId": server_id,
            "startedAtMs": started_at_ms,
            "port": port,
        })
        .to_string()
    })
}

/// `{"players":[{"name":"..","uuid":".."}],"max":n}`, or `null` when the
/// server is not running — the UI must not render a roster for a server
/// nobody can join.
#[no_mangle]
pub extern "C" fn homerun_server_players() -> *mut c_char {
    guarded(|| match server::host().players() {
        None => "null".to_string(),
        Some((players, max)) => json!({
            "players": players
                .into_iter()
                .map(|(name, uuid)| json!({ "name": name, "uuid": uuid }))
                .collect::<Vec<_>>(),
            "max": max,
        })
        .to_string(),
    })
}

/// Console lines since `cursor`: `{"lines":[".."],"cursor":n,"dropped":bool}`.
///
/// The buffer is bounded, so a host that falls far behind loses the oldest
/// lines; `dropped` says so rather than pretending the gap did not happen.
#[no_mangle]
pub extern "C" fn homerun_server_logs_since(cursor: u64) -> *mut c_char {
    guarded(move || {
        let slice = server::host().logs_since(cursor);
        json!({
            "lines": slice.lines,
            "cursor": slice.cursor,
            "dropped": slice.dropped,
        })
        .to_string()
    })
}

/// Dispatch a console command in-process (Pumpkin has no RCON).
///
/// # Safety
/// `command` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn homerun_server_command(command: *const c_char) -> *mut c_char {
    let cmd = borrow(command).map(str::to_owned);
    guarded(move || match cmd {
        None => err("command must be a valid UTF-8 string"),
        Some(cmd) => match server::host().command(&cmd) {
            Ok(()) => json!({ "ok": true }).to_string(),
            Err(message) => err(message),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Read and free a returned string, parsing it as JSON.
    fn take(raw: *mut c_char) -> Value {
        assert!(!raw.is_null(), "FFI returned null");
        let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
        unsafe { homerun_free_string(raw) };
        serde_json::from_str(&json).expect("FFI must return valid JSON")
    }

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
        let v = take(unsafe { homerun_server_start(ptr::null(), ptr::null(), 0) });
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("UTF-8"));
    }

    #[test]
    fn every_getter_returns_valid_json_when_idle() {
        // Guards against a host crashing on first poll, before any start.
        assert!(take(homerun_server_state())["state"].is_string());
        assert_eq!(take(homerun_server_stats())["running"], false);
        assert!(take(homerun_server_players()).is_null());

        let logs = take(homerun_server_logs_since(0));
        assert!(logs["lines"].is_array());
        assert!(logs["cursor"].is_number());
    }

    #[test]
    fn stopping_when_idle_reports_an_error() {
        let v = take(homerun_server_stop());
        assert_eq!(v["ok"], false);
        assert!(!v["error"].as_str().unwrap().is_empty());
    }

    #[test]
    fn commands_are_rejected_when_not_running() {
        let cmd = CString::new("say hello").unwrap();
        let v = take(unsafe { homerun_server_command(cmd.as_ptr()) });
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn error_messages_are_written_for_players() {
        let v = take(homerun_server_stop());
        let message = v["error"].as_str().unwrap();
        for jargon in ["errno", "unwrap", "panicked at", "Mutex", "null pointer"] {
            assert!(
                !message.contains(jargon),
                "player-facing error leaked {jargon:?}: {message}"
            );
        }
    }
}

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
/// Supervising a server that runs as a child process. Not iOS, which cannot.
#[cfg(feature = "process-engine")]
pub mod process_engine;
#[cfg(feature = "pumpkin-engine")]
pub mod pumpkin_engine;

pub mod server;
pub mod state;

/// Progress, cancellation and the one-at-a-time guard for a backup.
///
/// Built on every platform, engine or not: the host polls and cancels through
/// the same C surface whatever is underneath.
pub mod backup_job;

/// The linked backup engine. iOS only — see the `backup-engine` feature.
#[cfg(feature = "backup-engine")]
pub mod backup_engine;

/// What a build with no engine answers.
///
/// The C surface is declared unconditionally, so a host build or an Android
/// `.so` has to link these symbols even though it will never use them. Same
/// name as the real module, so nothing above here is cfg'd.
#[cfg(not(feature = "backup-engine"))]
mod backup_engine {
    pub fn available() -> bool {
        false
    }

    pub fn latest_snapshot(_request: &str) -> String {
        unavailable()
    }

    pub fn run(_request: &str) -> String {
        unavailable()
    }

    fn unavailable() -> String {
        serde_json::json!({
            "ok": false,
            // Written for a player, like every other error crossing this
            // boundary — a host could reach it through a mis-built app.
            "error": "This copy of Homerun cannot back up worlds.",
            "message": "built without the backup-engine feature",
            "cancelled": false,
        })
        .to_string()
    }
}

// The JVM resolves `external fun` by mangled symbol name, so Android needs an
// adapter around the C surface below. iOS links the C symbols directly.
#[cfg(target_os = "android")]
pub mod jni_bridge;

// `homerun-core`'s shared decisions. Separate from `jni_bridge` because it
// wraps something different: that adapts this crate's engine, this adapts a
// crate that knows nothing about engines.
//
// Built everywhere, because both hosts reach the same dispatch — Android via
// the JNI adapter below, iOS via `homerun_core_call`. Compiling it on the host
// too is what lets its tests run under plain `cargo test`.
pub mod core_dispatch;

#[cfg(target_os = "android")]
pub mod core_bridge;

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::time::Duration;

use serde_json::json;

/// Bumped when the C surface changes shape.
///
/// 2 added the `homerun_backup_*` calls.
///
/// 3 gave `homerun_server_start` an invocation, so a host can ask for a
/// child process instead of the linked engine.
///
/// Hosts *report* this at startup; neither of them currently compares it to
/// anything, so a stale staged library is caught by the linker (a missing
/// symbol) rather than by this. Worth wiring up properly — the failure it
/// exists to catch is a `.a` that links but decodes garbage.
pub const FFI_ABI_VERSION: u32 = 3;

/// How long [`homerun_server_stop`] waits for a graceful shutdown. A world
/// save can take a while on a phone; killing early risks losing it.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

#[no_mangle]
pub extern "C" fn homerun_abi_version() -> u32 {
    FFI_ABI_VERSION
}

/// Call into `homerun-core` — the C surface Android reaches over JNI.
///
/// `method` and `args` are NUL-terminated UTF-8; `args` is a JSON object. The
/// reply is a heap-allocated JSON string the caller **must** release with
/// [`homerun_free_string`], shaped `{"ok":true,"value":…}` or
/// `{"ok":false,"error":"…"}`.
///
/// Errors are verdicts written for players — "Homerun cannot host forge servers
/// on this device yet" — so a host should surface the text rather than reword
/// it. Fix the wording in the core, where every platform shares it.
///
/// Null is returned only if the reply could not be allocated. Nothing here
/// panics: [`core_dispatch::call`] contains its own `catch_unwind`, and a
/// panic arrives as an ordinary error envelope.
///
/// # Safety
/// `method` and `args` must be valid NUL-terminated strings for the duration
/// of the call. Passing null for either yields an error envelope rather than
/// dereferencing it.
#[no_mangle]
pub unsafe extern "C" fn homerun_core_call(
    method: *const c_char,
    args: *const c_char,
) -> *mut c_char {
    // A null or non-UTF-8 argument is the host's bug, but answering it with the
    // same envelope as any other failure keeps hosts to a single parse path.
    let read = |ptr: *const c_char| -> Option<&str> {
        if ptr.is_null() {
            return None;
        }
        CStr::from_ptr(ptr).to_str().ok()
    };

    let (Some(method), Some(args)) = (read(method), read(args)) else {
        return out(
            json!({ "ok": false, "error": "method and arguments must be UTF-8 text" }).to_string(),
        );
    };

    out(core_dispatch::call(method, args))
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
    CString::new(s)
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

/// Run `f`, converting a panic into a JSON error rather than unwinding into
/// Swift/Kotlin (which would be undefined behaviour).
fn guarded<F: FnOnce() -> String>(f: F) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(json) => out(json),
        Err(_) => {
            let detail =
                crash::take_last_panic().unwrap_or_else(|| "internal server panic".to_string());
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
/// `invocation_json` chooses **what to run**. Null or empty runs the engine
/// linked into this build, which is what iOS wants and all it can have. A JSON
/// [`process_engine::Invocation`] runs a child process instead — the program,
/// arguments and environment a host composed, because composing them is the
/// part that is genuinely platform-specific.
///
/// # Safety
/// `server_id`, `data_dir` and `invocation_json` must each be either null or a
/// valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn homerun_server_start(
    server_id: *const c_char,
    data_dir: *const c_char,
    port: u16,
    invocation_json: *const c_char,
) -> *mut c_char {
    let id = borrow(server_id).map(str::to_owned);
    let dir = borrow(data_dir).map(str::to_owned);
    let invocation = borrow(invocation_json).map(str::to_owned);

    guarded(move || {
        let (Some(id), Some(dir)) = (id, dir) else {
            return err("server_id and data_dir must be valid UTF-8 strings");
        };
        let port = if port == 0 {
            server::DEFAULT_JAVA_PORT
        } else {
            port
        };

        let engine = match invocation.as_deref().map(str::trim) {
            None | Some("") => Ok(None),
            Some(raw) => spawned_engine(raw),
        };

        let engine = match engine {
            Ok(engine) => engine,
            Err(message) => return err(message),
        };

        match server::host().start(&id, &dir, port, engine) {
            Ok(()) => json!({ "ok": true }).to_string(),
            Err(message) => err(message),
        }
    })
}

/// Build a child-process engine from a host's invocation.
///
/// Split by feature rather than answered at the call site so that a build
/// which cannot spawn says so in a sentence, rather than silently running the
/// linked engine against arguments meant for a JVM.
#[cfg(feature = "process-engine")]
fn spawned_engine(raw: &str) -> Result<Option<std::sync::Arc<dyn engine::Engine>>, String> {
    let invocation: process_engine::Invocation =
        serde_json::from_str(raw).map_err(|e| format!("bad invocation: {e}"))?;
    Ok(Some(std::sync::Arc::new(
        process_engine::ProcessEngine::new(invocation),
    )))
}

#[cfg(not(feature = "process-engine"))]
fn spawned_engine(_raw: &str) -> Result<Option<std::sync::Arc<dyn engine::Engine>>, String> {
    Err("This build cannot run a server as a separate process.".to_string())
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
            // Null for a linked engine, which has no separate process to
            // measure. A host samples this to graph what a server costs.
            "pid": server::host().pid(),
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

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------
//
// Separate entry points rather than methods on `homerun_core_call`, and the
// reason matters. That dispatch is one table compiled for both platforms, and
// every method in it is instantaneous and pure — hosts call it from the main
// thread without a second thought, because so far that has always been safe.
// A method there that opens TLS and blocks for four minutes would eventually
// be called the same way, and the app would hang with nothing to point at.
// A different symbol, with a doc comment that shouts, is the only guard the
// type system will give us.

/// Whether this build links a backup engine. 0 on Android and host builds.
#[no_mangle]
pub extern "C" fn homerun_backup_available() -> u32 {
    u32::from(backup_engine::available())
}

/// The newest snapshot, reduced to `homerun-core`'s `Snapshot` shape, or null.
///
/// Networked: seconds, not milliseconds. Do not call this on a UI thread.
///
/// # Safety
/// `request_json` must be a valid NUL-terminated UTF-8 JSON object.
#[no_mangle]
pub unsafe extern "C" fn homerun_backup_latest_snapshot(
    request_json: *const c_char,
) -> *mut c_char {
    let request = borrow(request_json).map(str::to_owned);
    guarded(move || match request {
        None => err("the backup request must be a valid UTF-8 string"),
        Some(request) => backup_engine::latest_snapshot(&request),
    })
}

/// Run one backup or restore to completion.
///
/// **Blocks for minutes**, and must run on a dedicated thread with at least an
/// 8 MB stack — the tree walk and the engine's own worker pool do not fit in
/// the 512 KB a default thread gets, and the failure mode is a stack overflow
/// with no panic report. Same rule as [`homerun_server_start`], same reason.
///
/// One at a time: a second call while one is in flight is an error, not a
/// queue. Watch it with [`homerun_backup_progress_since`].
///
/// # Safety
/// `request_json` must be a valid NUL-terminated UTF-8 JSON object.
#[no_mangle]
pub unsafe extern "C" fn homerun_backup_run(request_json: *const c_char) -> *mut c_char {
    let request = borrow(request_json).map(str::to_owned);
    guarded(move || match request {
        None => err("the backup request must be a valid UTF-8 string"),
        Some(request) => backup_engine::run(&request),
    })
}

/// Progress since `cursor`.
///
/// Cheap — a lock and a clone — and safe to call from the main thread while
/// [`homerun_backup_run`] blocks another one. Same idiom as
/// [`homerun_server_logs_since`].
///
/// `total` of 0 means "not known yet", which is most of the scanning phase.
#[no_mangle]
pub extern "C" fn homerun_backup_progress_since(cursor: u64) -> *mut c_char {
    guarded(move || {
        let progress = backup_job::job().progress_since(cursor);
        json!({
            "ok": true,
            "lines": progress.lines,
            "cursor": progress.cursor,
            "dropped": progress.dropped,
            "phase": progress.phase,
            "current": progress.current,
            "total": progress.total,
            "running": backup_job::job().is_running(),
        })
        .to_string()
    })
}

/// Ask the running backup to stop.
///
/// **Cooperative and coarse.** It takes effect at the next phase boundary and
/// cannot interrupt a transfer already inside the engine — rustic exposes no
/// cancellation hook, and unwinding out of a progress callback would panic
/// through its worker pool.
///
/// Never blocks, and is not an error when nothing is running: the caller is
/// usually a background-task expiry handler with a few seconds to live, and
/// the useful thing it can do is report the backup failed so the lease closes.
#[no_mangle]
pub extern "C" fn homerun_backup_cancel() -> *mut c_char {
    guarded(move || {
        backup_job::job().request_cancel();
        json!({ "ok": true }).to_string()
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

    /// The surface iOS links against, exercised the way Swift will use it:
    /// two C strings in, one owned C string out, freed by the caller.
    #[test]
    fn the_core_is_reachable_over_the_c_abi() {
        let method = CString::new("game.classify").unwrap();
        let args = CString::new(
            r#"{"line":"[12:00:00] [Server thread/INFO]: Done (1.0s)! For help, type \"help\""}"#,
        )
        .unwrap();

        let reply = take(unsafe { homerun_core_call(method.as_ptr(), args.as_ptr()) });
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["value"]["ready"], true);
    }

    /// Both hosts must see the same wording for the same mistake, so the C
    /// surface reports an unknown method exactly as JNI does.
    #[test]
    fn an_unknown_method_over_c_names_itself() {
        let method = CString::new("does.not.exist").unwrap();
        let args = CString::new("{}").unwrap();

        let reply = take(unsafe { homerun_core_call(method.as_ptr(), args.as_ptr()) });
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("does.not.exist"));
    }

    /// A host bug must not become a dereference of null.
    #[test]
    fn null_arguments_to_the_core_are_an_error_not_a_crash() {
        let method = CString::new("game.list").unwrap();

        for (m, a) in [
            (ptr::null(), ptr::null()),
            (method.as_ptr(), ptr::null()),
            (ptr::null(), method.as_ptr()),
        ] {
            let reply = take(unsafe { homerun_core_call(m, a) });
            assert_eq!(reply["ok"], false, "null input must be an error envelope");
        }
    }

    #[test]
    fn null_input_is_an_error_not_a_crash() {
        let v = take(unsafe { homerun_server_start(ptr::null(), ptr::null(), 0, ptr::null()) });
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

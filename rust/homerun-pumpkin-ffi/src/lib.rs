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

/// This device's own logs, for the support flow behind `get-app-logs`. Always
/// compiled: a host registers its source at launch, long before it knows
/// whether a socket will ever come up.
pub mod app_logs;
pub mod crash;
/// Where this crate's own diagnostics go on a platform that captures neither
/// stdout nor stderr. Android wires the `log` facade to logcat itself; iOS
/// registers a sink here. Always compiled, for the same reason `app_logs` is.
pub mod host_log;
/// Serving `wss://<device-fqdn>` so the dashboard can reach this device's
/// console and RCON directly. What the frames *mean* is decided in
/// `homerun_core::device_ws::protocol`, which is pure and always compiled;
/// this is the socket behind them, and it is the heaviest thing in the crate.
#[cfg(feature = "device-ws")]
pub mod device_ws;
pub mod engine;
pub mod errors;
pub mod log_buffer;
pub mod preflight;
/// Supervising a server that runs as a child process. Not iOS, which cannot.
#[cfg(feature = "process-engine")]
pub mod process_engine;
#[cfg(feature = "pumpkin-engine")]
pub mod pumpkin_engine;
#[cfg(feature = "pumpkin-engine")]
pub mod pumpkin_settings;

pub mod server;
pub mod state;

/// Progress, cancellation and the one-at-a-time guard for a backup.
///
/// Built on every platform, engine or not: the host polls and cancels through
/// the same C surface whatever is underneath.
/// What the API's settings mean to a linked engine — clamps, fallbacks and
/// player resolution, with no engine types in sight so it stays in the fast
/// test suite.
pub mod engine_settings;

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

// The same wire, for the handful of calls that need an effect the core is not
// allowed to have. It answers what it knows and delegates the rest, so both
// hosts reach it through the entry points they already call and no export
// changes.
pub mod host_dispatch;

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
/// 3 replaced `homerun_server_start`'s three scalar arguments with a single
/// JSON request so a launch can carry the player's settings, and added
/// `homerun_server_settings_preview`. This one is a genuine break: a host
/// built against 2 passes a `server_id` where 3 expects the whole request.
///
/// 4 added `invocation` to that request, so a host can ask for a **child
/// process** instead of the linked engine. Additive — a host that omits it
/// gets exactly what 3 gave it.
///
/// 5 added `homerun_server_metrics`, and with it the supervisor sampling its
/// own run — hosts stopped keeping a graph each.
///
/// 6 added `homerun_server_note` and `homerun_server_console_begin`, so a host
/// writes its own launch narrative into the one console instead of keeping a
/// second buffer for it. Additive: a host that calls neither gets exactly what
/// 5 gave it, because `start` still clears a console holding a finished run.
///
/// 7 added `homerun_device_ws_start` and `homerun_device_ws_stop`, so a device
/// can serve the dashboard's console and RCON directly. Additive: a host that
/// calls neither gets exactly what 6 gave it, and a build compiled without the
/// `device-ws` feature answers that it cannot serve one rather than failing to
/// link.
///
/// 8 added `homerun_set_app_logs_provider` and `homerun_set_log_sink`, the two
/// halves of iOS serving a device websocket: one lets the crate read this
/// app's own logs for the support flow, the other gives the crate's own
/// diagnostics somewhere to land on a platform that captures neither stdout
/// nor stderr. Additive: a host that calls neither gets exactly what 7 gave
/// it, and on Android nothing changes either way — logcat answers both.
///
/// Hosts *report* this at startup; Android also compares it
/// (`NativeServer.EXPECTED_ABI`), which is the check that catches a `.a` or
/// `.so` that links but decodes garbage.
pub const FFI_ABI_VERSION: u32 = 8;

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

    out(host_dispatch::call(method, args))
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

/// What a start call carries, once parsed.
struct StartRequest {
    server_id: String,
    data_dir: String,
    port: u16,
    settings: Option<engine_settings::EngineSettings>,
    /// What to run. Absent runs the engine linked into this build, which is
    /// what every iOS launch wants and all that platform can have; present
    /// runs a child process with the argv and environment a host composed.
    invocation: Option<serde_json::Value>,
}

/// Parse a start request.
///
/// `settings` is optional and its absence is not an error: a host that has not
/// been taught to send them starts a server on the engine's own defaults,
/// which is what every host did before this existed.
fn parse_start_request(raw: &str) -> Result<StartRequest, String> {
    let request: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("bad start request: {e}"))?;

    let text = |key: &str| -> Result<String, String> {
        request
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| format!("the start request needs {key}"))
    };

    let port = request.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    let settings = request.get("settings").filter(|v| !v.is_null()).map(|s| {
        // A name whose entry does not parse is simply not resolved, which the
        // offline path derives and the online path drops — the same handling a
        // lookup that failed already gets. Refusing the launch over it would
        // trade a wrong MOTD for no server at all.
        let resolved: Vec<homerun_core::game::Identity> = s
            .get("resolved")
            .and_then(|v| v.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| serde_json::from_value(e.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        engine_settings::resolve(
            s.get("env").unwrap_or(&serde_json::Value::Null),
            s.get("gameType").and_then(|v| v.as_str()).unwrap_or("java"),
            &resolved,
        )
    });

    Ok(StartRequest {
        invocation: request.get("invocation").filter(|v| !v.is_null()).cloned(),
        server_id: text("serverId")?,
        data_dir: text("dataDir")?,
        port: if port == 0 {
            server::DEFAULT_JAVA_PORT
        } else {
            port
        },
        settings,
    })
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
pub unsafe extern "C" fn homerun_server_start(request_json: *const c_char) -> *mut c_char {
    let request = borrow(request_json).map(str::to_owned);

    guarded(move || {
        let request = match request.as_deref().map(parse_start_request) {
            Some(Ok(request)) => request,
            Some(Err(message)) => return err(message),
            None => return err("the start request must be a valid UTF-8 string"),
        };

        let engine = match request.invocation {
            None => Ok(None),
            Some(invocation) => spawned_engine(invocation),
        };
        let engine = match engine {
            Ok(engine) => engine,
            Err(message) => return err(message),
        };

        match server::host().start(
            &request.server_id,
            &request.data_dir,
            request.port,
            request.settings,
            engine,
        ) {
            Ok(()) => json!({ "ok": true }).to_string(),
            Err(message) => err(message),
        }
    })
}

/// Build a child-process engine from a host's invocation.
///
/// Split by feature rather than answered at the call site, so a build that
/// cannot spawn says so in a sentence rather than silently running the linked
/// engine against argv meant for a JVM.
#[cfg(feature = "process-engine")]
fn spawned_engine(
    invocation: serde_json::Value,
) -> Result<Option<std::sync::Arc<dyn engine::Engine>>, String> {
    let invocation: process_engine::Invocation =
        serde_json::from_value(invocation).map_err(|e| format!("bad invocation: {e}"))?;
    Ok(Some(std::sync::Arc::new(
        process_engine::ProcessEngine::new(invocation),
    )))
}

#[cfg(not(feature = "process-engine"))]
fn spawned_engine(
    _invocation: serde_json::Value,
) -> Result<Option<std::sync::Arc<dyn engine::Engine>>, String> {
    Err("This build cannot run a server as a separate process.".to_string())
}

/// What this run has cost, oldest sample first.
///
/// Cheap — a lock and a clone — and safe from the main thread while
/// [`homerun_server_start`] blocks another one. The sampling itself happens
/// inside the supervisor for as long as a server runs, so a host does not
/// poll to *cause* a reading, only to read what is already there.
#[no_mangle]
pub extern "C" fn homerun_server_metrics() -> *mut c_char {
    guarded(|| {
        json!({
            "ok": true,
            "samples": server::host().metrics(),
        })
        .to_string()
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

/// Write a line from Homerun itself into the console.
///
/// For what a host does *around* a run — downloading a jar, restoring a world,
/// bringing the tunnel up — most of which happens before there is a run at
/// all. Those lines are the only explanation a slow launch ever gets, so they
/// belong in the console the UI pages through rather than only in an event a
/// screen that was not open never saw.
///
/// Appends only. What empties the console is
/// [`homerun_server_console_begin`], because a note is not evidence of a new
/// launch — the on-stop backup writes several after a run has ended.
///
/// # Safety
/// `line` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn homerun_server_note(line: *const c_char) -> *mut c_char {
    let line = borrow(line).map(str::to_owned);
    guarded(move || match line {
        None => err("line must be a valid UTF-8 string"),
        Some(line) => {
            server::host().push_note(line);
            json!({ "ok": true }).to_string()
        }
    })
}

/// A launch is beginning: clear the console of whatever the last one left.
///
/// Call this once, at the moment the host decides to launch — before the jar,
/// the world and the settings, all of which write into the console through
/// [`homerun_server_note`]. `homerun_server_start` is far too late to be the
/// thing that clears, because by then the interesting part has already been
/// written.
///
/// Forgetting is safe: `start` still clears a console holding a finished run.
#[no_mangle]
pub extern "C" fn homerun_server_console_begin() -> *mut c_char {
    guarded(|| {
        server::host().begin_launch();
        json!({ "ok": true }).to_string()
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
/// Where this crate's own diagnostics go.
///
/// Android needs no sink: `nativeInitLogging` wires the `log` facade to logcat.
/// iOS has no equivalent — the unified logging system is reached through
/// `os_log`, whose entry points are C macros rather than functions — so the
/// host registers one and every line this crate logs is handed to it.
///
/// Without this, every diagnostic a device websocket produces on iOS goes
/// nowhere: `println!` is not an option either, because after a launch stdout
/// *is* the pipe feeding the player-visible console. A certificate that is
/// ordered, issued, stored and never served looks identical to one that was
/// never ordered — that is a debugging round the Android port already paid for.
///
/// Called from whatever thread produced the line, including tokio workers. The
/// message is valid for the duration of the call and not afterwards. Passing
/// null unregisters, and lines are dropped rather than queued.
#[no_mangle]
pub extern "C" fn homerun_set_log_sink(sink: Option<host_log::Sink>) -> *mut c_char {
    guarded(move || {
        host_log::set(sink);
        json!({ "ok": true }).to_string()
    })
}

/// Where this app's own logs come from, on a platform this crate cannot read
/// them from itself.
///
/// Android needs no provider — logcat holds this process's entries and reading
/// them needs no permission. iOS does: its logs live in the unified logging
/// system, which only `OSLogStore` can read and only Swift can call. So the
/// host registers a function, and `get-app-logs` calls it at the moment
/// somebody asks rather than keeping a second copy of every line.
///
/// Passing null unregisters, which is what a host does when it is tearing down.
/// Registering twice replaces; there is one provider, not a list, because two
/// sources for one log is how a support flow ends up reading half of it.
///
/// The function is called from a worker thread, with a buffer that belongs to
/// this crate for the duration of the call and to nobody afterwards. It must
/// write UTF-8, at most `capacity` bytes, and answer how many — or a negative
/// number if it cannot. **It must not unwind**; a Swift or Kotlin exception
/// crossing back into Rust is undefined behaviour, exactly as a Rust panic
/// crossing out is.
#[no_mangle]
pub extern "C" fn homerun_set_app_logs_provider(
    provider: Option<app_logs::provider::Provider>,
) -> *mut c_char {
    guarded(move || {
        app_logs::provider::set(provider);
        json!({ "ok": true }).to_string()
    })
}

// The device websocket
// ---------------------------------------------------------------------------

/// Serve `wss://<device-fqdn>` on a loopback port the tunnel forwards to.
///
/// `config` is `{ port, apiUrl, jwksUrl, deviceId, fqdn?, storageDir?,
/// challengePort?, expectProxyProtocol?, acmeStaging? }`. A `port` of 0 asks
/// the OS to choose, and the answer carries **both** ports —
/// `{ ok: true, port, tlsPort }`. A host needs each for a different thing: its
/// own UI dials `port` over loopback, and the tunnel forwards the gateway's
/// `:443` to `tlsPort`. Forward the wrong one and every handshake fails, since
/// what arrives at a plaintext socket is a ClientHello.
///
/// Without `fqdn`, `storageDir` and `challengePort` there is no certificate to
/// obtain: the socket still serves, reachable through the tunnel and not by a
/// browser. `expectProxyProtocol` follows the gateway generation and defaults
/// to the legacy plane.
///
/// Builds without the `device-ws` feature answer that they cannot serve one,
/// rather than pretending to. Both phone targets have it; a host build does
/// not, which is what keeps the fast suite free of a TLS stack.
///
/// On iOS the socket lives exactly as long as the foreground does — the
/// platform suspends the process behind it, so the host brings this up when it
/// becomes active and stops it when it resigns, rather than letting the
/// listeners rot across a suspension. See `plans/ios-background-execution.md`
/// for why that limit is not a backlog item.
///
/// # Safety
/// `config` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn homerun_device_ws_start(config: *const c_char) -> *mut c_char {
    let config = borrow(config).map(str::to_owned);
    guarded(move || {
        let Some(raw) = config else {
            return err("the device websocket config must be a valid UTF-8 string");
        };

        #[cfg(not(feature = "device-ws"))]
        {
            let _ = raw;
            err("This build cannot serve a device websocket.")
        }

        #[cfg(feature = "device-ws")]
        {
            let parsed: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(value) => value,
                Err(e) => return err(format!("the device websocket config was not JSON: {e}")),
            };
            let text = |key: &str| {
                parsed
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let optional = |key: &str| {
                parsed
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            let port_of = |key: &str| {
                parsed
                    .get(key)
                    .and_then(|v| v.as_u64())
                    .filter(|p| *p > 0)
                    .map(|p| p as u16)
            };
            let config = device_ws::Config {
                port: parsed.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16,
                api_url: text("apiUrl"),
                jwks_url: text("jwksUrl"),
                device_id: text("deviceId"),
                fqdn: optional("fqdn"),
                storage_dir: optional("storageDir"),
                challenge_port: port_of("challengePort"),
                // Defaults to true: the legacy plane is what a device gets
                // today, and expecting a header that never comes is a stall a
                // log line explains, where not expecting one that does come is
                // a handshake failure that names nothing.
                expect_proxy_protocol: parsed
                    .get("expectProxyProtocol")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                acme_staging: parsed
                    .get("acmeStaging")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            };
            if config.api_url.is_empty() || config.jwks_url.is_empty() {
                return err("the device websocket needs an apiUrl and a jwksUrl");
            }
            match device_ws::start(config) {
                // Both ports, because the host needs each for a different
                // thing: `port` is what its own UI dials over loopback, and
                // `tlsPort` is what the tunnel forwards the gateway's `:443`
                // to. Reporting only one would have the gateway sending a
                // ClientHello at a plaintext socket.
                Ok(bound) => json!({
                    "ok": true,
                    "port": bound.plaintext,
                    "tlsPort": bound.tls,
                })
                .to_string(),
                Err(message) => err(message),
            }
        }
    })
}

/// Stop serving and release the port. Safe to call when nothing is running.
#[no_mangle]
pub extern "C" fn homerun_device_ws_stop() -> *mut c_char {
    guarded(|| {
        #[cfg(feature = "device-ws")]
        device_ws::stop();
        json!({ "ok": true }).to_string()
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

/// What a start request's settings would apply, without starting anything.
///
/// Exists because [`homerun_server_start`]'s arguments are otherwise only
/// observable by starting a real server, which blocks for its lifetime. A
/// misspelled key — `game_type` where the wire says `gameType` — compiles,
/// links, and yields a server on the engine's defaults with nothing anywhere
/// saying so. This is what lets a host's test catch that in milliseconds.
///
/// Pure: touches no global state and starts nothing.
///
/// # Safety
/// `request_json` must be a valid NUL-terminated UTF-8 JSON object.
#[no_mangle]
pub unsafe extern "C" fn homerun_server_settings_preview(
    request_json: *const c_char,
) -> *mut c_char {
    let request = borrow(request_json).map(str::to_owned);

    guarded(move || match request.as_deref().map(parse_start_request) {
        Some(Ok(request)) => match request.settings {
            Some(resolved) => json!({
                "ok": true,
                "settings": serde_json::to_value(&resolved).unwrap_or(serde_json::Value::Null),
                "summary": resolved.summary(),
                "advisories": resolved.advisories(),
            })
            .to_string(),
            None => json!({ "ok": true, "settings": serde_json::Value::Null }).to_string(),
        },
        Some(Err(message)) => err(message),
        None => err("the start request must be a valid UTF-8 string"),
    })
}

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

    /// Registering and unregistering a log provider are both ordinary calls.
    ///
    /// Null is how a host unregisters, and it arrives here as `None` rather
    /// than as a pointer to dereference — the shape that makes a torn-down
    /// host safe rather than lucky.
    #[test]
    fn a_log_provider_can_be_registered_and_taken_away() {
        unsafe extern "C" fn supply(_buffer: *mut c_char, _capacity: usize) -> isize {
            0
        }

        let reply = take(homerun_set_app_logs_provider(Some(supply)));
        assert_eq!(reply["ok"], true, "{reply}");

        let reply = take(homerun_set_app_logs_provider(None));
        assert_eq!(reply["ok"], true, "{reply}");
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
        let v = take(unsafe { homerun_server_start(ptr::null()) });
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("UTF-8"));

        let v = take(unsafe { homerun_server_settings_preview(ptr::null()) });
        assert_eq!(v["ok"], false);
    }

    /// A request the host got wrong must not start a server on defaults.
    ///
    /// This is the failure the JSON surface introduces that three scalars
    /// could not: a typo is now data rather than a compile error.
    #[test]
    fn a_malformed_start_request_is_refused() {
        for raw in [
            "not json at all",
            "[]",
            r#"{"dataDir":"/tmp"}"#,
            r#"{"serverId":"s1"}"#,
            r#"{"serverId":1,"dataDir":"/tmp"}"#,
        ] {
            let request = CString::new(raw).unwrap();
            let v = take(unsafe { homerun_server_start(request.as_ptr()) });
            assert_eq!(v["ok"], false, "{raw} should not have been accepted");
        }
    }

    #[test]
    fn a_preview_reports_what_a_start_would_apply() {
        let request = CString::new(
            json!({
                "serverId": "s1",
                "dataDir": "/tmp",
                "settings": {
                    "gameType": "native-crossplay",
                    "env": { "MOTD": "hi", "GAMEMODE": "creative", "MAX_PLAYERS": "8" },
                    "resolved": [],
                },
            })
            .to_string(),
        )
        .unwrap();

        let v = take(unsafe { homerun_server_settings_preview(request.as_ptr()) });
        assert_eq!(v["ok"], true);
        assert_eq!(v["settings"]["motd"], "hi");
        assert_eq!(v["settings"]["gameMode"], "creative");
        assert_eq!(v["settings"]["maxPlayers"], 8);
        // Crossplay cannot authenticate against Mojang, whatever the API says.
        assert_eq!(v["settings"]["onlineMode"], false);
        assert!(v["summary"].as_str().unwrap().contains("creative"));

        // Nothing was started: the preview is pure.
        assert_eq!(take(homerun_server_stats())["running"], false);
    }

    /// Absent settings is a real state — the host that has not been taught to
    /// send them — and must be a working start, not an error.
    #[test]
    fn a_request_without_settings_is_valid() {
        let request = CString::new(r#"{"serverId":"s1","dataDir":"/tmp","port":25565}"#).unwrap();
        let v = take(unsafe { homerun_server_settings_preview(request.as_ptr()) });
        assert_eq!(v["ok"], true);
        assert!(v["settings"].is_null());
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

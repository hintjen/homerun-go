//! Pumpkin, as a child process.
//!
//! # Why this exists rather than linking the engine
//!
//! `homerun-pumpkin-ffi` can link Pumpkin straight into the app, and on iOS it
//! must — that platform cannot spawn a process at all. Android can, and every
//! consequence of not doing so is one this app pays for:
//!
//! - An engine fault takes the **whole app** down, WebView and all. Two of the
//!   crate's non-negotiables exist to hold that line, and they can only ever
//!   hold it for panics — not for an abort inside a dependency.
//! - Memory has to be reported as the whole process, because there is no other
//!   process to measure. The number includes the browser engine.
//! - `set_current_dir` picks the world, so the choice is process-global.
//! - stdout and stderr have to be captured with a process-wide, permanent
//!   `dup2`, after which the host's own `println!` lands in the game console.
//!
//! A child process makes all four go away, and `ProcessEngine` — which already
//! supervises the JVM backend — owns the state machine, the stop ladder, the
//! console pump and real per-process metrics from `/proc`.
//!
//! # Why not Pumpkin's own binary
//!
//! Two reasons, one of them a bug.
//!
//! `pumpkin`'s `main` registers its Unix signal handlers **sequentially** — it
//! awaits `SIGINT`, and only then constructs the `SIGHUP` stream, and only then
//! `SIGTERM`. So `SIGTERM` has no handler until an interrupt has already
//! arrived, and the stop ladder's second rung — which exists precisely to be
//! gentler than `SIGKILL` — hits the default disposition and kills the server
//! **without saving the world**. That is indistinguishable from the third rung,
//! and it shows up as an occasional silent rollback rather than as an error.
//! [`spawn_signal_handlers`] is the fix, and it is the main reason this file is
//! not just upstream's `main.rs`.
//!
//! The second reason is smaller: cargo does not build a dependency's binaries,
//! so shipping upstream's would mean building from a checkout beside this repo
//! rather than from a rev this repo pins. The rev is pinned on purpose —
//! upstream tracks Minecraft protocol releases, and that churn must not land in
//! an app build uninvited.
//!
//! # What the host relies on
//!
//! **The readiness line.** `ProcessEngine` reaches `on_ready` only through
//! `homerun_core::minecraft::console::is_ready`, which matches Pumpkin's
//! `Server is now running.`. Do not reword it here without changing that, or a
//! launch will sit in `starting` until it times out, with a healthy server
//! accepting players behind it.
//!
//! **The working directory.** Pumpkin reads `pumpkin.toml` and `data/` from the
//! process CWD, and `ProcessEngine` sets that to the server's own directory. No
//! path is passed and none is parsed; there are no arguments at all.

use std::{
    backtrace::{Backtrace, BacktraceStatus},
    panic::PanicHookInfo,
    process::exit,
    sync::{atomic::Ordering, OnceLock},
    thread::{self, ThreadId},
    time::Instant,
};

use pumpkin::{
    crash::{CrashReport, FullBacktrace},
    data::VanillaData,
    stop_or_exit_server, stop_server, PumpkinServer, CRASH_REPORT, SERVER_EXIT_CODE,
    SERVER_IS_STOPPING,
};
use pumpkin_config::{LoadConfiguration, PumpkinConfig};
use pumpkin_data::packet::CURRENT_MC_VERSION;
use tracing::{info, warn};

use homerun_core::game::Identity;
use homerun_pumpkin_ffi::{engine_settings, pumpkin_settings};

/// What the host leaves in the server directory for this process to read.
///
/// The three raw inputs, not a rendered config: resolving them is
/// [`engine_settings::resolve`]'s job and it is already tested, and doing it
/// here rather than in Kotlin means no wire key or enum spelling is written
/// twice. Absent means the player configured nothing, which is a real state —
/// not an error.
const SETTINGS_FILE: &str = "homerun-settings.json";

static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();

#[tokio::main]
async fn main() {
    MAIN_THREAD
        .set(thread::current().id())
        .expect("the main thread id is set once, here");

    std::panic::set_hook(Box::new(handle_panic));

    let started = Instant::now();

    // The host prepared this directory and `ProcessEngine` made it the CWD.
    let exec_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            // The logger does not exist yet, so this is the only way to say it.
            eprintln!("[Homerun] could not read the working directory: {err}");
            exit(1);
        }
    };

    // Reads `pumpkin.toml`, writing a default one if it is absent and merging
    // in any key the file is missing. Unlike the linked engine, which overrides
    // in memory *after* this, our values go through `validate()` — so a bad one
    // aborts this process rather than the app.
    let mut config = PumpkinConfig::load(&exec_dir);
    let mut vanilla_data = VanillaData::load();

    pumpkin::init_logger(&config.advanced);

    info!(
        "Starting Pumpkin for Minecraft (protocol {})",
        CURRENT_MC_VERSION.protocol_version()
    );

    // Before the server is built: `PumpkinServer::new` snapshots the MOTD and
    // the player cap into the status response it will serve, so anything
    // applied afterwards is invisible to a client looking at the server list.
    apply_host_settings(&exec_dir, &mut config, &mut vanilla_data);

    // Before the server exists. A stop can arrive while a world is still being
    // generated, and that is exactly when it takes longest to become safe.
    spawn_signal_handlers();

    // The bind decision is ours here, unlike in the linked engine: a taken port
    // should end this process, and only this process.
    let server = match PumpkinServer::new(config.basic, config.advanced, vanilla_data).await {
        Ok(server) => server,
        Err(err) => {
            tracing::error!("The server could not start: {err}");
            exit(1);
        }
    };

    let plugin_wait = server.init_plugins().await;
    info!(
        "Started server; took {}ms",
        started.elapsed().saturating_sub(plugin_wait).as_millis()
    );

    // **Load-bearing** — this is what the host watches for to call `on_ready`;
    // see the module docs. The address printed is Pumpkin's own, which is the
    // honest one: the host asked for a port, but the config had the last word.
    info!(
        "Server is now running. Connect using port: Java Edition: {}",
        server.server.advanced_config.networking.java.address
    );

    server.start().await;

    info!("The server has stopped.");
    exit(SERVER_EXIT_CODE.load(Ordering::Acquire));
}

/// Apply what the player configured, if the host left anything.
///
/// Everything here is shared with the linked engine — same resolver, same
/// clamps, same assignment onto Pumpkin's types — so the two deployments
/// cannot come to disagree about what a setting means.
///
/// **Nothing in here fails a launch.** A missing, unreadable or malformed file
/// leaves the engine on its own configuration and says so loudly, because that
/// is the state worth shouting about: Pumpkin's defaults include
/// `online_mode = true`, and a server nobody can join looks exactly like a
/// server that started fine.
fn apply_host_settings(
    exec_dir: &std::path::Path,
    config: &mut PumpkinConfig,
    vanilla_data: &mut VanillaData,
) {
    let path = exec_dir.join(SETTINGS_FILE);
    if !path.exists() {
        warn!("[Homerun] No settings were supplied — the engine's own configuration applies.");
        return;
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            warn!("[Homerun] Could not read the settings ({err}); the engine's own apply.");
            return;
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!("[Homerun] Could not parse the settings ({err}); the engine's own apply.");
            return;
        }
    };

    let env = parsed.get("env").cloned().unwrap_or(serde_json::Value::Null);
    let game_type = parsed
        .get("gameType")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    // Names the host could not resolve are simply absent, and `resolve` drops
    // the entries that need one — an unresolvable operator is not a reason to
    // refuse to start.
    let resolved: Vec<Identity> = parsed
        .get("resolved")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let settings = engine_settings::resolve(&env, game_type, &resolved);

    // Onto the console, where the player is already looking, rather than into
    // a log nobody on a phone can reach.
    //
    // Both already carry the badge — `homerun-core` writes it into the lines
    // it hands back, because the desktop puts them straight into its log.
    // Adding a second produced "[Homerun] [Homerun] Settings applied…" in
    // front of a player, which is the same mistake `ServerHost.note` records
    // having made once already.
    info!("{}", settings.summary());
    for advisory in settings.advisories() {
        warn!("{advisory}");
    }

    pumpkin_settings::apply(&settings, config);
    pumpkin_settings::apply_lists(&settings, config, vanilla_data);
}

/// Listen for every stop signal at once.
///
/// Upstream awaits them in sequence, which leaves `SIGTERM` unhandled until a
/// `SIGINT` has already been received — see the module docs. Each signal gets
/// its own task so no one of them can gate another, and a stream that cannot be
/// registered is reported rather than taken as fatal: losing one signal is
/// survivable, and the stop ladder has a rung below it.
#[cfg(unix)]
fn spawn_signal_handlers() {
    use tokio::signal::unix::{signal, SignalKind};

    for (name, kind) in [
        ("SIGINT", SignalKind::interrupt()),
        ("SIGHUP", SignalKind::hangup()),
        ("SIGTERM", SignalKind::terminate()),
    ] {
        tokio::spawn(async move {
            match signal(kind) {
                Ok(mut stream) => {
                    if stream.recv().await.is_some() {
                        warn!("Received {name}; stopping the server and saving the world...");
                        stop_or_exit_server();
                    }
                }
                Err(err) => warn!("Could not listen for {name}: {err}"),
            }
        });
    }
}

#[cfg(not(unix))]
fn spawn_signal_handlers() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            warn!("Received Ctrl-C; stopping the server and saving the world...");
            stop_or_exit_server();
        }
    });
}

/// Pumpkin's own crash reporting, kept.
///
/// `ProcessEngine` has no crash capture — the crate's panic hook is in-process
/// and sees nothing a child does, so without this a crash reports only its exit
/// code. This writes a real report with a backtrace into the server's own
/// directory, which is strictly more than the linked engine offers.
fn handle_panic(panic_info: &PanicHookInfo<'_>) {
    let crash_report = {
        // Captured here rather than inside `CrashReport` so the trace does not
        // open with the constructor that made it.
        let captured = Backtrace::capture();
        let full = if captured.status() == BacktraceStatus::Captured {
            FullBacktrace::Captured
        } else {
            FullBacktrace::ForceCaptured(Backtrace::force_capture())
        };
        CrashReport::new(panic_info, captured, full)
    };

    let payload = panic_info.payload();
    let described = || {
        payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<unknown>")
    };

    if is_main_thread() {
        // Nothing can be shut down gracefully from here, but the report can
        // still be written.
        if let Some(report) = try_set_crash_report(crash_report) {
            report.print_to_console();
            report.save_and_log();
            tracing::error!("Aborting: the main thread panicked.");
        } else {
            tracing::error!(
                "The main thread panicked while stopping; aborting: {}",
                described()
            );
        }
        exit(1);
    }

    if try_set_crash_report(crash_report).is_some() {
        stop_server();
    } else {
        tracing::error!("Panicked while shutting down: {}", described());
    }
}

fn is_main_thread() -> bool {
    Some(&thread::current().id()) == MAIN_THREAD.get()
}

/// `Some` on the first panic, which is the one worth reporting. `None`
/// afterwards, so a cascade cannot overwrite the cause with a consequence.
fn try_set_crash_report(report: CrashReport) -> Option<&'static CrashReport> {
    if !SERVER_IS_STOPPING.load(Ordering::Acquire) && CRASH_REPORT.set(report).is_ok() {
        CRASH_REPORT.get()
    } else {
        None
    }
}

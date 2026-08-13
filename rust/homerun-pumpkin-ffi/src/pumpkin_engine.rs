//! The real engine: Pumpkin, driven as a library.
//!
//! Behind the `pumpkin-engine` feature so the rest of the crate keeps building
//! and testing in a couple of seconds on any machine, with no Pumpkin, no
//! device, and no wasmtime. That is not a convenience — it is why the FFI
//! surface could be reviewed and unit-tested before the fork was pinned.
//!
//! Everything here is the embedding logic the fork's own `ios.rs` works out,
//! reimplemented against Pumpkin's public API because that module is
//! `#[cfg(target_os = "ios")]` and unreachable from a host or Android build.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pumpkin::command::CommandSender;
use pumpkin::server::Server;
use pumpkin::{init_logger, reset_server_state, stop_server, PumpkinServer};

use crate::engine::{Engine, PlayerEntry, Roster, RunOutcome, RunRequest, StopSignal};
use crate::pumpkin_settings;

/// The running server, reachable while `run` is blocked inside the runtime.
static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

/// The configured player cap. Captured before the config is moved into the
/// server, which is the only chance to read it.
static MAX_PLAYERS: AtomicU32 = AtomicU32::new(0);

#[derive(Clone)]
struct Active {
    server: Arc<Server>,
    /// Commands and player queries arrive on other threads; they need a way
    /// back into the runtime that `run` owns.
    runtime: tokio::runtime::Handle,
}

fn active() -> Option<Active> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub struct PumpkinEngine;

impl PumpkinEngine {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PumpkinEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for PumpkinEngine {
    fn run(
        &self,
        request: &RunRequest,
        stop: StopSignal,
        on_line: &dyn Fn(String),
        on_ready: &dyn Fn(),
    ) -> RunOutcome {
        // Pumpkin keeps its stop state in process-wide statics. Without this a
        // second run would observe the previous run's stop flag and exit
        // immediately, which reads as "the server won't start".
        reset_server_state();

        // Console output reaches us as bytes on fd 1: Pumpkin logs through
        // `tracing` to stdout, which on a phone goes nowhere. The capture is
        // process-wide and permanent, so it is installed once and writes
        // straight to the console buffer rather than through `on_line`.
        capture::start();

        // Emitted directly rather than through the capture: the engine's own
        // logger is not installed yet, so without this the console is empty
        // for as long as world loading takes and the app looks stuck.
        on_line(format!(
            "[Homerun] starting {} on port {}",
            request.server_id, request.java_port
        ));

        // The engine selects a world by process working directory. This is
        // also why only one server runs at a time.
        if let Err(e) = std::env::set_current_dir(&request.data_dir) {
            let message = format!("Homerun could not open this server's files ({e}).");
            on_line(message.clone());
            return RunOutcome::Crashed(message);
        }

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let message = format!("The server could not start ({e}).");
                on_line(message.clone());
                return RunOutcome::Crashed(message);
            }
        };

        let outcome = runtime.block_on(async {
            let exec_dir = match std::env::current_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    return RunOutcome::Crashed(format!(
                        "Homerun could not open this server's files ({e})."
                    ));
                }
            };

            // Creates the default configuration on first run, and is where a
            // restored `pumpkin.toml` from another device arrives. The managed
            // settings are overridden below; everything else is the player's.
            let mut config = pumpkin_settings::load_config(&exec_dir, on_line);
            config
                .advanced
                .networking
                .java
                .address
                .set_port(request.java_port);

            // There is no stdin on a phone. Leaving the console enabled makes
            // `start()` block waiting for a readline that can never arrive.
            config.advanced.commands.use_console = false;

            let mut vanilla_data = pumpkin_settings::load_data(on_line);

            // After the config is loaded and before the server is built:
            // `PumpkinServer::new` snapshots the MOTD and the player cap into
            // its status response, so a setting applied after this point would
            // be live but invisible to the server list.
            if let Some(settings) = &request.settings {
                pumpkin_settings::apply(settings, &mut config);
                pumpkin_settings::apply_lists(settings, &config, &mut vanilla_data);
            }

            MAX_PLAYERS.store(
                config.advanced.networking.java.max_players,
                Ordering::Relaxed,
            );
            init_logger(&config.advanced);

            let server = match PumpkinServer::new(config.basic, config.advanced, vanilla_data).await
            {
                Ok(server) => server,
                Err(e) => {
                    // Reachable because the fork returns this rather than
                    // calling process::exit — which would take the whole
                    // app down with no report.
                    let message = describe_bind_failure(&e, request.java_port);
                    on_line(message.clone());
                    return RunOutcome::Crashed(message);
                }
            };

            *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Active {
                server: server.server.clone(),
                runtime: tokio::runtime::Handle::current(),
            });

            server.init_plugins().await;

            // Bridge our stop signal to Pumpkin's, which is a separate pair of
            // statics plus a cancellation token. Without this, `stop` would be
            // recorded and nothing would act on it.
            let watcher = stop.clone();
            let watchdog = tokio::spawn(async move {
                while !watcher.should_stop() {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                stop_server();
            });

            // The listener was bound inside `new`, and `start` is the accept
            // loop — so this is the first moment a player could actually
            // connect. Announced before `start` because that call blocks for
            // the rest of the server's life.
            on_ready();

            server.start().await;
            watchdog.abort();
            server.unload_plugins().await;

            RunOutcome::Stopped
        });

        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        outcome
    }

    fn command(&self, command: &str) -> Result<(), String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("Type a command to run.".to_string());
        }

        let Some(active) = active() else {
            return Err("The server is not running.".to_string());
        };

        let server = active.server.clone();
        let owned = command.to_string();

        // Called from the host's thread, which is outside the runtime — so
        // block_on is safe here. The reply is written to the console rather
        // than returned; the UI reads it from the log.
        active.runtime.block_on(async move {
            let dispatcher = server.command_dispatcher.read().await;
            // Two dispatchers exist with a `handle_command`; the one behind
            // this field takes a resolved `CommandSource`. Mirrors how the
            // engine dispatches its own console input.
            let source = CommandSender::Console.into_source(&server).await;
            dispatcher.handle_command(&source, &owned).await;
        });

        Ok(())
    }

    fn players(&self) -> Option<Roster> {
        let active = active()?;
        let max = MAX_PLAYERS.load(Ordering::Relaxed);

        let players: Vec<PlayerEntry> = active
            .server
            .get_all_players()
            .iter()
            .map(|p| {
                (
                    p.gameprofile.name.clone(),
                    Some(p.gameprofile.id.to_string()),
                )
            })
            .collect();

        Some((players, (max > 0).then_some(max)))
    }

    /// Every entry above carries `gameprofile.id`, so a stats report never
    /// needs `list uuids` from this engine — and must not use it. Pumpkin's
    /// console renders each player through `commands.list.nameAndId`, which
    /// its translation table cannot find (the lookup is lowercased, the key
    /// is not), so the reply names every player as the literal string
    /// `minecraft:commands.list.nameandid`. The header parses, the line is
    /// withheld from the console as a recognised reply, and the roster is
    /// empty. There is no symptom.
    fn roster_is_authoritative(&self) -> bool {
        true
    }
}

/// Bind failures are the common way a start fails, and the message is shown to
/// a player — so it names the port and says what to do, not what errno was.
fn describe_bind_failure(error: &std::io::Error, port: u16) -> String {
    match error.kind() {
        std::io::ErrorKind::AddrInUse => format!(
            "Port {port} is already being used by something else on this device. \
             Stop that and try again."
        ),
        std::io::ErrorKind::PermissionDenied => {
            format!("This device would not let Homerun use port {port}.")
        }
        _ => format!("The server could not start on port {port}."),
    }
}

/// stdout/stderr capture.
///
/// Pumpkin writes its console through `tracing` to fd 1, and there is no hook
/// to redirect that to a callback — so the fds themselves are replaced with a
/// pipe and a reader thread turns the bytes back into lines.
///
/// The reader thread outlives any single run, because replacing fd 1 is
/// process-wide and cannot be undone. It therefore writes to the crate's own
/// console buffer — which is equally process-wide and lives for the life of
/// the app — rather than borrowing `run`'s `on_line`. Handing a thread a
/// borrow that only lives as long as one call would be a dangling pointer the
/// moment the run ended.
mod capture {
    use std::io::{BufRead, BufReader};
    use std::os::unix::io::FromRawFd;
    use std::sync::OnceLock;

    static STARTED: OnceLock<()> = OnceLock::new();

    /// Remove terminal colour codes.
    ///
    /// The engine's logger assumes a terminal and wraps every line in ANSI
    /// escapes. The console this feeds is a WebView, which renders them as
    /// literal garbage — `[2m` and friends — in front of every message.
    /// Stripping here rather than in each host keeps it fixed once.
    fn strip_ansi(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut chars = line.chars();

        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            // CSI: ESC '[' … final byte in @-~. Anything else after ESC is a
            // shorter sequence whose next character is the whole of it.
            // Anything else after ESC is a shorter sequence whose next
            // character is the whole of it, and `next()` has consumed it.
            if chars.next() == Some('[') {
                for tail in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&tail) {
                        break;
                    }
                }
            }
        }
        out
    }

    unsafe extern "C" {
        fn pipe(fds: *mut i32) -> i32;
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }

    /// Install the redirect. Safe to call on every run; only the first does
    /// anything.
    pub fn start() {
        STARTED.get_or_init(|| {
            let mut fds = [0i32; 2];
            unsafe {
                if pipe(fds.as_mut_ptr()) != 0 {
                    return;
                }
                dup2(fds[1], 1);
                dup2(fds[1], 2);
                close(fds[1]);
            }

            let read_end = fds[0];
            std::thread::Builder::new()
                .name("homerun-console".into())
                .spawn(move || {
                    let reader = BufReader::new(unsafe { std::fs::File::from_raw_fd(read_end) });
                    for line in reader.lines().map_while(Result::ok) {
                        crate::server::host().push_log(strip_ansi(&line));
                    }
                })
                .ok();
        });
    }
}

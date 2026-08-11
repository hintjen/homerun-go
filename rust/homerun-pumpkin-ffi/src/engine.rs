//! The seam between this crate's infrastructure and the actual server.
//!
//! Everything else here — state, log buffering, pre-flight, crash capture —
//! is ours and testable on any machine. Only `Engine::run` needs Pumpkin, so
//! it lives behind a trait. That keeps the FFI surface reviewable and tested
//! before the fork is pinned, and lets tests drive failure paths (a crash
//! mid-run, a slow shutdown) that would be painful to provoke for real.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What the host asked for.
pub struct RunRequest {
    pub server_id: String,
    /// Working directory. The engine keeps global state and distinguishes
    /// worlds by CWD, which is why only one server runs at a time.
    pub data_dir: String,
    pub java_port: u16,
    /// The player's settings, already resolved into what an engine can take.
    ///
    /// `None` means "apply nothing", which is what a host that has not been
    /// taught to send them does. That is the dangerous state rather than the
    /// harmless one — the engine's own defaults are not the player's choices,
    /// and for Pumpkin that includes authenticating against Mojang — so
    /// [`crate::server::ServerHost::start`] says so on the console rather than
    /// leaving it silent.
    pub settings: Option<crate::engine_settings::EngineSettings>,
}

/// Signals a running engine to shut down. Shared with the stop path.
#[derive(Clone, Default)]
pub struct StopSignal(Arc<AtomicBool>);

impl StopSignal {
    pub fn request_stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn should_stop(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A connected player: display name and UUID (absent in offline mode).
pub type PlayerEntry = (String, Option<String>);

/// Who is connected, and the server's player cap.
pub type Roster = (Vec<PlayerEntry>, Option<u32>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Shut down cleanly after a stop request.
    Stopped,
    /// Exited on its own — a crash, or a fatal config error.
    Crashed(String),
}

/// A Minecraft server implementation.
///
/// `run` blocks for the server's whole lifetime and must return rather than
/// exit the process: on a phone `process::exit` takes the entire app down
/// with no report.
pub trait Engine: Send + Sync {
    /// Block until the server stops. Emit console output through `on_line`,
    /// and call `on_ready` once — and only once — the server is actually
    /// accepting connections.
    ///
    /// `on_ready` is what moves the host from `Starting` to `Running`. The
    /// host cannot infer that moment: `run` blocks from the first instant, so
    /// without this signal the only options are to report running immediately
    /// (telling a player to join a world that is still generating) or never.
    /// An engine that returns without calling it never started.
    fn run(
        &self,
        request: &RunRequest,
        stop: StopSignal,
        on_line: &dyn Fn(String),
        on_ready: &dyn Fn(),
    ) -> RunOutcome;

    /// Dispatch a console command. Pumpkin has no RCON, so this goes
    /// in-process; the reply comes back as console output.
    fn command(&self, command: &str) -> Result<(), String>;

    /// Currently connected players, if the engine can report them.
    fn players(&self) -> Option<Roster>;

    /// What this server is costing right now: resident KiB, and cumulative
    /// CPU seconds since it started.
    ///
    /// **Counters, never a rate.** A percentage is a difference between two
    /// moments, and `homerun_core::metrics` is what turns two of these into
    /// one — computing it here would put that arithmetic back in the place
    /// this crate spent a day taking it out of.
    ///
    /// `None` from an engine that shares this process: measuring it would
    /// measure the app, the WebView included, and report it as the server.
    fn usage(&self) -> Option<(u64, f64)> {
        None
    }

    /// The operating-system process this server is, if it is one.
    ///
    /// A linked engine has none to give — it *is* this process — and the
    /// default says so. A host uses it to sample what the server costs, which
    /// it cannot do for an engine sharing its own address space.
    fn pid(&self) -> Option<u32> {
        None
    }
}

/// Engine used until the Pumpkin fork is pinned, and by the tests.
///
/// Behaves like a real server: reports startup, honours stop requests, and
/// can be told to fail so the failure paths are covered.
pub struct StubEngine {
    pub fail_with: Option<String>,
}

impl StubEngine {
    pub fn healthy() -> Self {
        Self { fail_with: None }
    }

    pub fn failing(reason: impl Into<String>) -> Self {
        Self {
            fail_with: Some(reason.into()),
        }
    }
}

impl Engine for StubEngine {
    fn run(
        &self,
        request: &RunRequest,
        stop: StopSignal,
        on_line: &dyn Fn(String),
        on_ready: &dyn Fn(),
    ) -> RunOutcome {
        if let Some(reason) = &self.fail_with {
            // Deliberately without calling on_ready: a failed start must never
            // reach Running, and this is the path that proves it.
            on_line(format!("[Homerun] server failed: {reason}"));
            return RunOutcome::Crashed(reason.clone());
        }

        on_line(format!(
            "[Homerun] starting {} on port {}",
            request.server_id, request.java_port
        ));
        on_line("[Homerun] Done! For help, type \"help\"".to_string());
        on_ready();

        while !stop.should_stop() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        on_line("[Homerun] stopping, saving world".to_string());
        RunOutcome::Stopped
    }

    fn command(&self, command: &str) -> Result<(), String> {
        if command.trim().is_empty() {
            return Err("empty command".to_string());
        }
        Ok(())
    }

    fn players(&self) -> Option<Roster> {
        Some((Vec::new(), Some(20)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn request() -> RunRequest {
        RunRequest {
            server_id: "s1".into(),
            data_dir: ".".into(),
            java_port: 25565,
            settings: None,
        }
    }

    #[test]
    fn a_stop_request_ends_the_run() {
        let engine = StubEngine::healthy();
        let stop = StopSignal::default();
        let lines = Arc::new(Mutex::new(Vec::new()));

        let stopper = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            stopper.request_stop();
        });

        let sink = lines.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let ready_flag = ready.clone();
        let outcome = engine.run(
            &request(),
            stop,
            &move |line| {
                sink.lock().unwrap().push(line);
            },
            &move || ready_flag.store(true, Ordering::SeqCst),
        );

        assert!(matches!(outcome, RunOutcome::Stopped));
        assert!(
            ready.load(Ordering::SeqCst),
            "a server that came up must announce it, or the host never leaves Starting"
        );
        let captured = lines.lock().unwrap();
        assert!(captured.iter().any(|l| l.contains("Done!")));
        assert!(captured.iter().any(|l| l.contains("saving world")));
    }

    #[test]
    fn a_failing_engine_returns_rather_than_exiting() {
        let engine = StubEngine::failing("port 25565 already in use");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();

        let ready = Arc::new(AtomicBool::new(false));
        let ready_flag = ready.clone();
        let outcome = engine.run(
            &request(),
            StopSignal::default(),
            &move |line| {
                sink.lock().unwrap().push(line);
            },
            &move || ready_flag.store(true, Ordering::SeqCst),
        );

        match outcome {
            RunOutcome::Crashed(reason) => assert!(reason.contains("25565")),
            RunOutcome::Stopped => panic!("expected a crash outcome"),
        }
        assert!(
            !ready.load(Ordering::SeqCst),
            "a failed start must never reach Running — the UI would invite players to a dead server"
        );
        // The reason has to reach the console too, or the UI shows a server
        // that stopped for no visible reason.
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("failed")));
    }

    #[test]
    fn a_stop_signal_can_be_reused_across_runs() {
        let stop = StopSignal::default();
        stop.request_stop();
        assert!(stop.should_stop());
        stop.reset();
        assert!(!stop.should_stop(), "restart would stop immediately");
    }
}

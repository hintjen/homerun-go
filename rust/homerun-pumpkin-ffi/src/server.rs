//! Composes the pieces into the one server this device hosts.
//!
//! Owns the global the FFI functions talk to. Everything here is
//! synchronised because the host calls in from several threads: the server
//! runs on its own (16 MB stack) thread while the UI polls stats and logs
//! from the main one.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crash;
use crate::engine::{Engine, Roster, RunOutcome, RunRequest, StopSignal, StubEngine};
use crate::log_buffer::{LogBuffer, LogSlice};
use crate::preflight;
use crate::state::{ServerState, ServerStatus};

pub const DEFAULT_JAVA_PORT: u16 = 25565;

struct Inner {
    status: ServerStatus,
    logs: LogBuffer,
    stop: StopSignal,
}

pub struct ServerHost {
    inner: Mutex<Inner>,
    engine: Box<dyn Engine>,
}

static HOST: OnceLock<ServerHost> = OnceLock::new();

/// The process-wide host. Uses the stub engine until Pumpkin is pinned.
pub fn host() -> &'static ServerHost {
    HOST.get_or_init(|| ServerHost::new(Box::new(StubEngine::healthy())))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ServerHost {
    pub fn new(engine: Box<dyn Engine>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: ServerStatus::idle(),
                logs: LogBuffer::default(),
                stop: StopSignal::default(),
            }),
            engine,
        }
    }

    /// Recover rather than propagate: a poisoned lock means some other
    /// thread panicked, and refusing to report state afterwards would turn
    /// one failure into a permanently unusable app.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push_log(&self, line: impl Into<String>) {
        self.lock().logs.push(line);
    }

    pub fn logs_since(&self, cursor: u64) -> LogSlice {
        self.lock().logs.since(cursor)
    }

    pub fn state(&self) -> ServerState {
        self.lock().status.state
    }

    pub fn snapshot(&self) -> (ServerState, Option<String>, Option<u64>, Option<u16>) {
        let inner = self.lock();
        (
            inner.status.state,
            inner.status.server_id.clone(),
            inner.status.started_at_ms,
            inner.status.port,
        )
    }

    pub fn players(&self) -> Option<Roster> {
        // Only meaningful while running; otherwise the UI would render a
        // roster for a server nobody can join.
        if self.state() != ServerState::Running {
            return None;
        }
        self.engine.players()
    }

    /// Start a server. Blocks for its whole lifetime — the host must call
    /// this on a dedicated thread with at least a 16 MB stack.
    pub fn start(&self, server_id: &str, data_dir: &str, port: u16) -> Result<(), String> {
        {
            let mut inner = self.lock();
            if inner.status.state.is_active() {
                let running = inner.status.server_id.clone().unwrap_or_default();
                return Err(if running == server_id {
                    format!("Server {server_id} is already running")
                } else {
                    // Phrased for a player: this is a product rule, not a bug.
                    "Another server is already running. Stop it first — this device can host one at a time.".to_string()
                });
            }
            inner.status.server_id = Some(server_id.to_string());
            inner.status.transition(ServerState::Starting)?;
            // A new run must not replay the previous one's console.
            inner.logs.clear();
            inner.stop.reset();
        }

        crash::set_crash_dir(data_dir);
        crash::install_hook();
        // Discard any panic recorded before this run. The crash path below
        // uses the last panic to explain *why* the engine died, and a stale
        // message from an earlier run would attribute the wrong cause.
        let _ = crash::take_last_panic();

        // Before the engine gets a chance to exit the process over a taken port.
        if let Err(message) = preflight::check_ports(port, None) {
            let mut inner = self.lock();
            inner.logs.push(format!("[Homerun] {message}"));
            let _ = inner.status.transition(ServerState::Stopped);
            return Err(message);
        }

        let request = RunRequest {
            server_id: server_id.to_string(),
            data_dir: data_dir.to_string(),
            java_port: port,
        };

        let stop = self.lock().stop.clone();

        {
            let mut inner = self.lock();
            inner.status.transition(ServerState::Running)?;
            inner.status.started_at_ms = Some(now_ms());
            inner.status.port = Some(port);
        }

        let outcome = self.engine.run(&request, stop, &|line| self.push_log(line));

        let mut inner = self.lock();
        match outcome {
            RunOutcome::Stopped => {
                // The engine may have returned on its own, without a stop
                // request having moved us to Stopping.
                if inner.status.state == ServerState::Running {
                    inner.status.transition(ServerState::Stopping)?;
                }
                inner.status.transition(ServerState::Stopped)?;
                Ok(())
            }
            RunOutcome::Crashed(reason) => {
                // A panic recorded during this run is the more specific
                // cause; otherwise report what the engine told us.
                let detail = match crash::take_last_panic() {
                    Some(panic) => format!("{reason} ({panic})"),
                    None => reason,
                };
                inner
                    .logs
                    .push(format!("[Homerun] server stopped: {detail}"));
                inner.status.transition(ServerState::Crashed)?;
                Err(detail)
            }
        }
    }

    /// Ask the server to stop and save. Returns once it has, or on timeout.
    pub fn stop(&self, timeout: std::time::Duration) -> Result<(), String> {
        {
            let mut inner = self.lock();
            match inner.status.state {
                ServerState::Stopped | ServerState::Crashed => {
                    return Err("Server is not running".to_string())
                }
                ServerState::Running => inner.status.transition(ServerState::Stopping)?,
                // Already stopping, or still starting — signalling is enough.
                _ => {}
            }
            inner.stop.request_stop();
        }

        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if !self.state().is_active() {
                return Ok(());
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
        Err("Server did not stop in time".to_string())
    }

    pub fn command(&self, command: &str) -> Result<(), String> {
        if self.state() != ServerState::Running {
            return Err("Server is not running".to_string());
        }
        self.engine.command(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn temp_dir() -> String {
        let dir = std::env::temp_dir().join(format!(
            "homerun-server-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    /// Port 0 is never bindable as a listener target here, so tests pick a
    /// real free port to avoid colliding with anything on the machine.
    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    #[test]
    fn a_full_lifecycle_reports_the_expected_states() {
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();
        let port = free_port();

        assert_eq!(host.state(), ServerState::Stopped);

        let runner = host.clone();
        let run_dir = dir.clone();
        let handle = thread::spawn(move || runner.start("s1", &run_dir, port));

        // Wait for it to come up.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.state() != ServerState::Running {
            assert!(std::time::Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(10));
        }

        let (_, id, started, reported_port) = host.snapshot();
        assert_eq!(id.as_deref(), Some("s1"));
        assert!(started.is_some());
        assert_eq!(reported_port, Some(port));

        host.stop(Duration::from_secs(5)).unwrap();
        handle.join().unwrap().unwrap();

        assert_eq!(host.state(), ServerState::Stopped);
        // Port and uptime must not survive the stop.
        let (_, _, started_after, port_after) = host.snapshot();
        assert!(started_after.is_none());
        assert!(port_after.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_port_is_an_error_not_a_process_exit() {
        let host = ServerHost::new(Box::new(StubEngine::healthy()));
        let dir = temp_dir();

        let held = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = held.local_addr().unwrap().port();

        let err = host.start("s1", &dir, port).unwrap_err();
        assert!(err.contains(&port.to_string()));
        // Recoverable: the user stops the other server and retries.
        assert_eq!(host.state(), ServerState::Stopped);
        assert!(host
            .logs_since(0)
            .lines
            .iter()
            .any(|l| l.contains("in use")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_server_is_refused_with_a_readable_message() {
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();
        let port = free_port();

        let runner = host.clone();
        let run_dir = dir.clone();
        let handle = thread::spawn(move || runner.start("s1", &run_dir, port));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.state() != ServerState::Running {
            assert!(std::time::Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(10));
        }

        let err = host.start("s2", &dir, free_port()).unwrap_err();
        assert!(err.contains("one at a time"), "got: {err}");

        host.stop(Duration::from_secs(5)).unwrap();
        handle.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_is_reported_and_leaves_the_host_restartable() {
        let host = ServerHost::new(Box::new(StubEngine::failing("world corrupted")));
        let dir = temp_dir();

        let err = host.start("s1", &dir, free_port()).unwrap_err();
        assert!(err.contains("world corrupted"));
        assert_eq!(host.state(), ServerState::Crashed);
        assert!(host.state().can_transition_to(ServerState::Starting));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_is_not_blamed_on_an_older_unrelated_panic() {
        let _guard = crash::test_guard();
        // A panic anywhere earlier in the process used to leak into the next
        // crash's message, sending whoever read it after the wrong cause.
        crash::install_hook();
        let _ = std::panic::catch_unwind(|| panic!("something unrelated, much earlier"));

        let host = ServerHost::new(Box::new(StubEngine::failing("world corrupted")));
        let dir = temp_dir();
        let err = host.start("s1", &dir, free_port()).unwrap_err();

        assert!(err.contains("world corrupted"), "got: {err}");
        assert!(!err.contains("unrelated"), "stale panic leaked into: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stopping_an_idle_server_is_an_error_not_a_hang() {
        let host = ServerHost::new(Box::new(StubEngine::healthy()));
        assert!(host.stop(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn commands_are_rejected_unless_running() {
        let host = ServerHost::new(Box::new(StubEngine::healthy()));
        assert!(host.command("say hi").is_err());
    }

    #[test]
    fn a_restart_does_not_replay_the_previous_console() {
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();

        for _ in 0..2 {
            let port = free_port();
            let runner = host.clone();
            let run_dir = dir.clone();
            let handle = thread::spawn(move || runner.start("s1", &run_dir, port));

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while host.state() != ServerState::Running {
                assert!(std::time::Instant::now() < deadline, "never started");
                thread::sleep(Duration::from_millis(10));
            }
            host.stop(Duration::from_secs(5)).unwrap();
            handle.join().unwrap().unwrap();
        }

        // One run's worth of "starting" lines, not two.
        let starts = host
            .logs_since(0)
            .lines
            .iter()
            .filter(|l| l.contains("starting"))
            .count();
        assert_eq!(starts, 1, "console should be cleared between runs");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

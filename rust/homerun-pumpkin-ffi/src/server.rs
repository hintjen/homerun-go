//! Composes the pieces into the one server this device hosts.
//!
//! Owns the global the FFI functions talk to. Everything here is
//! synchronised because the host calls in from several threads: the server
//! runs on its own (16 MB stack) thread while the UI polls stats and logs
//! from the main one.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crash;
// `test` as well as the ungated build: every test here drives the stub, so
// without it a `--features pumpkin-engine` test run does not compile.
#[cfg(any(test, not(feature = "pumpkin-engine")))]
use crate::engine::StubEngine;
use crate::engine::{Engine, Roster, RunOutcome, RunRequest, StopSignal};
use crate::log_buffer::{LogBuffer, LogSlice};
use crate::preflight;
use crate::state::{ServerState, ServerStatus};

pub const DEFAULT_JAVA_PORT: u16 = 25565;

struct Inner {
    status: ServerStatus,
    logs: LogBuffer,
    /// The console holds a run that has already finished.
    ///
    /// Only consulted by `start`, and only as a safety net: a host that
    /// announced its launch through [`ServerHost::begin_launch`] has already
    /// cleared the console and written into it, and `start` must leave that
    /// alone. A launch is minutes of jar downloads and world restores before
    /// there is a process, and that is exactly the part a player wants
    /// explained when a start is slow.
    console_holds_finished_run: bool,
    stop: StopSignal,
    /// What this run has cost, sampled while it runs.
    ///
    /// One history per run: a graph covers a session, so a restart starts a
    /// new one rather than continuing the last. The retention rule, the rate
    /// arithmetic and the judgement about when a number cannot be trusted are
    /// all `homerun_core::metrics` — this only takes the readings.
    metrics: homerun_core::metrics::History,

    /// The engine running right now, if one is.
    ///
    /// Per-run rather than per-host because a device may host either kind:
    /// a linked engine, or a child process. Which one is a property of the
    /// server being started, not of the app that is starting it.
    engine: Option<Arc<dyn Engine>>,
}

pub struct ServerHost {
    /// Shared rather than owned so the sampler thread can hold the state for
    /// as long as a run lasts, without borrowing the host it belongs to.
    inner: Arc<Mutex<Inner>>,
    /// The engine that is compiled in — Pumpkin, or the stub. Used when a
    /// start names no other, which is every iOS launch.
    linked: Arc<dyn Engine>,
}

static HOST: OnceLock<ServerHost> = OnceLock::new();

/// The process-wide host.
///
/// Runs the real server when the `pumpkin-engine` feature is on — the app
/// builds enable it. Without it the stub stands in, which is what keeps the
/// test suite fast and device-free.
pub fn host() -> &'static ServerHost {
    HOST.get_or_init(|| {
        #[cfg(feature = "pumpkin-engine")]
        {
            ServerHost::new(Box::new(crate::pumpkin_engine::PumpkinEngine::new()))
        }
        #[cfg(not(feature = "pumpkin-engine"))]
        {
            ServerHost::new(Box::new(StubEngine::healthy()))
        }
    })
}

/// A running sampler, stopped when its run ends.
struct Sampling {
    done: Arc<StopSignal>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Sampling {
    fn stop(mut self) {
        self.done.request_stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
            inner: Arc::new(Mutex::new(Inner {
                status: ServerStatus::idle(),
                logs: LogBuffer::default(),
                console_holds_finished_run: false,
                stop: StopSignal::default(),
                metrics: homerun_core::metrics::History::new(Default::default()),
                engine: None,
            })),
            linked: Arc::from(engine),
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

    /// A line from Homerun rather than from the server.
    ///
    /// Jar downloads, runtime unpacking, world restores, the tunnel — the
    /// things a host does *around* a run, most of them before there is a run
    /// at all. They belong in the console because they are the only account of
    /// why a launch took two minutes, and a console opened after the fact
    /// should still show it.
    ///
    /// Appends, and never clears. A note is not evidence that a new launch has
    /// begun — the on-stop backup writes several *after* a run has ended, and
    /// treating the first of those as a new launch wiped the console of the
    /// run the player had just watched stop. [`begin_launch`] is the boundary.
    ///
    /// [`begin_launch`]: ServerHost::begin_launch
    pub fn push_note(&self, line: impl Into<String>) {
        self.lock().logs.push(line);
    }

    /// A launch is beginning: everything the console holds belongs to the last
    /// one and goes now.
    ///
    /// The host announces this because only the host knows it. A launch starts
    /// minutes before `start` is called — a jar to fetch, a world to restore —
    /// and those minutes are exactly what the console should be showing, so
    /// `start` is far too late to be the thing that empties it.
    ///
    /// Forgetting to call this is safe: `start` still clears a console holding
    /// a finished run, which is the behaviour every host had before this
    /// existed. The cost of forgetting is losing that launch's own notes, not
    /// showing the previous run's.
    pub fn begin_launch(&self) {
        let mut inner = self.lock();
        inner.logs.clear();
        inner.console_holds_finished_run = false;
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

    /// The process this run is, if it is one. See `Engine::pid`.
    pub fn pid(&self) -> Option<u32> {
        self.lock().engine.clone()?.pid()
    }

    pub fn players(&self) -> Option<Roster> {
        // Only meaningful while running; otherwise the UI would render a
        // roster for a server nobody can join.
        if self.state() != ServerState::Running {
            return None;
        }
        self.lock().engine.clone()?.players()
    }

    /// Start a server. Blocks for its whole lifetime — the host must call
    /// this on a dedicated thread with at least a 16 MB stack.
    /// Start a server, blocking for its whole lifetime.
    ///
    /// `settings` is what the player configured, applied before the server
    /// comes up. `engine` is *what to run*: absent means the engine linked
    /// into this build — every iOS launch, and the tests — while an
    /// [`Invocation`] means a child process, which only a build with
    /// `process-engine` can honour.
    ///
    /// [`Invocation`]: crate::process_engine::Invocation
    pub fn start(
        &self,
        server_id: &str,
        data_dir: &str,
        port: u16,
        settings: Option<crate::engine_settings::EngineSettings>,
        engine: Option<Arc<dyn Engine>>,
    ) -> Result<(), String> {
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
            // A new run must not replay the previous one's console. The safety
            // net rather than the rule: a host that announced its launch has
            // already cleared this and written its narrative into it, and
            // wiping that here would throw away the only record of what those
            // minutes were spent on. `begin_launch` holds the other half.
            if inner.console_holds_finished_run {
                inner.logs.clear();
                inner.console_holds_finished_run = false;
            }
            inner.stop.reset();
            inner.engine = Some(engine.unwrap_or_else(|| Arc::clone(&self.linked)));
            inner.metrics = homerun_core::metrics::History::new(Default::default());
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

        // On the console before the engine starts, because this is the only
        // record of what a launch was told. A server whose game mode is wrong
        // is otherwise a conversation; with this line it is a glance.
        {
            let mut inner = self.lock();
            match &settings {
                Some(resolved) => {
                    inner.logs.push(resolved.summary());
                    for line in resolved.advisories() {
                        inner.logs.push(line);
                    }
                }
                None => inner.logs.push(
                    "[Homerun] No settings were supplied — the engine's own configuration applies."
                        .to_string(),
                ),
            }
        }

        let request = RunRequest {
            server_id: server_id.to_string(),
            data_dir: data_dir.to_string(),
            java_port: port,
            settings,
        };

        let stop = self.lock().stop.clone();

        // Running is announced by the engine, not assumed here. `run` blocks
        // from its first instant, so a host that flipped the state before
        // calling it would tell a player to join a world that is still
        // generating — which is exactly what happened before the engine grew
        // this signal.
        let on_ready = || {
            let mut inner = self.lock();
            if inner.status.state == ServerState::Starting
                && inner.status.transition(ServerState::Running).is_ok()
            {
                inner.status.started_at_ms = Some(now_ms());
                inner.status.port = Some(port);
            }
        };

        // Cloned out of the lock: `run` blocks for the whole life of the
        // server, and holding the mutex across that would freeze every getter
        // the UI polls.
        let engine = self
            .lock()
            .engine
            .clone()
            .ok_or_else(|| "no engine for this run".to_string())?;

        // Sampling runs beside the server for as long as it lives. It is here
        // rather than in a host because the supervisor is the only thing that
        // knows *which* process to measure — and because three hosts each
        // keeping their own graph is how they ended up covering three
        // different spans of time.
        let sampler = self.start_sampling(Arc::clone(&engine));

        let outcome = engine.run(&request, stop, &|line| self.push_log(line), &on_ready);
        sampler.stop();

        let mut inner = self.lock();
        // The run is over; nothing should be able to reach its stdin or ask it
        // who is playing.
        inner.engine = None;
        // What is in the console now belongs to a run that has ended. It stays
        // readable — the crash reason below is written *into* it, and a player
        // looking at why their server stopped is the whole point — until the
        // next launch speaks or starts, whichever comes first.
        inner.console_holds_finished_run = true;
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

    /// The graph of this run, oldest first.
    pub fn metrics(&self) -> Vec<homerun_core::metrics::Sample> {
        self.lock().metrics.samples().to_vec()
    }

    /// Take a reading every so often, for as long as the run lasts.
    ///
    /// The interval is the core's and is re-read each pass: it doubles once
    /// the graph is full, and a sampler still scheduling on the original would
    /// keep paying to read `/proc` at a resolution the core has stopped
    /// keeping.
    fn start_sampling(&self, engine: Arc<dyn Engine>) -> Sampling {
        let done = Arc::new(StopSignal::default());
        let finished = Arc::clone(&done);
        let inner = Arc::clone(&self.inner);

        let handle = thread::spawn(move || loop {
            if finished.should_stop() {
                return;
            }

            let (mem, cpu) = match engine.usage() {
                Some((mem, cpu)) => (Some(mem), Some(cpu)),
                // An engine with nothing to report still anchors the clock;
                // the graph renders "unavailable" rather than a fabricated
                // zero.
                None => (None, None),
            };
            // Straight from the engine rather than through the host's own
            // getter, which refuses unless the state is Running — a sample
            // taken while a world is still generating is worth having.
            let players = engine.players().map(|(players, _)| players.len() as u32);

            let interval = {
                let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
                inner.metrics.record(homerun_core::metrics::Reading {
                    at_ms: now_ms() as i64,
                    mem_used_kb: mem,
                    cpu_seconds: cpu,
                    player_count: players,
                });
                inner.metrics.interval_ms()
            };

            // Slept in slices so a stop is noticed promptly rather than half
            // an hour later, once the interval has grown.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(interval);
            while std::time::Instant::now() < deadline {
                if finished.should_stop() {
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(200));
            }
        });

        Sampling {
            done,
            handle: Some(handle),
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
        let engine = self
            .lock()
            .engine
            .clone()
            .ok_or_else(|| "Server is not running".to_string())?;
        engine.command(command)
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

    /// Takes a while to come up, like a real server generating a world.
    struct SlowEngine {
        ready_after: Duration,
    }

    impl Engine for SlowEngine {
        fn run(
            &self,
            _request: &RunRequest,
            stop: StopSignal,
            on_line: &dyn Fn(String),
            on_ready: &dyn Fn(),
        ) -> RunOutcome {
            on_line("generating world".to_string());
            std::thread::sleep(self.ready_after);
            on_ready();
            while !stop.should_stop() {
                std::thread::sleep(Duration::from_millis(5));
            }
            RunOutcome::Stopped
        }

        fn command(&self, _command: &str) -> Result<(), String> {
            Ok(())
        }

        fn players(&self) -> Option<Roster> {
            None
        }
    }

    /// The bug this pins: the host used to flip to Running immediately before
    /// calling the engine, so the UI told players to join a world that was
    /// still generating. Only the engine knows when it is actually up.
    /// The point of the whole exercise: a child process goes through the same
    /// state machine, console buffer and crash handling as the linked engine,
    /// because the supervisor cannot tell them apart.
    #[cfg(feature = "process-engine")]
    #[test]
    fn a_child_process_is_supervised_by_the_same_host() {
        use crate::process_engine::{Invocation, ProcessEngine};

        let host = ServerHost::new(Box::new(StubEngine::healthy()));
        let dir = temp_dir();
        let port = free_port();

        let mut env = std::collections::BTreeMap::new();
        env.insert("HOMERUN_FAKE_SERVER".to_string(), "ready".to_string());
        let engine: Arc<dyn Engine> = Arc::new(ProcessEngine::new(Invocation {
            program: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".into(),
                "process_engine::tests::i_am_the_fake_server".into(),
                "--nocapture".into(),
                "--ignored".into(),
            ],
            env,
        }));

        let host = Arc::new(host);
        let runner = {
            let (host, dir) = (Arc::clone(&host), dir.clone());
            thread::spawn(move || host.start("s1", &dir, port, None, Some(engine)))
        };

        // Running is announced by the console, not by the process existing.
        let mut waited = 0;
        while host.state() != ServerState::Running && waited < 100 {
            thread::sleep(Duration::from_millis(50));
            waited += 1;
        }
        assert_eq!(host.state(), ServerState::Running, "never reached running");

        // The roster the console built, through the supervisor rather than
        // from the engine directly.
        let (players, max) = host.players().expect("a running server has a roster");
        assert_eq!(players.first().map(|p| p.0.as_str()), Some("Notch"));
        assert_eq!(max, Some(7));

        // And the console reached the buffer the UI pages through.
        let slice = host.logs_since(0);
        assert!(
            slice.lines.iter().any(|l| l.contains("Done (")),
            "the console did not reach the log buffer: {:?}",
            slice.lines
        );

        // Sampled while it ran, by the supervisor rather than by a host.
        let graph = host.metrics();
        assert!(!graph.is_empty(), "the run was never sampled");

        // The readings themselves come from `/proc`, which exists on the
        // platform this ships to and not on the one it is usually written on.
        // Everything above is asserted everywhere; this part is only true
        // where there is a `/proc` to read, and is covered on device.
        #[cfg(unix)]
        assert!(
            graph.iter().any(|s| s.mem_used_mb.is_some()),
            "a child process must report memory: {graph:?}"
        );

        host.command("stop").expect("stdin must be reachable");
        host.stop(Duration::from_secs(30)).expect("it must stop");
        runner
            .join()
            .unwrap()
            .expect("a clean stop is not an error");
        assert_eq!(host.state(), ServerState::Stopped);
    }
    #[test]
    fn a_slow_start_stays_starting_until_the_engine_is_ready() {
        // `start` clears the process-global last-panic slot, so any test
        // that starts a server races `crash`'s tests for it.
        let _guard = crash::test_guard();
        let host = Arc::new(ServerHost::new(Box::new(SlowEngine {
            ready_after: Duration::from_millis(300),
        })));
        let dir = temp_dir();
        let port = free_port();

        let runner = {
            let host = host.clone();
            std::thread::spawn(move || host.start("slow", &dir, port, None, None))
        };

        // Well inside the engine's startup window.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            host.state(),
            ServerState::Starting,
            "reported Running while the world was still generating"
        );

        // And it does get there once the engine says so.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.state() != ServerState::Running && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(host.state(), ServerState::Running);

        host.stop(Duration::from_secs(5)).unwrap();
        runner.join().unwrap().unwrap();
    }

    #[test]
    fn a_full_lifecycle_reports_the_expected_states() {
        // `start` clears the process-global last-panic slot, so any test
        // that starts a server races `crash`'s tests for it.
        let _guard = crash::test_guard();
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();
        let port = free_port();

        assert_eq!(host.state(), ServerState::Stopped);

        let runner = host.clone();
        let run_dir = dir.clone();
        let handle = thread::spawn(move || runner.start("s1", &run_dir, port, None, None));

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

    /// Every bug report includes the console. "It says survival and my server
    /// is creative" has to be a two-second diagnosis, so what a launch was
    /// told is on the console before the engine starts.
    #[test]
    fn a_launch_records_the_settings_it_was_given() {
        // `start` clears the process-global last-panic slot, so any test
        // that starts a server races `crash`'s tests for it.
        let _guard = crash::test_guard();
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();
        let port = free_port();

        let settings = crate::engine_settings::resolve(
            &serde_json::json!({
                "GAMEMODE": "creative",
                "MAX_PLAYERS": "8",
                "DIFFICULTY": "hard",
            }),
            "java",
            &[],
        );

        let runner = host.clone();
        let run_dir = dir.clone();
        let handle =
            thread::spawn(move || runner.start("s1", &run_dir, port, Some(settings), None));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.state() != ServerState::Running {
            assert!(std::time::Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(10));
        }

        let console = host.logs_since(0).lines.join("\n");
        assert!(console.contains("creative"), "{console}");
        assert!(console.contains("8 players"), "{console}");
        // And what it could not honour, which is the only place a player is
        // told that a setting they chose went nowhere.
        assert!(console.contains("difficulty"), "{console}");

        host.stop(Duration::from_secs(5)).unwrap();
        handle.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host that has not been taught to send settings is the state worth
    /// naming: the engine's own defaults are not the player's choices.
    #[test]
    fn a_launch_without_settings_says_so() {
        // `start` clears the process-global last-panic slot, so any test
        // that starts a server races `crash`'s tests for it.
        let _guard = crash::test_guard();
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();
        let port = free_port();

        let runner = host.clone();
        let run_dir = dir.clone();
        let handle = thread::spawn(move || runner.start("s1", &run_dir, port, None, None));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.state() != ServerState::Running {
            assert!(std::time::Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(10));
        }

        let console = host.logs_since(0).lines.join("\n");
        assert!(console.contains("No settings were supplied"), "{console}");

        host.stop(Duration::from_secs(5)).unwrap();
        handle.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_port_is_an_error_not_a_process_exit() {
        // `start` clears the process-global last-panic slot, so any test
        // that starts a server races `crash`'s tests for it.
        let _guard = crash::test_guard();
        let host = ServerHost::new(Box::new(StubEngine::healthy()));
        let dir = temp_dir();

        let held = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = held.local_addr().unwrap().port();

        let err = host.start("s1", &dir, port, None, None).unwrap_err();
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
        // `start` clears the process-global last-panic slot, so any test
        // that starts a server races `crash`'s tests for it.
        let _guard = crash::test_guard();
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();
        let port = free_port();

        let runner = host.clone();
        let run_dir = dir.clone();
        let handle = thread::spawn(move || runner.start("s1", &run_dir, port, None, None));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.state() != ServerState::Running {
            assert!(std::time::Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(10));
        }

        let err = host.start("s2", &dir, free_port(), None, None).unwrap_err();
        assert!(err.contains("one at a time"), "got: {err}");

        host.stop(Duration::from_secs(5)).unwrap();
        handle.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_is_reported_and_leaves_the_host_restartable() {
        // `start` clears the process-global last-panic slot, so any test
        // that starts a server races `crash`'s tests for it.
        let _guard = crash::test_guard();
        let host = ServerHost::new(Box::new(StubEngine::failing("world corrupted")));
        let dir = temp_dir();

        let err = host.start("s1", &dir, free_port(), None, None).unwrap_err();
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
        let err = host.start("s1", &dir, free_port(), None, None).unwrap_err();

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
        // `start` clears the process-global last-panic slot, so any test
        // that starts a server races `crash`'s tests for it.
        let _guard = crash::test_guard();
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();

        for _ in 0..2 {
            let port = free_port();
            let runner = host.clone();
            let run_dir = dir.clone();
            let handle = thread::spawn(move || runner.start("s1", &run_dir, port, None, None));

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

    /// The reason `start` no longer clears unconditionally.
    ///
    /// A launch is minutes of downloading a jar and restoring a world before
    /// there is a process to have a console. Those lines are the only account
    /// of where the time went, and clearing at `start` deleted them at exactly
    /// the moment they became worth reading.
    #[test]
    fn a_launch_keeps_the_notes_it_wrote_before_starting() {
        let _guard = crash::test_guard();
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();
        let port = free_port();

        host.begin_launch();
        host.push_note("[Homerun] Downloading the server jar…");
        host.push_note("[Homerun] Restoring the world from a backup…");

        let runner = host.clone();
        let run_dir = dir.clone();
        let handle = thread::spawn(move || runner.start("s1", &run_dir, port, None, None));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.state() != ServerState::Running {
            assert!(std::time::Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(10));
        }
        host.stop(Duration::from_secs(5)).unwrap();
        handle.join().unwrap().unwrap();

        let lines = host.logs_since(0).lines;
        assert!(
            lines.iter().any(|l| l.contains("Downloading the server jar")),
            "the launch narrative was wiped by start: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Restoring the world")),
            "the launch narrative was wiped by start: {lines:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half: keeping notes through `start` must not resurrect the
    /// *previous* run. Announcing the launch is what empties the console.
    #[test]
    fn announcing_a_launch_clears_the_last_run() {
        let _guard = crash::test_guard();
        let host = Arc::new(ServerHost::new(Box::new(StubEngine::healthy())));
        let dir = temp_dir();
        let port = free_port();

        let runner = host.clone();
        let run_dir = dir.clone();
        let handle = thread::spawn(move || runner.start("s1", &run_dir, port, None, None));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.state() != ServerState::Running {
            assert!(std::time::Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(10));
        }
        host.stop(Duration::from_secs(5)).unwrap();
        handle.join().unwrap().unwrap();

        // Still readable while nothing else is happening: this is what a
        // player reads to find out why their server stopped.
        assert!(!host.logs_since(0).lines.is_empty(), "a finished run's console must survive it");

        // The on-stop backup runs for minutes after the JVM is gone and writes
        // as it goes. Those lines belong to the run that just ended, so they
        // must **append**. Treating the first of them as a new launch wiped
        // the console of the run the player had just watched stop.
        host.push_note("[Backup] Backing up the world…");
        let during_backup = host.logs_since(0).lines;
        assert!(
            during_backup.iter().any(|l| l.contains("stopping, saving world")),
            "the on-stop backup wiped the run it belongs to: {during_backup:?}"
        );

        host.begin_launch();
        host.push_note("[Homerun] Downloading the server jar…");

        let lines = host.logs_since(0).lines;
        assert_eq!(
            lines.iter().filter(|l| l.contains("starting")).count(),
            0,
            "the new launch is replaying the last run's console: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("Downloading the server jar")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

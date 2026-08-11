//! An [`Engine`] that supervises a **child process**.
//!
//! The other implementation runs a server inside this process because iOS
//! cannot spawn one. This one is for the platforms that can, and it exists so
//! that supervising a real Minecraft server — reading its console, asking it
//! to stop, deciding what its exit meant — stops being written once per host.
//!
//! Android had all of this in Kotlin: a `ProcessBuilder`, a `readLine` pump, a
//! roster built from console lines, and a stop escalation. None of it was
//! Android-specific except the argv, and none of it could be tested without a
//! device. Here it is one implementation, and the tests below drive it against
//! a real child process on whatever machine you are sitting at.
//!
//! # What the host still supplies
//!
//! The [`Invocation`] — program, arguments, environment. Building it *is*
//! platform work: which `libjvm.so` to load, what `LD_LIBRARY_PATH` a
//! Termux-built runtime needs, where a temp directory may live. This engine
//! takes it as data and never composes one.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use homerun_core::minecraft::{console, jvm};
use serde::{Deserialize, Serialize};

use crate::engine::{Engine, PlayerEntry, Roster, RunOutcome, RunRequest, StopSignal};

/// Everything needed to start the server, decided by the host.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invocation {
    /// The executable. On Android this is the launcher inside the APK, which
    /// is the only place the platform will exec from.
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Added to the inherited environment, not replacing it. `LD_LIBRARY_PATH`
    /// has to be set before the process starts — the linker reads it at exec,
    /// so there is no setting it afterwards.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A Minecraft server running as a child process.
pub struct ProcessEngine {
    invocation: Invocation,
    /// The live run. Held so `command` can reach stdin and `players` can read
    /// the roster the console pump is building.
    run: Mutex<Option<Live>>,
    /// How to climb out of a stop. Always the core's in production — the
    /// override exists so the tests do not sit through the real 30-second
    /// save grace on every `cargo test`, which would be thirty seconds added
    /// to a suite that otherwise finishes in under one.
    ladder: Vec<jvm::Rung>,
}

struct Live {
    stdin: Option<ChildStdin>,
    roster: Arc<Mutex<RosterState>>,
    pid: u32,
}

#[derive(Default)]
struct RosterState {
    /// Insertion-ordered: the UI shows them in the order they arrived, and a
    /// set that reordered on every join would be visible churn.
    players: Vec<String>,
    max: Option<u32>,
}

impl ProcessEngine {
    pub fn new(invocation: Invocation) -> Self {
        Self {
            invocation,
            run: Mutex::new(None),
            ladder: jvm::stop_ladder(true),
        }
    }

    /// The same engine on a ladder of your choosing. Tests only: a production
    /// host must not get to shorten the window a world save is given.
    #[cfg(test)]
    fn with_ladder(invocation: Invocation, ladder: Vec<jvm::Rung>) -> Self {
        Self {
            invocation,
            run: Mutex::new(None),
            ladder,
        }
    }

    /// Recover from a poisoned lock rather than propagate. A panic elsewhere
    /// should not make the app permanently unable to report who is online.
    fn live(&self) -> std::sync::MutexGuard<'_, Option<Live>> {
        self.run.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Engine for ProcessEngine {
    fn run(
        &self,
        request: &RunRequest,
        stop: StopSignal,
        on_line: &dyn Fn(String),
        on_ready: &dyn Fn(),
    ) -> RunOutcome {
        let mut command = Command::new(&self.invocation.program);
        command
            .args(&self.invocation.args)
            .current_dir(&request.data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &self.invocation.env {
            command.env(key, value);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            // Never reached `on_ready`, so this is a launch that did not
            // happen rather than a server that died.
            Err(err) => {
                return RunOutcome::Crashed(format!(
                    "could not start {}: {err}",
                    self.invocation.program
                ))
            }
        };

        let roster = Arc::new(Mutex::new(RosterState::default()));
        *self.live() = Some(Live {
            stdin: child.stdin.take(),
            roster: Arc::clone(&roster),
            pid: child.id(),
        });

        // stderr on its own thread, merged into the same console. A server
        // that only complains on stderr — a bad JVM flag, a missing class —
        // would otherwise fail silently as far as the player can see.
        let stderr = child.stderr.take().map(|stderr| {
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
            });
            rx
        });

        // The stop watcher: `run` is busy reading stdout for the whole life of
        // the server, so climbing the ladder has to happen from somewhere
        // else. It takes the child's pid rather than the child itself, because
        // waiting on the child belongs to this thread alone.
        let watcher = spawn_stop_watcher(&child, stop.clone(), self.ladder.clone());

        let mut ready = false;
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(err) = stderr.as_ref() {
                    for line in err.try_iter() {
                        observe(&line, &roster);
                        on_line(line);
                    }
                }

                observe(&line, &roster);
                // The console saying it is accepting connections is the only
                // honest signal for this; the process existing is not one.
                if !ready && console::is_ready(&line) {
                    ready = true;
                    on_ready();
                }
                on_line(line);
            }
        }

        // The pipe closed, which means the process is on its way out.
        let status = child.wait();
        watcher.finish();
        *self.live() = None;

        // Whatever stderr had left to say, now that stdout is done.
        if let Some(err) = stderr {
            for line in err.try_iter() {
                on_line(line);
            }
        }

        match status {
            // Intent is not decided here. `homerun-core::lifecycle` owns
            // crashed-versus-stopped, and it reads a stop request the host
            // recorded — this only reports what the process did.
            Ok(status) if stop.should_stop() || status.success() => RunOutcome::Stopped,
            Ok(status) => RunOutcome::Crashed(match status.code() {
                Some(code) => format!("the server exited with code {code}"),
                None => "the server was terminated".to_string(),
            }),
            Err(err) => RunOutcome::Crashed(format!("could not wait for the server: {err}")),
        }
    }

    fn command(&self, command: &str) -> Result<(), String> {
        let mut live = self.live();
        let stdin = live
            .as_mut()
            .and_then(|run| run.stdin.as_mut())
            .ok_or_else(|| jvm::Refusal::NotAcceptingCommands.text().to_string())?;

        writeln!(stdin, "{command}")
            .and_then(|_| stdin.flush())
            .map_err(|_| jvm::Refusal::NotAcceptingCommands.text().to_string())
    }

    fn pid(&self) -> Option<u32> {
        self.live().as_ref().map(|run| run.pid)
    }

    fn players(&self) -> Option<Roster> {
        let live = self.live();
        let roster = live.as_ref()?.roster.lock().ok()?;
        let players: Vec<PlayerEntry> = roster
            .players
            .iter()
            // Console lines carry a name and never a UUID. Offline-mode
            // servers have none to give, and inventing one would be worse
            // than admitting it is unknown.
            .map(|name| (name.clone(), None))
            .collect();
        Some((players, roster.max))
    }
}

/// Fold one console line into the roster.
fn observe(line: &str, roster: &Arc<Mutex<RosterState>>) {
    let Ok(mut roster) = roster.lock() else {
        return;
    };

    if let Some(name) = console::joined(line) {
        if !roster.players.iter().any(|p| p == name) {
            roster.players.push(name.to_string());
        }
    }
    if let Some(name) = console::left(line) {
        roster.players.retain(|p| p != name);
    }
    if let Some(max) = console::max_players(line) {
        roster.max = Some(max);
    }
}

/// Watches for a stop request and climbs the ladder until the process goes.
struct StopWatcher {
    done: Arc<StopSignal>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StopWatcher {
    /// Called once the process has exited, so the watcher stops climbing a
    /// ladder against a pid that may since have been reused.
    fn finish(mut self) {
        self.done.request_stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_stop_watcher(child: &Child, stop: StopSignal, ladder: Vec<jvm::Rung>) -> StopWatcher {
    let pid = child.id();
    let done = Arc::new(StopSignal::default());
    let finished = Arc::clone(&done);

    let handle = std::thread::spawn(move || {
        // Poll rather than block: this has to notice both the stop request and
        // the process ending on its own, and only one of those is a signal we
        // are given.
        while !stop.should_stop() {
            if finished.should_stop() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        for rung in ladder {
            if finished.should_stop() {
                return;
            }
            match rung.action {
                // The console rung is the host's: it writes `stop` on stdin
                // through `Engine::command`, because that is the same path a
                // player's console command takes. Waiting for it is this
                // watcher's job.
                jvm::Action::Console => {}
                jvm::Action::Terminate => terminate(pid),
                jvm::Action::Kill => kill(pid),
            }

            let deadline = Instant::now() + Duration::from_millis(rung.wait_ms);
            while Instant::now() < deadline {
                if finished.should_stop() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    });

    StopWatcher {
        done,
        handle: Some(handle),
    }
}

/// Ask the process to exit — the rung before the last one.
#[cfg(unix)]
fn terminate(pid: u32) {
    // SAFETY: `kill` with a pid we spawned and a valid signal. The worst a
    // reaped pid can do is return ESRCH, which is ignored.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn terminate(pid: u32) {
    // Windows has no SIGTERM. The ladder collapses to its last rung, which is
    // what `taskkill` without /F cannot promise anyway.
    kill(pid);
}

#[cfg(unix)]
fn kill(pid: u32) {
    // SAFETY: as above.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in Minecraft server: this test binary, re-invoked.
    ///
    /// Spawning a real program is the point — a mock would test the mock — and
    /// `current_exe` is the one program guaranteed to exist and to behave the
    /// same on every machine this is developed on.
    fn fake_server(script: &str) -> Invocation {
        let mut env = BTreeMap::new();
        env.insert("HOMERUN_FAKE_SERVER".to_string(), script.to_string());
        Invocation {
            program: std::env::current_exe()
                .expect("the test binary must be locatable")
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".into(),
                "process_engine::tests::i_am_the_fake_server".into(),
                "--nocapture".into(),
                "--ignored".into(),
            ],
            env,
        }
    }

    /// Not a test. This is the fake server's body, run only when the parent
    /// asked for it by environment variable; `cargo test` skips it otherwise
    /// because it is ignored.
    #[test]
    #[ignore = "spawned as a child by the tests below"]
    fn i_am_the_fake_server() {
        let Ok(script) = std::env::var("HOMERUN_FAKE_SERVER") else {
            return;
        };

        match script.as_str() {
            "ready" => {
                println!("[12:00:00] [main/INFO]: max-players=7");
                println!("[12:00:01] [Server thread/INFO]: Done (1.234s)! For help, type \"help\"");
                println!("[12:00:02] [Server thread/INFO]: Notch joined the game");
                // Wait for `stop` on stdin, exactly as a server does.
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                println!("[12:00:03] [Server thread/INFO]: Stopping server");
            }
            "crash" => {
                println!("[12:00:00] [main/INFO]: loading");
                eprintln!("something went badly wrong");
                std::process::exit(3);
            }
            "deaf" => {
                // Never ready, never listens. The ladder has to end this.
                println!("[12:00:00] [main/INFO]: not going to cooperate");
                std::thread::sleep(Duration::from_secs(120));
            }
            _ => {}
        }
        // Leave without the harness printing a result the parent would read as
        // console output.
        std::io::stdout().flush().ok();
        std::process::exit(0);
    }

    fn drive(script: &str, on_running: impl FnOnce(&ProcessEngine, &StopSignal)) -> RunOutcome {
        let engine = Arc::new(ProcessEngine::new(fake_server(script)));
        let stop = StopSignal::default();
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 25565,
        };

        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let ready = Arc::new(Mutex::new(false));

        let outcome = std::thread::scope(|scope| {
            let runner = {
                let (engine, stop, lines, ready) = (
                    Arc::clone(&engine),
                    stop.clone(),
                    Arc::clone(&lines),
                    Arc::clone(&ready),
                );
                scope.spawn(move || {
                    engine.run(
                        &request,
                        stop,
                        &|line| lines.lock().unwrap().push(line),
                        &|| *ready.lock().unwrap() = true,
                    )
                })
            };

            // Give the child a moment to be up before poking it.
            std::thread::sleep(Duration::from_millis(400));
            on_running(&engine, &stop);
            runner.join().expect("the run thread must not panic")
        });

        let seen = lines.lock().unwrap().join("\n");
        assert!(!seen.is_empty(), "the child produced no console output");
        outcome
    }

    #[test]
    fn a_server_that_is_asked_to_stop_stops() {
        let outcome = drive("ready", |engine, stop| {
            // What the host does: the console rung, then record the intent.
            engine.command("stop").expect("stdin must be reachable");
            stop.request_stop();
        });
        assert_eq!(outcome, RunOutcome::Stopped);
    }

    #[test]
    fn readiness_comes_from_the_console_and_brings_the_roster_with_it() {
        let engine = Arc::new(ProcessEngine::new(fake_server("ready")));
        let stop = StopSignal::default();
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 25565,
        };
        let ready = Arc::new(Mutex::new(false));

        std::thread::scope(|scope| {
            let runner = {
                let (engine, stop, ready) = (Arc::clone(&engine), stop.clone(), Arc::clone(&ready));
                scope.spawn(move || {
                    engine.run(&request, stop, &|_| {}, &|| *ready.lock().unwrap() = true)
                })
            };

            std::thread::sleep(Duration::from_millis(500));
            assert!(*ready.lock().unwrap(), "`Done (…)` must announce ready");

            let (players, max) = engine.players().expect("a live run reports a roster");
            assert_eq!(players, vec![("Notch".to_string(), None)]);
            assert_eq!(max, Some(7), "the ceiling comes off the console too");

            engine.command("stop").ok();
            stop.request_stop();
            runner.join().expect("the run thread must not panic");
        });

        // The run is over, so there is no roster to report — not an empty one,
        // which would read as "nobody is playing" rather than "nothing is up".
        assert!(engine.players().is_none());
    }

    #[test]
    fn a_server_that_exits_on_its_own_is_a_crash() {
        let outcome = drive("crash", |_, _| {});
        match outcome {
            RunOutcome::Crashed(reason) => assert!(reason.contains('3'), "{reason}"),
            other => panic!("expected a crash, got {other:?}"),
        }
    }

    #[test]
    fn stderr_reaches_the_console_too() {
        let engine = ProcessEngine::new(fake_server("crash"));
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 25565,
        };
        engine.run(
            &request,
            StopSignal::default(),
            &|line| lines.lock().unwrap().push(line),
            &|| {},
        );
        let seen = lines.lock().unwrap().join("\n");
        assert!(
            seen.contains("something went badly wrong"),
            "a server that only complains on stderr must still be heard: {seen}"
        );
    }

    /// The rung that exists for a wedged JVM. Without it a server that ignores
    /// `stop` would hold the app open until the process died on its own.
    #[test]
    fn a_server_that_ignores_stop_is_taken_out_by_the_ladder() {
        // The real ladder gives a save thirty seconds before it terminates,
        // which is right on a device and pointless here — this is proving
        // that the climb happens at all, not how patient it is.
        let engine = ProcessEngine::with_ladder(
            fake_server("deaf"),
            vec![
                jvm::Rung {
                    action: jvm::Action::Console,
                    wait_ms: 300,
                },
                jvm::Rung {
                    action: jvm::Action::Terminate,
                    wait_ms: 2_000,
                },
                jvm::Rung {
                    action: jvm::Action::Kill,
                    wait_ms: 0,
                },
            ],
        );
        let stop = StopSignal::default();
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 25565,
        };

        let started = Instant::now();
        std::thread::scope(|scope| {
            let inner = stop.clone();
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                inner.request_stop();
            });
            engine.run(&request, stop, &|_| {}, &|| {});
        });

        // The child sleeps for two minutes; the ladder must not.
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the ladder took {:?} — it should have terminated the process",
            started.elapsed()
        );
    }
}

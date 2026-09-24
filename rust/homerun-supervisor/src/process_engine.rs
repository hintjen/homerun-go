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
//!
//! # Two kinds of server, one supervisor
//!
//! Until descriptor-driven games existed, everything this engine knew about a
//! server was Minecraft's: `console::is_ready` decided when it was up, the
//! roster was built from Minecraft's join and leave lines, and the stop verb
//! was the literal `stop` written to stdin.
//!
//! None of that is true of a game described by a `game.json`. Its ready line
//! is a substring the descriptor names, its console may be RCON on a loopback
//! port rather than stdin, and its stop verb is whatever the vendor chose.
//!
//! So those three answers moved out of the code and into [`Supervision`],
//! which the host supplies alongside the invocation. [`ProcessEngine::new`]
//! still means "a Minecraft server", so nothing that already used this engine
//! changed; [`ProcessEngine::supervised`] is the descriptor-driven door.
//!
//! The stop ladder is the part worth watching. Both callers produce one —
//! `minecraft::jvm::stop_ladder` and `homerun_core::engine::control::stop_ladder`
//! — and this file has its own [`Rung`] that both convert into, rather than
//! one of the two winning. That is not indirection for its own sake: the core
//! is not allowed to depend on its own Minecraft module, so there is no shared
//! type up there to use, and a supervisor that walked two different ladder
//! types would be two stop paths pretending to be one.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use homerun_core::minecraft::{console, jvm};
use serde::{Deserialize, Serialize};

use crate::engine::{Engine, Roster, RunOutcome, RunRequest, StopSignal};
use crate::platform;

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
    /// Removed from the inherited environment before `env` is added, so a
    /// variable can be taken away rather than only set to "".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset: Vec<String>,
}

/// How to tell this particular server is up, talk to it, and stop it.
///
/// Everything here used to be Minecraft's, hard-coded. See the module header.
pub struct Supervision {
    pub readiness: Readiness,
    pub presence: Presence,
    pub console: ConsoleRoute,
    /// Walked in order when a stop is requested. Always ends somewhere it
    /// cannot be ignored — both producers guarantee it, and
    /// [`ProcessEngine::supervised`] does not check, because a ladder that
    /// ended early would leave a process running rather than fail visibly.
    pub ladder: Vec<Rung>,
}

/// What makes a line mean "the server is accepting connections".
pub enum Readiness {
    /// Minecraft's own console rules, across its three engines.
    Minecraft,
    /// A substring a descriptor named.
    ///
    /// Not a regex, and `engine::control` has the argument for why: four
    /// console defects in one PowerNukkitX bring-up came from parsers being
    /// cleverer than the stream deserved.
    Marker(String),
}

/// Where the player count comes from.
pub enum Presence {
    /// Minecraft's join and leave lines, and its player ceiling.
    Minecraft,
    /// Substrings a descriptor named. Counts only; these lines carry no name
    /// this engine can trust to be a player's.
    Markers { join: String, leave: String },
    /// Nothing in the log says. The roster stays empty, which is honest —
    /// inventing a count is how a server reported zero players for ever.
    None,
}

/// Where a console command goes.
pub enum ConsoleRoute {
    /// A line on the process's stdin, as every Minecraft server takes it.
    Stdin,
    /// RCON on a loopback port. Built by the host once it knows which port
    /// the server was told to bind.
    #[cfg(feature = "game-engine")]
    Rcon(crate::rcon::Target),
    /// This server takes no commands. `stop` still works; it just starts
    /// lower on the ladder.
    None,
}

/// One rung of a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rung {
    pub action: Action,
    /// How long to wait for the server to go before the next rung.
    pub wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this verb over whatever [`ConsoleRoute`] says. The rung that
    /// flushes a world.
    Console(String),
    /// A console control event. Unsupported on Windows, and
    /// [`platform::graceful_interrupt`] says so rather than pretending.
    Interrupt,
    /// Ask the process to exit.
    Terminate,
    /// The rung that cannot be refused.
    Kill,
}

impl From<&jvm::Rung> for Rung {
    /// Minecraft's ladder, in this file's terms. `Action::Console` there
    /// carries no verb because there has only ever been one.
    fn from(rung: &jvm::Rung) -> Self {
        Rung {
            action: match rung.action {
                jvm::Action::Console => Action::Console(jvm::STOP_COMMAND.to_string()),
                jvm::Action::Terminate => Action::Terminate,
                jvm::Action::Kill => Action::Kill,
            },
            wait_ms: rung.wait_ms,
        }
    }
}

impl From<&homerun_core::engine::control::Rung> for Rung {
    fn from(rung: &homerun_core::engine::control::Rung) -> Self {
        use homerun_core::engine::control::Action as Core;
        Rung {
            action: match &rung.action {
                Core::Console { command } => Action::Console(command.clone()),
                Core::Interrupt => Action::Interrupt,
                Core::Terminate => Action::Terminate,
                Core::Kill => Action::Kill,
            },
            wait_ms: rung.wait_ms,
        }
    }
}

impl Supervision {
    /// A Minecraft server, which is what this engine meant before descriptors
    /// existed.
    pub fn minecraft() -> Self {
        Self {
            readiness: Readiness::Minecraft,
            presence: Presence::Minecraft,
            console: ConsoleRoute::Stdin,
            ladder: jvm::stop_ladder(true).iter().map(Rung::from).collect(),
        }
    }
}

/// A game server running as a child process.
pub struct ProcessEngine {
    invocation: Invocation,
    required_job: Option<Arc<crate::job::Job>>,
    /// The live run. Held so `command` can reach stdin and `players` can read
    /// the roster the console pump is building.
    run: Arc<Mutex<Option<Live>>>,
    /// How to tell this server is up, talk to it, and stop it. Always the
    /// core's answer in production — the tests supply a shorter ladder so
    /// they do not sit through the real 30-second save grace on every
    /// `cargo test`, which would be thirty seconds added to a suite that
    /// otherwise finishes in under one.
    supervision: Supervision,
}

struct Live {
    stdin: Option<Box<dyn Write + Send>>,
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
    /// A Minecraft server. Unchanged from before descriptors existed, and
    /// every existing caller means this one.
    pub fn new(invocation: Invocation) -> Self {
        Self::supervised(invocation, Supervision::minecraft())
    }

    /// A server whose readiness, console and stop come from its descriptor.
    pub fn supervised(invocation: Invocation, supervision: Supervision) -> Self {
        Self {
            invocation,
            required_job: None,
            run: Arc::new(Mutex::new(None)),
            supervision,
        }
    }

    /// Mounted saves require ownership before any game code can run. The
    /// runtime owner also retains this job until tree exit and link cleanup.
    pub fn require_job(&mut self, job: Arc<crate::job::Job>) {
        self.required_job = Some(job);
    }

    /// Windows inspects the required Job even before spawn and after root exit.
    /// Development adapters return None when their root PID is absent.
    pub fn network_snapshot(&self) -> Result<Option<Vec<(u32, platform::Listening)>>, String> {
        #[cfg(not(windows))]
        {
            let Some(pid) = self.pid() else {
                return Ok(None);
            };
            return platform::checked_listening_ports(&[pid]).map(Some);
        }
        #[cfg(windows)]
        {
            let job = self
                .required_job
                .as_ref()
                .ok_or("The game process tree is not owned.")?;
            platform::checked_listening_ports(&job.process_ids()?).map(Some)
        }
    }

    /// A network refusal must not leave an exposed listener alive for the save grace.
    pub fn terminate_owned_tree(&self) -> Result<(), String> {
        #[cfg(not(windows))]
        {
            if let Some(pid) = self.pid() {
                kill(pid);
            }
            return Ok(());
        }
        #[cfg(windows)]
        {
            let job = self
                .required_job
                .as_ref()
                .ok_or("The game process tree is not owned.")?;
            job.terminate_and_wait()
        }
    }

    /// The same engine on a ladder of your choosing. Tests only: a production
    /// host must not get to shorten the window a world save is given.
    #[cfg(test)]
    fn with_ladder(invocation: Invocation, ladder: Vec<Rung>) -> Self {
        Self::supervised(
            invocation,
            Supervision {
                ladder,
                ..Supervision::minecraft()
            },
        )
    }

    /// Whether a line means this server has finished starting.
    fn is_ready(&self, line: &str) -> bool {
        match &self.supervision.readiness {
            Readiness::Minecraft => console::is_ready(line),
            // An empty marker never matches. A descriptor with none is
            // refused by `engine::validate`; this is what happens if one
            // reaches a running system anyway, and "never ready" fails
            // visibly at the start timeout rather than reporting a server
            // that is not up as up.
            Readiness::Marker(marker) => !marker.is_empty() && line.contains(marker),
        }
    }

    /// Send a command wherever this server takes one.
    fn say(&self, command: &str) -> Result<(), String> {
        match &self.supervision.console {
            ConsoleRoute::Stdin => {
                let mut live = self.live();
                let stdin = live
                    .as_mut()
                    .and_then(|run| run.stdin.as_mut())
                    .ok_or_else(|| jvm::Refusal::NotAcceptingCommands.text().to_string())?;
                writeln!(stdin, "{command}")
                    .and_then(|_| stdin.flush())
                    .map_err(|_| jvm::Refusal::NotAcceptingCommands.text().to_string())
            }
            #[cfg(feature = "game-engine")]
            ConsoleRoute::Rcon(target) => {
                // The reply is discarded here: `Engine::command` returns
                // nothing, and a Minecraft server's reply has always arrived
                // as console output rather than as a return value. A caller
                // that wants the text asks `crate::rcon` directly.
                crate::rcon::command(target, command).map(|_| ())
            }
            ConsoleRoute::None => Err(jvm::Refusal::NotAcceptingCommands.text().to_string()),
        }
    }

    /// Recover from a poisoned lock rather than propagate. A panic elsewhere
    /// should not make the app permanently unable to report who is online.
    fn live(&self) -> std::sync::MutexGuard<'_, Option<Live>> {
        self.run.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A way for the stop watcher to say the stop verb, without a borrow of
    /// `self` it cannot hold.
    fn console_sender(&self) -> ConsoleSender {
        match &self.supervision.console {
            ConsoleRoute::Stdin => {
                let run = Arc::clone(&self.run);
                Box::new(move |verb: &str| {
                    let mut live = run.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(stdin) = live.as_mut().and_then(|r| r.stdin.as_mut()) {
                        // A broken pipe means it is already on its way out,
                        // which the next rung handles harmlessly.
                        let _ = writeln!(stdin, "{verb}");
                        let _ = stdin.flush();
                    }
                })
            }
            #[cfg(feature = "game-engine")]
            ConsoleRoute::Rcon(target) => {
                let target = target.clone();
                Box::new(move |verb: &str| {
                    // Same reasoning: an unreachable console on the way down
                    // is what the next rung is for.
                    let _ = crate::rcon::command(&target, verb);
                })
            }
            ConsoleRoute::None => Box::new(|_verb: &str| {}),
        }
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
        self.run_streamed(request, stop, &|line, _| on_line(line), on_ready)
    }

    fn command(&self, command: &str) -> Result<(), String> {
        if self.live().is_none() {
            return Err(jvm::Refusal::NotAcceptingCommands.text().to_string());
        }
        self.say(command)
    }

    fn pid(&self) -> Option<u32> {
        self.live().as_ref().map(|run| run.pid)
    }

    fn usage(&self) -> Option<(u64, f64)> {
        platform::process_stats(self.pid()?).map(|s| (s.rss_kb, s.cpu_seconds))
    }

    fn players(&self) -> Option<Roster> {
        let live = self.live();
        let roster = live.as_ref()?.roster.lock().ok()?;
        Some((
            roster
                .players
                .iter()
                .map(|name| (name.clone(), None))
                .collect(),
            roster.max,
        ))
    }
}

impl ProcessEngine {
    /// Like `Engine::run`, retaining each pipe's identity for the NDJSON host.
    /// Both pipes are drained independently: a quiet stdout must not hide
    /// stderr readiness or let a full stderr pipe deadlock startup.
    pub fn run_streamed(
        &self,
        request: &RunRequest,
        stop: StopSignal,
        on_line: &dyn Fn(String, &'static str),
        on_ready: &dyn Fn(),
    ) -> RunOutcome {
        let mut command = Command::new(&self.invocation.program);
        command
            .args(&self.invocation.args)
            .current_dir(&request.data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in &self.invocation.unset {
            command.env_remove(key);
        }
        for (key, value) in &self.invocation.env {
            command.env(key, value);
        }

        // Created before the spawn so that a failure to make one is known
        // before there is a process to own. See `job.rs`: on Windows this is
        // what stops a hard-killed runner leaving the game behind holding its
        // ports and its saves, and what makes the kill rung a tree kill. It
        // is `None` everywhere else, where a pid and a signal already are one.
        let job = self
            .required_job
            .clone()
            .or_else(|| crate::job::Job::kill_on_close().map(Arc::new));

        let spawned = if let Some(job) = &self.required_job {
            job.spawn(&command)
        } else {
            command.spawn().map(|child| {
                if let Some(job) = &job {
                    if !job.adopt(&child) {
                        log::warn!("this server could not be put in a job object");
                    }
                }
                crate::job::Process::from(child)
            })
        };
        let mut child = match spawned {
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
        let (tx, rx) = std::sync::mpsc::sync_channel::<(String, &'static str)>(256);
        fn pump(
            reader: impl std::io::Read + Send + 'static,
            tx: std::sync::mpsc::SyncSender<(String, &'static str)>,
            stream: &'static str,
        ) {
            std::thread::spawn(move || {
                read_lines_lossy(reader, |line| tx.send((line, stream)).is_ok());
            });
        }
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, tx.clone(), "stdout");
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, tx.clone(), "stderr");
        }
        drop(tx);

        // The stop watcher: `run` is busy reading stdout for the whole life of
        // the server, so climbing the ladder has to happen from somewhere
        // else. It takes the child's pid rather than the child itself, because
        // waiting on the child belongs to this thread alone.
        let watcher = spawn_stop_watcher(
            child.id(),
            stop.clone(),
            self.supervision.ladder.clone(),
            self.console_sender(),
            job.clone(),
        );

        let mut ready = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((line, stream)) => {
                    observe(&line, &roster, &self.supervision.presence);
                    on_line(line.clone(), stream);
                    // The console saying it is accepting connections is the only
                    // honest signal for this; the process existing is not one.
                    if !ready && self.is_ready(&line) {
                        ready = true;
                        on_ready();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // A descendant may retain a pipe after the actual server exits.
                    if child.try_wait().ok().flatten().is_some() {
                        break;
                    }
                }
            }
        }

        // The pipe closed, which means the process is on its way out.
        let status = child.wait();
        watcher.finish();
        *self.live() = None;

        // Whatever stderr had left to say, now that stdout is done.
        for (line, stream) in rx.try_iter() {
            on_line(line, stream);
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
}

/// Fold one console line into the roster.
fn observe(line: &str, roster: &Arc<Mutex<RosterState>>, presence: &Presence) {
    let Ok(mut roster) = roster.lock() else {
        return;
    };

    match presence {
        Presence::Minecraft => {
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
        // A descriptor's markers say *that* somebody joined, not who. Naming
        // them would mean parsing a name out of a line whose shape we have
        // only ever seen in one vendor's log, and a wrong name is worse than
        // no name: it reaches the API as a player.
        Presence::Markers { join, leave } => {
            if !join.is_empty() && line.contains(join.as_str()) {
                roster.players.push(String::new());
            }
            if !leave.is_empty() && line.contains(leave.as_str()) {
                roster.players.pop();
            }
        }
        Presence::None => {}
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

/// How the stop watcher says something to the server.
///
/// A boxed closure rather than a borrow of the engine: the watcher outlives
/// the call that made it and runs on its own thread, and `ProcessEngine` is
/// not `'static`.
type ConsoleSender = Box<dyn Fn(&str) + Send + 'static>;

fn spawn_stop_watcher(
    pid: u32,
    stop: StopSignal,
    ladder: Vec<Rung>,
    say: ConsoleSender,
    job: Option<Arc<crate::job::Job>>,
) -> StopWatcher {
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
            match &rung.action {
                // **This rung has to be carried out here.** It was briefly a
                // no-op, on the theory that the host would write `stop`
                // through `Engine::command` — which was true only while the
                // host still ran its own ladder. Once stopping became "set the
                // signal and let the supervisor climb", nothing wrote it at
                // all, and every stop sat through the full save grace in
                // silence and then took a SIGTERM. The rung that exists to
                // save the world was the one being skipped.
                Action::Console(verb) => say(verb),
                // A failure here is not fatal to the stop: the next rung
                // handles it, and on Windows this rung always fails because
                // the platform cannot do it. Saying so on the diagnostics
                // stream and carrying on is the whole behaviour.
                Action::Interrupt => {
                    if let Err(why) = platform::graceful_interrupt(pid) {
                        log::warn!("{why}");
                    }
                }
                Action::Terminate => terminate(pid),
                // The rung that cannot be refused, and on Windows that now
                // means the whole subtree. A launcher-style server -- which
                // is what a great many vendors ship -- has already exited by
                // this point, so ending its pid ends nothing; the job holds
                // what it started regardless of re-parenting.
                Action::Kill => match &job {
                    Some(job) => job.terminate(),
                    None => kill(pid),
                },
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

/// The longest console line kept whole, in bytes.
///
/// Not a guess about servers: it is a ceiling on what one line can cost when
/// the pipe turns out not to be carrying lines at all. A game that writes a
/// megabyte of binary with no newline in it would otherwise be read into one
/// `Vec` before anything looked at it.
const MAX_LINE: usize = 16 * 1024;

/// Read a child's output as lines, tolerating bytes that are not UTF-8.
///
/// `BufRead::lines()` yields `Err(InvalidData)` for a line that is not UTF-8,
/// and `map_while(Result::ok)` — which is what this was — treats that as the
/// end of the stream. So **one** bad byte ended log capture for the rest of
/// the server's life and dropped the pipe. That is not a hypothetical: a
/// player with a name in a legacy code page stops `server-log`, and if it
/// happens before the ready marker the server never reports ready at all and
/// is killed at the start timeout.
///
/// So: read to the newline, decode lossily, and keep going. A line longer
/// than [`MAX_LINE`] is cut with a marker and the rest of it discarded, which
/// is what stops a binary blob from being a memory problem.
///
/// Otherwise identical to `lines()`, deliberately — this is the path
/// Minecraft's console takes on Android, and its parsers are written against
/// what `lines()` produced: one trailing newline removed, then one trailing
/// carriage return, and nothing else touched. `on_line` returning false stops
/// the read, which is how a closed channel ends the thread.
pub(crate) fn read_lines_lossy(
    reader: impl std::io::Read,
    mut on_line: impl FnMut(String) -> bool,
) {
    let mut reader = BufReader::new(reader);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        // One byte past the ceiling, so that "no newline yet" and "a line
        // exactly at the ceiling" are told apart.
        // Fully qualified: `.take()` on something iterable resolves to
        // `Iterator::take`, which is a different method entirely.
        let read = match std::io::Read::take(&mut reader, MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut raw)
        {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };

        let overlong = read > MAX_LINE && raw.last() != Some(&b'\n');
        if overlong {
            raw.truncate(MAX_LINE);
        }
        if raw.last() == Some(&b'\n') {
            raw.pop();
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
        }

        let mut line = String::from_utf8_lossy(&raw).into_owned();
        if overlong {
            line.push_str(" [truncated]");
        }
        if !on_line(line) {
            return;
        }
        if overlong && !skip_to_newline(&mut reader) {
            return;
        }
    }
}

/// Throw away the rest of a line that was too long to keep, without ever
/// holding more than the ceiling in memory. False means end of stream.
fn skip_to_newline(reader: &mut impl BufRead) -> bool {
    let mut sink = Vec::new();
    loop {
        sink.clear();
        match std::io::Read::take(&mut *reader, MAX_LINE as u64).read_until(b'\n', &mut sink) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {
                if sink.last() == Some(&b'\n') {
                    return true;
                }
            }
        }
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
    // `/T` as well as `/F`, for the case where there was no job to own the
    // subtree. It walks parent links, so a launcher that has already exited
    // hides its children from it — which is exactly why the job above is the
    // real answer and this is the fallback.
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F", "/T"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use homerun_core::engine::GameDescriptor;

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
            // A game described by a `game.json`: nothing about its console
            // looks like Minecraft's.
            "game" => {
                println!("18:22:00 Loading world");
                println!("18:22:01 Server startup complete");
                println!("18:22:02 Craig has entered the world");
                // Waits for the descriptor's own verb, not `stop`.
                loop {
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if line.trim() == "quit" {
                        println!("18:22:09 Saving and shutting down");
                        break;
                    }
                }
            }
            // A game whose console is not UTF-8. The bytes below are a name
            // in a legacy code page, which is what a player called François
            // produces on a machine that has never heard of UTF-8 -- and the
            // marker comes *after* it, which is the case that used to kill
            // the server rather than merely lose a line.
            "mojibake" => {
                let mut out = std::io::stdout();
                out.write_all(b"18:22:00 Loading world\n").unwrap();
                out.write_all(b"18:22:01 Fran\xe7ois has entered the world\n")
                    .unwrap();
                out.write_all(b"18:22:02 Server startup complete\n")
                    .unwrap();
                out.flush().unwrap();
                loop {
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if line.trim() == "quit" {
                        println!("18:22:09 Saving and shutting down");
                        break;
                    }
                }
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

    // ─── a server described by a game.json ──────────────────────────────────
    //
    // The same supervisor, told different answers. These exist because the
    // three things that moved into `Supervision` are exactly the three that
    // used to be Minecraft's by assumption, and an assumption that moved into
    // a field can still be read from the wrong place.

    /// A descriptor-driven game's supervision, with a ladder short enough for
    /// a test.
    #[cfg(feature = "game-engine")]
    fn descriptor_supervision(console: ConsoleRoute) -> Supervision {
        Supervision {
            readiness: Readiness::Marker("Server startup complete".into()),
            presence: Presence::Markers {
                join: "has entered the world".into(),
                leave: "has left the world".into(),
            },
            console,
            ladder: vec![
                Rung {
                    action: Action::Console("quit".into()),
                    wait_ms: 3_000,
                },
                Rung {
                    action: Action::Terminate,
                    wait_ms: 2_000,
                },
                Rung {
                    action: Action::Kill,
                    wait_ms: 0,
                },
            ],
        }
    }

    #[cfg(feature = "game-engine")]
    fn drive_supervised(
        script: &str,
        supervision: Supervision,
        on_running: impl FnOnce(&ProcessEngine, &StopSignal),
    ) -> RunOutcome {
        let engine = Arc::new(ProcessEngine::supervised(fake_server(script), supervision));
        let stop = StopSignal::default();
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 28015,
            settings: None,
            local_network: false,
        };

        let ready = Arc::new(Mutex::new(false));
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));

        std::thread::scope(|scope| {
            let engine_for_run = Arc::clone(&engine);
            let ready_for_run = Arc::clone(&ready);
            let lines_for_run = Arc::clone(&lines);
            let stop_for_run = stop.clone();

            let run = scope.spawn(move || {
                engine_for_run.run(
                    &request,
                    stop_for_run,
                    &|line| lines_for_run.lock().unwrap().push(line),
                    &|| *ready_for_run.lock().unwrap() = true,
                )
            });

            // Wait for the ready callback rather than sleeping a fixed time,
            // so a slow machine does not make this flaky.
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && !*ready.lock().unwrap() {
                std::thread::sleep(Duration::from_millis(20));
            }

            on_running(&engine, &stop);
            run.join().expect("the run thread must not panic")
        })
    }

    // ─── a console that is not UTF-8 ───────────────────────────────────────
    //
    // `BufRead::lines()` yields `Err(InvalidData)` for a line that is not
    // UTF-8, and `map_while(Result::ok)` read that as end of stream. One bad
    // byte therefore ended log capture for the rest of the server's life and
    // dropped the pipe — and before the ready marker, that is a server that
    // never reports ready and is killed at the start timeout.

    fn lossy(input: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        read_lines_lossy(input, |line| {
            out.push(line);
            true
        });
        out
    }

    #[test]
    fn a_byte_that_is_not_utf8_costs_its_own_line_and_nothing_after_it() {
        let got = lossy(b"first\n\xe7 second\nthird\n");
        assert_eq!(got.len(), 3, "the stream ended early: {got:?}");
        assert_eq!(got[0], "first");
        assert!(got[1].ends_with(" second"), "{:?}", got[1]);
        assert!(
            got[1].contains('\u{fffd}'),
            "the bad byte vanished silently"
        );
        assert_eq!(got[2], "third", "everything after one bad byte was lost");
    }

    /// Identical to `lines()` on every shape that is not the bug, because
    /// this is the path Minecraft's console takes on Android and its parsers
    /// are written against exactly what `lines()` produced.
    #[test]
    fn line_endings_are_handled_the_way_lines_handled_them() {
        for input in [
            &b"a\r\nb\nc"[..],
            b"\n\na\n",
            b"a",
            b"",
            b"trailing\r\n",
            b"a\r\r\n",
        ] {
            let mine = lossy(input);
            let theirs: Vec<String> = BufReader::new(input)
                .lines()
                .map_while(Result::ok)
                .collect();
            assert_eq!(mine, theirs, "for {input:?}");
        }
    }

    /// A pipe that turns out not to be carrying lines must not be read into
    /// one allocation. The line is cut, marked, and the rest of it dropped —
    /// and the *next* line still arrives, which is what says the reader
    /// resynchronised rather than giving up.
    #[test]
    fn a_line_with_no_end_to_it_is_cut_rather_than_kept() {
        let mut input = vec![b'a'; MAX_LINE * 4];
        input.extend_from_slice(b"\nnext\n");

        let got = lossy(&input);
        assert_eq!(
            got.len(),
            2,
            "{:?}",
            got.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(got[0].len(), MAX_LINE + " [truncated]".len());
        assert!(got[0].ends_with(" [truncated]"), "cut without saying so");
        assert_eq!(got[1], "next", "the reader did not find the next line");
    }

    /// The end-to-end case, with a real child process: the bad byte arrives
    /// *before* the ready marker, so the old reader meant this server never
    /// became ready at all.
    #[cfg(feature = "game-engine")]
    #[test]
    fn a_console_that_is_not_utf8_still_reaches_the_ready_marker() {
        let engine = Arc::new(ProcessEngine::supervised(
            fake_server("mojibake"),
            descriptor_supervision(ConsoleRoute::Stdin),
        ));
        let stop = StopSignal::default();
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 0,
            local_network: false,
            settings: None,
        };

        let ready = Arc::new(Mutex::new(false));
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));

        std::thread::scope(|scope| {
            let (e, r, l, s) = (
                Arc::clone(&engine),
                Arc::clone(&ready),
                Arc::clone(&lines),
                stop.clone(),
            );
            let run = scope.spawn(move || {
                e.run(&request, s, &|line| l.lock().unwrap().push(line), &|| {
                    *r.lock().unwrap() = true
                })
            });

            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && !*ready.lock().unwrap() {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                *ready.lock().unwrap(),
                "a server whose console is not UTF-8 never reported ready: {:?}",
                lines.lock().unwrap()
            );
            stop.request_stop();
            run.join().expect("the run thread must not panic")
        });

        let lines = lines.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("Server startup complete")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("has entered the world")),
            "the line with the bad byte in it was dropped whole: {lines:?}"
        );
    }

    /// The descriptor's marker is what makes this server ready — and
    /// Minecraft's would not have.
    #[cfg(feature = "game-engine")]
    #[test]
    fn a_descriptor_game_becomes_ready_on_its_own_marker() {
        let engine = ProcessEngine::supervised(
            fake_server("game"),
            descriptor_supervision(ConsoleRoute::Stdin),
        );

        assert!(engine.is_ready("18:22:01 Server startup complete"));
        assert!(!engine.is_ready("18:22:00 Loading world"));
        // The line that would have started a Minecraft server means nothing
        // here, which is the whole point of the field.
        assert!(!engine
            .is_ready("[12:00:01] [Server thread/INFO]: Done (1.234s)! For help, type \"help\""));
    }

    /// And the reverse: a Minecraft server is unmoved by a descriptor's
    /// marker.
    #[test]
    fn a_minecraft_server_is_unmoved_by_another_games_marker() {
        let engine = ProcessEngine::new(fake_server("ready"));
        assert!(engine
            .is_ready("[12:00:01] [Server thread/INFO]: Done (1.234s)! For help, type \"help\""));
        assert!(!engine.is_ready("Server startup complete"));
    }

    /// End to end on a real child process: the descriptor's marker brings it
    /// up, and the descriptor's verb — not `stop` — takes it down.
    #[cfg(feature = "game-engine")]
    #[test]
    fn a_descriptor_game_starts_on_its_marker_and_stops_on_its_own_verb() {
        let started = Instant::now();
        let outcome = drive_supervised(
            "game",
            descriptor_supervision(ConsoleRoute::Stdin),
            |_engine, stop| stop.request_stop(),
        );

        assert_eq!(outcome, RunOutcome::Stopped);
        // The polite rung did it. Reaching `Terminate` would have taken the
        // three-second grace first, so finishing well inside that is the
        // assertion that `quit` was what worked.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the console rung did not stop it; the ladder had to escalate ({:?})",
            started.elapsed()
        );
    }

    /// An empty marker must never look like a server that is instantly up.
    #[cfg(feature = "game-engine")]
    #[test]
    fn a_game_with_no_marker_never_reports_ready() {
        let engine = ProcessEngine::supervised(
            fake_server("game"),
            Supervision {
                readiness: Readiness::Marker(String::new()),
                ..descriptor_supervision(ConsoleRoute::Stdin)
            },
        );
        assert!(!engine.is_ready(""));
        assert!(!engine.is_ready("Server startup complete"));
        assert!(!engine.is_ready("anything at all"));
    }

    /// A console command goes wherever the route says, and for a game with no
    /// stdin console that is a socket.
    #[cfg(feature = "game-engine")]
    #[test]
    fn a_command_for_an_rcon_game_goes_over_rcon_rather_than_stdin() {
        use std::sync::mpsc;

        // A stand-in Source RCON server: enough of the protocol to prove the
        // command arrived.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback");
        let address = listener.local_addr().unwrap().to_string();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            loop {
                let mut header = [0u8; 4];
                if std::io::Read::read_exact(&mut stream, &mut header).is_err() {
                    return;
                }
                let size = i32::from_le_bytes(header) as usize;
                let mut rest = vec![0u8; size];
                if std::io::Read::read_exact(&mut stream, &mut rest).is_err() {
                    return;
                }
                let id = i32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
                let kind = i32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
                let body = &rest[8..];
                let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
                let text = String::from_utf8_lossy(&body[..end]).into_owned();

                let (answer_id, answer_kind): (i32, i32) =
                    if kind == 3 { (id, 2) } else { (id, 0) };
                if kind == 2 && !text.is_empty() {
                    let _ = tx.send(text);
                }
                let size: i32 = 4 + 4 + 2;
                let mut packet = Vec::new();
                packet.extend_from_slice(&size.to_le_bytes());
                packet.extend_from_slice(&answer_id.to_le_bytes());
                packet.extend_from_slice(&answer_kind.to_le_bytes());
                packet.extend_from_slice(&[0, 0]);
                if std::io::Write::write_all(&mut stream, &packet).is_err() {
                    return;
                }
            }
        });

        let route = ConsoleRoute::Rcon(crate::rcon::Target {
            protocol: homerun_core::engine::descriptor::RconProtocol::Source,
            address,
            password: "hunter2".into(),
        });

        drive_supervised("game", descriptor_supervision(route), |engine, stop| {
            engine
                .command("playerlist")
                .expect("an RCON console must accept a command");
            stop.request_stop();
        });

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "playerlist",
            "the command did not arrive over RCON"
        );
    }

    /// Before the process exists there is nothing to talk to, whatever the
    /// route. Without this an RCON game would open a socket to a port nothing
    /// has bound and report *that* — true, and useless to a player.
    #[cfg(feature = "game-engine")]
    #[test]
    fn a_command_before_the_server_starts_is_refused_without_reaching_for_a_socket() {
        let engine = ProcessEngine::supervised(
            fake_server("game"),
            descriptor_supervision(ConsoleRoute::Rcon(crate::rcon::Target {
                protocol: homerun_core::engine::descriptor::RconProtocol::Source,
                // Nothing is listening here, and nothing should try.
                address: "127.0.0.1:1".into(),
                password: "x".into(),
            })),
        );
        let started = Instant::now();
        assert!(engine.command("status").is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "it went to the network before checking there was a server"
        );
    }

    /// A game whose descriptor gives no console still stops — it just starts
    /// lower on the ladder.
    #[cfg(feature = "game-engine")]
    #[test]
    fn a_game_with_no_console_refuses_commands_and_still_stops() {
        // The `game` script, which becomes ready and then waits on a stdin
        // verb it will never be sent -- so the ladder below is what ends it.
        // Using the `deaf` script here instead would mean this test sat out
        // the whole ready deadline before it began.
        let outcome = drive_supervised(
            "game",
            Supervision {
                console: ConsoleRoute::None,
                ladder: vec![
                    Rung {
                        action: Action::Terminate,
                        wait_ms: 2_000,
                    },
                    Rung {
                        action: Action::Kill,
                        wait_ms: 0,
                    },
                ],
                ..descriptor_supervision(ConsoleRoute::None)
            },
            |engine, stop| {
                assert!(engine.command("anything").is_err());
                stop.request_stop();
            },
        );
        assert_eq!(outcome, RunOutcome::Stopped);
    }

    /// The presence markers count, and deliberately do not name anyone: a
    /// name parsed out of a line whose shape we have seen in one vendor's log
    /// would reach the API as a player.
    #[cfg(feature = "game-engine")]
    #[test]
    fn descriptor_presence_counts_players_without_inventing_names() {
        let roster = Arc::new(Mutex::new(RosterState::default()));
        let presence = Presence::Markers {
            join: "has entered the world".into(),
            leave: "has left the world".into(),
        };

        observe("Craig has entered the world", &roster, &presence);
        observe("Dana has entered the world", &roster, &presence);
        assert_eq!(roster.lock().unwrap().players.len(), 2);

        observe("Craig has left the world", &roster, &presence);
        assert_eq!(roster.lock().unwrap().players.len(), 1);

        // Nothing invented a name.
        assert!(roster.lock().unwrap().players.iter().all(String::is_empty));

        // And a line that is neither changes nothing.
        observe("Dana said hello", &roster, &presence);
        assert_eq!(roster.lock().unwrap().players.len(), 1);
    }

    #[cfg(feature = "game-engine")]
    #[test]
    fn a_game_that_reports_no_presence_keeps_an_empty_roster() {
        let roster = Arc::new(Mutex::new(RosterState::default()));
        observe("Notch joined the game", &roster, &Presence::None);
        observe("Craig has entered the world", &roster, &Presence::None);
        assert!(roster.lock().unwrap().players.is_empty());
    }

    /// Both ladders end somewhere they cannot be ignored, and this is the
    /// conversion that has to preserve that.
    #[test]
    fn both_kinds_of_ladder_convert_with_their_last_rung_intact() {
        let minecraft: Vec<Rung> = jvm::stop_ladder(true).iter().map(Rung::from).collect();
        assert_eq!(minecraft.last().unwrap().action, Action::Kill);
        assert_eq!(
            minecraft.first().unwrap().action,
            Action::Console(jvm::STOP_COMMAND.to_string()),
            "Minecraft's console rung has no verb of its own and gains one here"
        );

        let descriptor: GameDescriptor = serde_json::from_str(
            r#"{ "id": "g", "console": { "via": "stdin" },
                 "stop": { "via": "console", "command": "quit", "graceMs": 1000 } }"#,
        )
        .unwrap();
        let converted: Vec<Rung> = homerun_core::engine::control::stop_ladder(&descriptor)
            .iter()
            .map(Rung::from)
            .collect();
        assert_eq!(
            converted.first().unwrap().action,
            Action::Console("quit".to_string())
        );
        assert_eq!(converted.last().unwrap().action, Action::Kill);
    }

    fn drive(script: &str, on_running: impl FnOnce(&ProcessEngine, &StopSignal)) -> RunOutcome {
        let engine = Arc::new(ProcessEngine::new(fake_server(script)));
        let stop = StopSignal::default();
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 25565,
            settings: None,
            local_network: false,
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

    /// A stop is a signal and nothing else — the supervisor carries out every
    /// rung, including the console one.
    ///
    /// This test used to write `stop` itself, "as the host does", and that
    /// assumption outlived the host doing it: once stopping became a signal,
    /// nothing wrote the command, and every stop sat through the full 30
    /// second save grace in silence before being terminated. The test passed
    /// throughout, because it was supplying the missing piece.
    ///
    /// So it no longer supplies it, and the elapsed time is the assertion: on
    /// the real ladder a console stop ends the run in milliseconds, and a
    /// console rung that does nothing cannot finish inside thirty seconds.
    #[test]
    fn a_stop_is_carried_out_by_the_supervisor_not_the_caller() {
        let started = Instant::now();
        let outcome = drive("ready", |_engine, stop| {
            stop.request_stop();
        });
        assert_eq!(outcome, RunOutcome::Stopped);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the console rung was skipped — this took {:?}, so the server was              terminated rather than asked to save",
            started.elapsed()
        );
    }

    #[test]
    fn readiness_comes_from_the_console_and_brings_the_roster_with_it() {
        let engine = Arc::new(ProcessEngine::new(fake_server("ready")));
        let stop = StopSignal::default();
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 25565,
            settings: None,
            local_network: false,
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
            settings: None,
            local_network: false,
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
                Rung {
                    action: Action::Console(jvm::STOP_COMMAND.to_string()),
                    wait_ms: 300,
                },
                Rung {
                    action: Action::Terminate,
                    wait_ms: 2_000,
                },
                Rung {
                    action: Action::Kill,
                    wait_ms: 0,
                },
            ],
        );
        let stop = StopSignal::default();
        let request = RunRequest {
            server_id: "s1".into(),
            data_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            java_port: 25565,
            settings: None,
            local_network: false,
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

//! Game extensions — the effects half.
//!
//! The argument for extensions, and their pure half (config validation, the
//! values they supply, the hosts they may reach), is in
//! `homerun_core::engine::extensions`. This module is what runs them: the
//! hooks an extension implements, the context it is given, and the one place
//! the runner calls into it.
//!
//! # The shape
//!
//! [`GameExtension`] is one game's extension, registered by name and alive
//! for the whole runner process. [`GameExtension::begin`] starts one server
//! run and returns a [`Run`], which owns that run's state and is dropped when
//! the run ends. Every hook has a default that does nothing.
//!
//! # The rules the runner enforces, so an extension cannot get them wrong
//!
//!  - **An extension reaches the world only through its context.** It gets no
//!    process, file or socket of its own. What it wants done on the server
//!    it asks for with an [`Action`], and the runner does it.
//!  - **Observers never block.** [`Run::on_line`] runs on the output pump,
//!    where one slow call stalls `server-log` and readiness. It returns
//!    actions; a worker thread carries them out.
//!  - **A panic never takes the runner down.** Every call is wrapped in
//!    `catch_unwind`. A panic in `begin` fails the start with
//!    `extension_failed`; one in an observer switches that run's observers
//!    off and is reported once.
//!  - **A sign-in URL is shown only if it is on a host the extension's spec
//!    allows** (`engine::extensions::url_allowed`), whatever printed it.
//!  - **What an extension supplies as secret is redacted** from every
//!    `server-log` line and the crash tail, like a host secret. Which keys are
//!    secret is the spec's answer, never the extension's.
//!  - **A stop never waits on a vendor.** [`Run::on_stop`] gets
//!    [`STOP_BUDGET`] in total and is then abandoned.

// The interface is wider than the extensions that exist today use: it is
// what every future extension is written against, and trimming it to fit
// the one test fixture would make the second extension re-open this file.
#![allow(dead_code)]

use std::{
    collections::{BTreeMap, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use homerun_core::engine::{extensions as specs, extensions::ExtensionSpec, GameDescriptor};
use homerun_supervisor::{
    engine::{Engine, StopSignal},
    process_engine::ProcessEngine,
    rcon,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    prepare::{fail, Failure, Result},
    protocol::{codes, Event},
    runner::Output,
};

#[cfg(feature = "test-extensions")]
mod fixture;
#[cfg(all(test, windows, feature = "test-extensions"))]
mod harness;
mod hytale;
mod prompt;
mod store;

pub use prompt::{Choice, Prompt, Prompts};
pub use store::{MachineStore, ServerStore};
// What an extension builds its requests from. Part of the interface whether
// or not today's extensions use every name.
#[allow(unused_imports)]
pub use homerun_supervisor::vendor_http::{Method, Request, Response};

use homerun_supervisor::vendor_http::{self, Policy};

/// `http()` also reaches `http://127.0.0.1`, for the lifecycle tests' fake
/// vendor. Never in a release build.
const LOOPBACK_HTTP: bool = cfg!(feature = "test-extensions");

/// How long [`Run::on_stop`] may take, in total, before the runner stops
/// waiting for it.
pub const STOP_BUDGET: Duration = Duration::from_secs(10);

/// The least time between two console commands one run's extension sends.
const CONSOLE_INTERVAL: Duration = Duration::from_secs(1);

/// How long the same console command is not sent again. A server that says
/// "not signed in" on every tick must not start a new sign-in on every tick.
const CONSOLE_REPEAT: Duration = Duration::from_secs(30);

/// Actions waiting for the worker. Past this, new ones are dropped: an
/// observer producing faster than a console can take them is a bug, and the
/// output pump must never wait for the worker.
const ACTION_QUEUE: usize = 64;

// ─── what an extension implements ──────────────────────────────────────────

/// The effects half of one game's extension. See the module header.
pub trait GameExtension: Send + Sync {
    /// Matches the pure half's `ExtensionSpec::name`.
    fn name(&self) -> &'static str;

    /// Before a start: after the fetch, before any configuration is written
    /// or the launch is composed.
    ///
    /// Runs on the lifecycle thread and may block -- waiting for a person to
    /// sign in, say -- but must return promptly once [`StartContext::stopping`]
    /// is true. Returns the values `{extension:<key>}` asks for, and the
    /// [`Run`] that sees this run through.
    fn begin(&self, ctx: &mut StartContext) -> std::result::Result<Begun, ExtError>;

    /// Delete everything this extension keeps on this machine: the host's
    /// "Sign out". No server needs to be running.
    fn forget(&self, _ctx: &MachineContext) -> std::result::Result<(), ExtError> {
        Ok(())
    }

    /// What the host may show without starting anything.
    fn status(&self, _ctx: &MachineContext) -> ExtensionStatus {
        ExtensionStatus::default()
    }
}

/// One run of one server. Every method has a default that does nothing.
pub trait Run: Send {
    /// One line of the server's output, already decoded and redacted.
    /// Runs on the output pump: return actions, never block.
    fn on_line(&mut self, _line: &str, _stream: &str) -> Vec<Action> {
        vec![]
    }

    /// A lifecycle change. Never blocks either.
    fn on_state(&mut self, _state: RunState) -> Vec<Action> {
        vec![]
    }

    /// After the whole process tree has exited, whatever the outcome.
    /// Best effort, within [`STOP_BUDGET`].
    fn on_stop(&mut self, _ctx: &StopContext, _outcome: Outcome) {}
}

/// What `begin` hands back.
pub struct Begun {
    /// `{extension:<key>}` values. Every key must be one the spec declares.
    pub supplied: BTreeMap<String, String>,
    pub run: Box<dyn Run>,
}

/// Something an observer asks the runner to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send a command to the server's console, stdin or RCON.
    Console(String),
    /// Show the person a sign-in card.
    SignIn(SignIn),
    /// The sign-in for `purpose` is done; the host closes its card.
    SignedIn(Purpose),
    /// A line from the runner, shown with the server's output.
    Note(String),
    /// Stop this server, and tell the person why.
    Fail(ExtError),
}

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Ready, and every declared port bound: what `server-started` means.
    Started,
    /// A stop was asked for.
    Stopping,
}

/// How a run ended, as the runner reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Stopped,
    Crashed,
}

/// What a sign-in is for. The host words its card by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Purpose {
    /// Signing in so the game's files can be downloaded.
    Download,
    /// Signing in so the running server admits players.
    Server,
}

/// A sign-in the person has to complete in their own browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignIn {
    pub purpose: Purpose,
    /// Must pass `url_allowed` against the spec's hosts, or it is not shown.
    pub url: String,
    /// The code to type, when the URL does not already carry it.
    pub code: Option<String>,
    /// How long the code lasts, for a countdown.
    pub expires_in_secs: Option<u64>,
}

/// An extension's failure, for a player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtError {
    /// One of the extension codes in `protocol::codes`.
    pub code: &'static str,
    /// A sentence a player can read. Never a diagnostic.
    pub message: String,
}

impl ExtError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// For a `begin` that noticed a stop. The runner reports the stop, not
    /// this.
    pub fn cancelled() -> Self {
        Self::new(codes::EXTENSION_FAILED, "The start was cancelled.")
    }

    fn failure(&self) -> Failure {
        fail(self.code, self.message.clone())
    }
}

/// What an extension says about itself when nothing is running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionStatus {
    /// `None` when the extension keeps no sign-in, or cannot tell.
    pub signed_in: Option<bool>,
    /// A name the person recognises, such as their account's username.
    pub account: Option<String>,
}

// ─── what an extension is given ────────────────────────────────────────────

/// `begin`'s view of the world.
pub struct StartContext<'a> {
    spec: &'static ExtensionSpec,
    config: &'a Value,
    descriptor: &'a GameDescriptor,
    server_id: &'a str,
    stop: &'a StopSignal,
    out: &'a Output,
    machine: MachineStore,
    server: ServerStore,
    policy: Policy,
    prompts: &'a Prompts,
}

impl StartContext<'_> {
    /// The descriptor's `extension.config`, already validated by the spec.
    pub fn config(&self) -> &Value {
        self.config
    }
    pub fn descriptor(&self) -> &GameDescriptor {
        self.descriptor
    }
    pub fn server_id(&self) -> &str {
        self.server_id
    }
    /// Whether the person asked for a stop. A `begin` that waits must check.
    pub fn stopping(&self) -> bool {
        self.stop.should_stop()
    }
    /// Sleep, waking early for a stop. `true` if a stop ended it.
    pub fn wait(&self, duration: Duration) -> bool {
        wait(self.stop, duration)
    }
    /// A progress line while the start is being prepared.
    pub fn note(&self, message: impl Into<String>) {
        self.out.send(Event::FetchProgress {
            server_id: self.server_id.into(),
            phase: "extension".into(),
            received: None,
            total: None,
            message: Some(message.into()),
        });
    }
    /// Show the person a sign-in card. Refused unless the URL is on a host
    /// the spec allows.
    pub fn sign_in(&self, sign_in: SignIn) -> std::result::Result<(), ExtError> {
        let event = sign_in_event(self.spec, self.config, self.server_id, sign_in)?;
        self.out.send(event);
        Ok(())
    }
    /// The sign-in for `purpose` is done.
    pub fn signed_in(&self, purpose: Purpose) {
        self.out.send(Event::SignedIn {
            server_id: self.server_id.into(),
            purpose,
        });
    }
    /// This extension's sealed document on this machine.
    pub fn machine_store(&self) -> &MachineStore {
        &self.machine
    }
    /// This server's plain document for this extension. Not for secrets.
    pub fn server_store(&self) -> &ServerStore {
        &self.server
    }
    /// HTTPS to a host the spec names. Ends early for a stop.
    pub fn http(&self, request: &Request) -> std::result::Result<Response, ExtError> {
        http(request, &self.policy, &|| self.stop.should_stop())
    }
    /// Ask the person to choose, and wait for the answer or a stop.
    pub fn prompt(&self, prompt: Prompt) -> std::result::Result<String, ExtError> {
        self.prompts
            .ask(self.server_id, prompt, self.stop, self.out)
    }
}

/// `on_stop`'s view of the world.
pub struct StopContext {
    config: Value,
    server_id: String,
    deadline: Instant,
    out: Output,
    machine: MachineStore,
    server: ServerStore,
    policy: Policy,
}

impl StopContext {
    pub fn config(&self) -> &Value {
        &self.config
    }
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
    /// What is left of [`STOP_BUDGET`]. Past it, nobody is waiting.
    pub fn time_left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    /// A line from the runner, shown with the server's output.
    pub fn note(&self, message: impl Into<String>) {
        self.out.send(host_line(&self.server_id, message.into()));
    }
    pub fn machine_store(&self) -> &MachineStore {
        &self.machine
    }
    pub fn server_store(&self) -> &ServerStore {
        &self.server
    }
    /// HTTPS to a host the spec names, abandoned when the budget runs out.
    pub fn http(&self, request: &Request) -> std::result::Result<Response, ExtError> {
        http(request, &self.policy, &|| self.time_left().is_zero())
    }
}

/// `forget` and `status`'s view: no server, only the extension itself.
pub struct MachineContext {
    spec: &'static ExtensionSpec,
    machine: MachineStore,
}

impl MachineContext {
    pub fn name(&self) -> &'static str {
        self.spec.name
    }
    pub fn machine_store(&self) -> &MachineStore {
        &self.machine
    }
}

// ─── the registry ──────────────────────────────────────────────────────────

/// Every extension this runner can run. Must name exactly the specs in
/// `engine::extensions::registry()` -- a test holds the two together.
// Pushed one by one because each entry can carry its own `cfg`.
#[allow(clippy::vec_init_then_push)]
pub fn registry() -> Vec<&'static dyn GameExtension> {
    #[allow(unused_mut)]
    let mut all: Vec<&'static dyn GameExtension> = vec![];
    all.push(&hytale::Hytale);
    #[cfg(feature = "test-extensions")]
    all.push(&fixture::Fixture);
    all
}

fn find(name: &str) -> Option<(&'static dyn GameExtension, &'static ExtensionSpec)> {
    let spec = specs::spec(name)?;
    let extension = registry().into_iter().find(|e| e.name() == name)?;
    Some((extension, spec))
}

/// The feature names this runner's extensions add to `ready.features`.
pub fn features() -> Vec<String> {
    let mut out = vec![specs::FEATURE.to_string()];
    out.extend(registry().iter().map(|e| specs::feature(e.name())));
    out
}

// ─── running one ───────────────────────────────────────────────────────────

/// An extension that has begun a run, as the lifecycle holds it.
pub struct Active {
    spec: &'static ExtensionSpec,
    config: Value,
    server_id: String,
    supplied: BTreeMap<String, String>,
    run: Arc<Mutex<Box<dyn Run>>>,
    /// Set once an observer panics: that run's observers are then skipped.
    poisoned: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<Failure>>>,
    actions: Option<mpsc::SyncSender<Action>>,
    worker: Option<JoinHandle<()>>,
    stopping_seen: AtomicBool,
    machine: MachineStore,
    server: ServerStore,
    policy: Policy,
}

/// Call the descriptor's extension's `begin`, if it has one.
///
/// `Ok(None)` for a game with no extension, which is most of them.
#[allow(clippy::too_many_arguments)]
pub fn begin(
    d: &GameDescriptor,
    server_id: &str,
    runtime_root: &Path,
    server_dir: &Path,
    stop: &StopSignal,
    out: &Output,
    prompts: &Prompts,
) -> Result<Option<Active>> {
    let Some(named) = &d.extension else {
        return Ok(None);
    };
    // `validate` has refused a name missing from the core registry already.
    // Missing here too means the two registries disagree, which a test
    // prevents; still a refusal in words rather than a panic.
    let Some((extension, spec)) = find(&named.name) else {
        return Err(fail(
            codes::DESCRIPTOR_INVALID,
            format!(
                "This game needs a part of Homerun called \"{}\" that this version does not \
                 have. Updating Homerun should fix it.",
                named.name
            ),
        ));
    };
    let config = match &named.config {
        Value::Null => Value::Object(Default::default()),
        other => other.clone(),
    };
    let machine = MachineStore::new(&crate::prepare::tools_dir(runtime_root), spec.name);
    let server = ServerStore::new(server_dir, spec.name);
    let policy = policy(spec, &config);
    let begun = {
        let mut ctx = StartContext {
            spec,
            config: &config,
            descriptor: d,
            server_id,
            stop,
            out,
            machine: machine.clone(),
            server: server.clone(),
            policy: policy.clone(),
            prompts,
        };
        match catch_unwind(AssertUnwindSafe(|| extension.begin(&mut ctx))) {
            Ok(Ok(begun)) => begun,
            Ok(Err(e)) => return Err(e.failure()),
            Err(_) => {
                eprintln!(
                    "The \"{}\" extension panicked while preparing a start.",
                    spec.name
                );
                return Err(fail(
                    codes::EXTENSION_FAILED,
                    "Homerun hit a problem getting this game ready. Try starting it again.",
                ));
            }
        }
    };
    if let Some(key) = begun.supplied.keys().find(|k| spec.supply(k).is_none()) {
        eprintln!(
            "The \"{}\" extension supplied \"{key}\", which its spec does not declare.",
            spec.name
        );
        return Err(fail(
            codes::EXTENSION_FAILED,
            "Homerun hit a problem getting this game ready. Try starting it again.",
        ));
    }
    Ok(Some(Active {
        spec,
        config,
        server_id: server_id.into(),
        supplied: begun.supplied,
        run: Arc::new(Mutex::new(begun.run)),
        poisoned: Arc::new(AtomicBool::new(false)),
        failure: Arc::new(Mutex::new(None)),
        actions: None,
        worker: None,
        stopping_seen: AtomicBool::new(false),
        machine,
        server,
        policy,
    }))
}

impl Active {
    /// The values for `{extension:<key>}`.
    pub fn supplied(&self) -> &BTreeMap<String, String> {
        &self.supplied
    }

    /// Every supplied value the spec marks secret, for redaction.
    pub fn secret_values(&self) -> Vec<String> {
        self.supplied
            .iter()
            .filter(|(k, v)| !v.is_empty() && self.spec.supply(k).is_some_and(|s| s.secret))
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Start the worker that carries out actions, once the server exists.
    pub fn attach(
        &mut self,
        engine: Arc<ProcessEngine>,
        console: Option<rcon::Target>,
        stop: StopSignal,
        out: Output,
    ) {
        let (tx, rx) = mpsc::sync_channel::<Action>(ACTION_QUEUE);
        let worker = Worker {
            spec: self.spec,
            config: self.config.clone(),
            server_id: self.server_id.clone(),
            engine,
            console,
            stop,
            out,
            failure: self.failure.clone(),
            sent: VecDeque::new(),
            last_console: None,
        };
        self.actions = Some(tx);
        self.worker = thread::Builder::new()
            .name("game-extension".into())
            .spawn(move || worker.run(rx))
            .ok();
    }

    /// One redacted output line.
    pub fn on_line(&self, line: &str, stream: &str) {
        self.observe(|run| run.on_line(line, stream));
    }

    /// Ready and bound.
    pub fn started(&self) {
        self.observe(|run| run.on_state(RunState::Started));
    }

    /// The first time a stop is seen, and only then.
    pub fn stopping(&self) {
        if !self.stopping_seen.swap(true, Ordering::SeqCst) {
            self.observe(|run| run.on_state(RunState::Stopping));
        }
    }

    /// Why an action stopped the server, if one did.
    pub fn failure(&self) -> Option<Failure> {
        self.failure.lock().unwrap().clone()
    }

    /// End the run: drain the worker, then give `on_stop` its budget.
    pub fn finish(mut self, outcome: Outcome, out: &Output) {
        self.actions = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if self.poisoned.load(Ordering::SeqCst) {
            return;
        }
        let ctx = StopContext {
            config: self.config.clone(),
            server_id: self.server_id.clone(),
            deadline: Instant::now() + STOP_BUDGET,
            out: out.clone(),
            machine: self.machine.clone(),
            server: self.server.clone(),
            policy: self.policy.clone(),
        };
        let run = self.run.clone();
        let name = self.spec.name;
        let (done, finished) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("game-extension-stop".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run.lock().unwrap().on_stop(&ctx, outcome);
                }));
                if result.is_err() {
                    eprintln!("The \"{name}\" extension panicked while a server stopped.");
                }
                let _ = done.send(());
            });
        if spawned.is_ok() && finished.recv_timeout(STOP_BUDGET).is_err() {
            eprintln!("The \"{name}\" extension took too long after a server stopped.");
        }
    }

    fn observe(&self, call: impl FnOnce(&mut dyn Run) -> Vec<Action>) {
        if self.poisoned.load(Ordering::SeqCst) {
            return;
        }
        // `try_lock`: an observer never waits, and the only other holder is
        // `on_stop`, after the server has gone.
        let Ok(mut run) = self.run.try_lock() else {
            return;
        };
        match catch_unwind(AssertUnwindSafe(|| call(run.as_mut()))) {
            Ok(actions) => {
                if let Some(tx) = &self.actions {
                    for action in actions {
                        // Full queue: drop rather than stall the pump.
                        let _ = tx.try_send(action);
                    }
                }
            }
            Err(_) => {
                self.poisoned.store(true, Ordering::SeqCst);
                eprintln!(
                    "The \"{}\" extension panicked watching a server; its checks are off \
                     for the rest of this run.",
                    self.spec.name
                );
            }
        }
    }
}

/// Carries out one run's actions, off the output pump.
struct Worker {
    spec: &'static ExtensionSpec,
    config: Value,
    server_id: String,
    engine: Arc<ProcessEngine>,
    console: Option<rcon::Target>,
    stop: StopSignal,
    out: Output,
    failure: Arc<Mutex<Option<Failure>>>,
    /// Console commands sent recently, for [`CONSOLE_REPEAT`].
    sent: VecDeque<(Instant, String)>,
    last_console: Option<Instant>,
}

impl Worker {
    fn run(mut self, actions: mpsc::Receiver<Action>) {
        for action in actions {
            match action {
                Action::Console(command) => self.console(command),
                Action::SignIn(sign_in) => {
                    match sign_in_event(self.spec, &self.config, &self.server_id, sign_in) {
                        Ok(event) => self.out.send(event),
                        Err(e) => eprintln!(
                            "The \"{}\" extension asked to show a sign-in it may not: {}",
                            self.spec.name, e.message
                        ),
                    }
                }
                Action::SignedIn(purpose) => self.out.send(Event::SignedIn {
                    server_id: self.server_id.clone(),
                    purpose,
                }),
                Action::Note(message) => self.out.send(host_line(&self.server_id, message)),
                Action::Fail(error) => {
                    let mut failure = self.failure.lock().unwrap();
                    if failure.is_none() {
                        *failure = Some(error.failure());
                    }
                    self.stop.request_stop();
                }
            }
        }
    }

    fn console(&mut self, command: String) {
        if self.stop.should_stop() || command.trim().is_empty() {
            return;
        }
        let now = Instant::now();
        self.sent
            .retain(|(at, _)| now.duration_since(*at) < CONSOLE_REPEAT);
        if self.sent.iter().any(|(_, c)| *c == command) {
            return;
        }
        if let Some(last) = self.last_console {
            let wait = CONSOLE_INTERVAL.saturating_sub(now.duration_since(last));
            if !wait.is_zero() && self::wait(&self.stop, wait) {
                return;
            }
        }
        let result = match &self.console {
            Some(target) => rcon::command(target, &command).map(|_| ()),
            None => self.engine.command(&command),
        };
        self.last_console = Some(Instant::now());
        self.sent.push_back((Instant::now(), command));
        if result.is_err() {
            eprintln!(
                "The \"{}\" extension's console command was not accepted.",
                self.spec.name
            );
        }
    }
}

// ─── commands that need no server ──────────────────────────────────────────

/// `extension-status`. `runtime_root` is the one `fetch` and `start` are
/// given: the machine store lives beside the runtimes, as steamcmd does.
pub fn status(name: &str, runtime_root: &Path) -> std::result::Result<Event, Failure> {
    let (extension, spec) = find(name).ok_or_else(|| missing(name))?;
    let ctx = machine_context(spec, runtime_root);
    let status = catch_unwind(AssertUnwindSafe(|| extension.status(&ctx))).unwrap_or_default();
    Ok(Event::ExtensionStatus {
        extension: name.into(),
        signed_in: status.signed_in,
        account: status.account,
    })
}

/// `extension-forget`: forget, then report the status that leaves.
pub fn forget(name: &str, runtime_root: &Path) -> std::result::Result<Event, Failure> {
    let (extension, spec) = find(name).ok_or_else(|| missing(name))?;
    let ctx = machine_context(spec, runtime_root);
    match catch_unwind(AssertUnwindSafe(|| extension.forget(&ctx))) {
        Ok(Ok(())) => status(name, runtime_root),
        Ok(Err(e)) => Err(e.failure()),
        Err(_) => Err(fail(
            codes::EXTENSION_FAILED,
            "Homerun could not sign out of this game's account. Try again.",
        )),
    }
}

fn machine_context(spec: &'static ExtensionSpec, runtime_root: &Path) -> MachineContext {
    MachineContext {
        spec,
        machine: MachineStore::new(&crate::prepare::tools_dir(runtime_root), spec.name),
    }
}

fn missing(name: &str) -> Failure {
    fail(
        codes::REQUIRES_UNMET,
        format!("This version of Homerun does not include \"{name}\"."),
    )
}

// ─── shared ────────────────────────────────────────────────────────────────

fn sign_in_event(
    spec: &ExtensionSpec,
    config: &Value,
    server_id: &str,
    sign_in: SignIn,
) -> std::result::Result<Event, ExtError> {
    if !specs::url_allowed(&sign_in.url, &(spec.hosts)(config)) {
        return Err(ExtError::new(
            codes::EXTENSION_FAILED,
            "This game asked to send you to a sign-in page Homerun does not trust, so it \
             was not shown.",
        ));
    }
    Ok(Event::SignIn {
        server_id: server_id.into(),
        purpose: sign_in.purpose,
        url: sign_in.url,
        code: sign_in.code,
        expires_in_secs: sign_in.expires_in_secs,
    })
}

fn policy(spec: &ExtensionSpec, config: &Value) -> Policy {
    Policy {
        hosts: (spec.hosts)(config),
        loopback_http: LOOPBACK_HTTP,
    }
}

fn http(
    request: &Request,
    policy: &Policy,
    cancelled: &dyn Fn() -> bool,
) -> std::result::Result<Response, ExtError> {
    vendor_http::send(request, policy, cancelled).map_err(|failure| match failure {
        vendor_http::Failure::NotAllowed => ExtError::new(
            codes::EXTENSION_FAILED,
            "This game tried to reach a site Homerun does not trust, so it was not contacted.",
        ),
        vendor_http::Failure::Cancelled => ExtError::cancelled(),
        vendor_http::Failure::Unreachable(message) => {
            ExtError::new(codes::VENDOR_UNAVAILABLE, message)
        }
    })
}

fn host_line(server_id: &str, line: String) -> Event {
    Event::ServerLog {
        server_id: server_id.into(),
        line,
        stream: "host".into(),
    }
}

/// Sleep in short steps, waking for a stop. `true` if a stop ended it.
fn wait(stop: &StopSignal, duration: Duration) -> bool {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if stop.should_stop() {
            return true;
        }
        thread::sleep(Duration::from_millis(25).min(until - Instant::now()));
    }
    stop.should_stop()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A runtime root of its own, under a fresh temporary directory, so each
    /// test's machine store is its own.
    fn runtime_root() -> std::path::PathBuf {
        use std::sync::atomic::AtomicUsize;
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "homerun-ext-contract-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        root.join("runtime")
    }

    // ─── contract: every registered extension, not only the fixture ───────

    /// Sign-out means signed out, for every extension there is.
    #[cfg(windows)]
    #[test]
    fn every_extension_is_signed_out_after_forget() {
        for extension in registry() {
            let spec = specs::spec(extension.name()).unwrap();
            let root = runtime_root();
            let ctx = machine_context(spec, &root);
            extension.forget(&ctx).unwrap();
            assert_ne!(
                extension.status(&ctx).signed_in,
                Some(true),
                "{}",
                extension.name()
            );
            assert!(
                !crate::prepare::tools_dir(&root)
                    .join("extensions")
                    .join(extension.name())
                    .join("store.bin")
                    .exists(),
                "{} left its store behind",
                extension.name()
            );
        }
    }

    /// `status` answers even with nothing kept and nothing running.
    #[test]
    fn every_extension_answers_status_on_a_fresh_machine() {
        for extension in registry() {
            let spec = specs::spec(extension.name()).unwrap();
            let _ = extension.status(&machine_context(spec, &runtime_root()));
        }
    }

    /// The two halves are registered separately; this holds them together.
    #[test]
    fn every_spec_has_exactly_one_implementation_and_the_reverse() {
        let specs: BTreeSet<&str> = specs::registry().iter().map(|s| s.name).collect();
        let effects: Vec<&str> = registry().iter().map(|e| e.name()).collect();
        let unique: BTreeSet<&str> = effects.iter().copied().collect();
        assert_eq!(
            unique.len(),
            effects.len(),
            "a name registered twice: {effects:?}"
        );
        // The core's registry carries its test fixture whenever the core is
        // built for tests or with `test-extensions`; this crate's does only
        // with the feature. Compare on what this build can run.
        let runnable: BTreeSet<&str> = if cfg!(feature = "test-extensions") {
            specs.clone()
        } else {
            specs.iter().copied().filter(|n| *n != "fixture").collect()
        };
        assert_eq!(unique, runnable);
        for name in specs::PUBLISHED.iter().map(|s| s.name) {
            assert!(
                unique.contains(name),
                "{name} is published but not runnable"
            );
        }
    }

    #[test]
    fn features_name_the_mechanism_and_each_extension() {
        let features = features();
        assert_eq!(features[0], "extensions");
        for extension in registry() {
            assert!(features.contains(&format!("extension:{}", extension.name())));
        }
    }

    #[test]
    fn a_wait_ends_early_for_a_stop() {
        let stop = StopSignal::default();
        let begun = Instant::now();
        let s = stop.clone();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            s.request_stop();
        });
        assert!(wait(&stop, Duration::from_secs(10)));
        assert!(begun.elapsed() < Duration::from_secs(2));
        t.join().unwrap();
        assert!(!wait(&StopSignal::default(), Duration::from_millis(10)));
    }
}

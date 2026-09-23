//! A single owned server; stdin remains responsive while fetching or running.
use crate::{
    prepare::{self, fail, Result},
    protocol::{codes, Command, Event, Player, ServerStatus, FEATURES, PROTOCOL},
};
use homerun_core::engine::{
    self,
    descriptor::{PlayersVia, ServerSignIn, SignIn},
};
use homerun_supervisor::{
    engine::{Engine, RunOutcome, RunRequest, StopSignal},
    fetcher,
    // Aliased: `Job` below is this file's fetch/start worker, which is a
    // different thing entirely from an owned process tree.
    job::Job as ProcessJob,
    process_engine::ProcessEngine,
    rcon,
};
use std::{
    collections::VecDeque,
    io::{BufRead, Write},
    process::{Child, Command as Process, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Where a server that must be signed in to its vendor's service has got to.
/// See `descriptor::ServerSignIn`.
#[derive(Debug, Default)]
struct SignInState {
    /// The server said it is not signed in.
    needed: bool,
    /// The sign-in command has been sent for this round. Cleared only when
    /// the server says it is signed in, so a repeated "not signed in" line
    /// cannot start a second sign-in while the first is waiting on the person.
    sent: bool,
    /// `server-signed-in` has been sent and nothing has said otherwise since.
    /// Hytale prints its success line twice; the host hears it once.
    signed_in: bool,
    url: Option<String>,
    code: Option<String>,
}

/// Read one console line for the sign-in states it may announce.
fn observe_sign_in(
    s: &ServerSignIn,
    line: &str,
    state: &Mutex<SignInState>,
    out: &Output,
    id: &str,
) {
    let mut st = state.lock().unwrap();
    if !s.done.is_empty() && line.contains(&s.done) {
        let already = st.signed_in;
        *st = SignInState {
            signed_in: true,
            ..SignInState::default()
        };
        if !already {
            out.send(Event::ServerSignedIn {
                server_id: id.into(),
            });
        }
        return;
    }
    if !s.needed.is_empty() && line.contains(&s.needed) {
        st.needed = true;
        st.signed_in = false;
    }
    let markers = SignIn {
        url: s.url.clone(),
        code: s.code.clone(),
    };
    let (url, code) = fetcher::sign_in_parts(line, &markers);
    // The same rule as a downloader's prompt: a bare address never replaces
    // the one with the code already in it.
    let url = url.filter(|new| fetcher::replaces_sign_in_url(st.url.as_deref(), new));
    let changed = url.is_some() || (code.is_some() && code != st.code);
    if url.is_some() {
        st.url = url;
    }
    if code.is_some() {
        st.code = code;
    }
    if changed {
        if let Some(url) = &st.url {
            out.send(Event::ServerSignIn {
                server_id: id.into(),
                url: url.clone(),
                code: st.code.clone(),
            });
        }
    }
}

#[derive(Clone)]
pub struct Output(Arc<dyn Fn(Event) + Send + Sync>);
impl Output {
    pub fn new(f: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
    pub fn send(&self, event: Event) {
        (self.0)(event)
    }
    pub fn error(&self, id: Option<&str>, failure: prepare::Failure) {
        self.send(Event::Error {
            server_id: id.map(str::to_owned),
            code: failure.0.into(),
            message: failure.1,
        });
    }
}

struct Live {
    state: String,
    engine: Option<Arc<ProcessEngine>>,
    console: Option<rcon::Target>,
}
struct Job {
    id: String,
    stop: StopSignal,
    live: Arc<Mutex<Live>>,
    task: JoinHandle<()>,
}
pub struct Runner {
    out: Output,
    job: Option<Job>,
    /// The wireproxy child, and the job that owns it and anything it starts.
    /// Same ownership the game gets: a runner that is hard-killed must not
    /// leave a tunnel behind holding a gateway connection nothing is serving.
    tunnel: Option<(String, Child, Option<ProcessJob>)>,
    console_tasks: Vec<JoinHandle<()>>,
    build: String,
    pub(crate) network: crate::network::Audit,
}

impl Runner {
    pub fn new(out: Output) -> std::result::Result<Self, String> {
        let exe = std::env::current_exe()
            .map_err(|_| "The runner cannot identify its build.".to_string())?;
        let digest = fetcher::digest_of(&exe)?;
        Ok(Self {
            out,
            job: None,
            tunnel: None,
            console_tasks: vec![],
            build: digest[..12].into(),
            network: crate::network::Audit::default(),
        })
    }
    pub fn ready(&self) {
        self.out.send(Event::Ready {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            build: self.build.clone(),
            features: FEATURES.iter().map(|f| f.to_string()).collect(),
        });
    }
    pub fn idle(&self) -> bool {
        self.job.as_ref().is_none_or(|j| j.task.is_finished())
    }
    pub fn tick(&mut self) {
        self.console_tasks.retain(|t| !t.is_finished());
        if self.idle() {
            self.stop_tunnel();
        }
        if let Some((id, child, _)) = &mut self.tunnel {
            if child.try_wait().ok().flatten().is_some() {
                self.out.send(Event::TunnelFailed {
                    server_id: id.clone(),
                    message: "The connection to the gateway stopped.".into(),
                });
                self.tunnel = None;
            }
        }
    }
    fn stop_tunnel(&mut self) {
        if let Some((_, mut child, job)) = self.tunnel.take() {
            match &job {
                // Takes wireproxy and anything it started, which
                // `Child::kill` -- one process, no tree -- would not.
                Some(job) => job.terminate(),
                None => {
                    let _ = child.kill();
                }
            }
            let _ = child.wait();
        }
    }
    pub fn shutdown(&mut self) {
        if let Some(job) = self.job.take() {
            job.stop.request_stop();
            let _ = job.task.join();
        }
        self.stop_tunnel();
        for task in self.console_tasks.drain(..) {
            let _ = task.join();
        }
        self.out.send(Event::ShutdownComplete);
    }
    pub fn command(&mut self, command: Command) -> bool {
        self.tick();
        match command {
            Command::Hello { protocol } => {
                if protocol == PROTOCOL {
                    self.ready();
                } else {
                    eprintln!("Unsupported runner protocol {protocol}; expected {PROTOCOL}.");
                    return false;
                }
            }
            Command::Unknown => eprintln!("Ignoring a command this runner does not recognize."),
            Command::Shutdown => return false,
            Command::Status => self.out.send(Event::Status {
                servers: self
                    .job
                    .iter()
                    .map(|j| ServerStatus {
                        server_id: j.id.clone(),
                        state: j.live.lock().unwrap().state.clone(),
                    })
                    .collect(),
            }),
            Command::Stop { server_id } => {
                if let Some(job) = self
                    .job
                    .as_ref()
                    .filter(|j| j.id == server_id && !j.task.is_finished())
                {
                    job.live.lock().unwrap().state = "stopping".into();
                    job.stop.request_stop();
                } else {
                    self.out.send(Event::ServerStopped {
                        server_id,
                        code: None,
                    });
                }
            }
            Command::Console {
                server_id,
                command,
                req_id,
            } => {
                let live = self
                    .job
                    .as_ref()
                    .filter(|j| j.id == server_id && !j.task.is_finished())
                    .map(|j| j.live.clone());
                if let Some(live) = live {
                    if self.console_tasks.len() >= 8 {
                        self.out.send(Event::ConsoleResponse {
                            server_id,
                            req_id,
                            response: "The server is still answering earlier console commands."
                                .into(),
                        });
                    } else {
                        let out = self.out.clone();
                        self.console_tasks.push(thread::spawn(move || {
                            let (engine, target) = {
                                let l = live.lock().unwrap();
                                (l.engine.clone(), l.console.clone())
                            };
                            let result = match (engine, target) {
                                (Some(_), Some(target)) => rcon::command(&target, &command),
                                (Some(engine), None) => {
                                    engine.command(&command).map(|_| String::new())
                                }
                                _ => Err("The server is not accepting commands yet.".into()),
                            };
                            // The v1 error event has no reqId. Complete the request even
                            // on refusal so a desktop promise is never stranded.
                            let response = result.unwrap_or_else(|e| e);
                            out.send(Event::ConsoleResponse {
                                server_id,
                                req_id,
                                response,
                            });
                        }));
                    }
                } else {
                    self.out.send(Event::ConsoleResponse {
                        server_id,
                        req_id,
                        response: "The server is not running.".into(),
                    });
                }
            }
            Command::StartTunnel {
                server_id,
                bin_path,
                conf_path,
            } => {
                let running = self.job.as_ref().is_some_and(|j| {
                    j.id == server_id && j.live.lock().unwrap().state == "running"
                });
                if !running || self.tunnel.is_some() {
                    self.out.send(Event::TunnelFailed { server_id, message: "Start the server before opening its gateway connection, and open only one connection.".into() });
                } else {
                    let job = ProcessJob::kill_on_close();
                    match Process::new(bin_path)
                        .args(["-c", &conf_path])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        Ok(child) => {
                            if let Some(job) = &job {
                                job.adopt(&child);
                            }
                            self.tunnel = Some((server_id.clone(), child, job));
                            self.out.send(Event::TunnelStarted { server_id });
                        }
                        Err(_) => self.out.send(Event::TunnelFailed {
                            server_id,
                            message: "The gateway connection could not be started.".into(),
                        }),
                    }
                }
            }
            command @ (Command::Fetch { .. } | Command::Start { .. }) => self.begin(command),
        }
        true
    }
    fn begin(&mut self, command: Command) {
        let (id, value, accepted) = match &command {
            Command::Fetch {
                server_id,
                descriptor,
                licence_accepted,
                ..
            }
            | Command::Start {
                server_id,
                descriptor,
                licence_accepted,
                ..
            } => (server_id.clone(), descriptor.clone(), *licence_accepted),
            _ => unreachable!(),
        };
        let d = match prepare::descriptor(value, accepted) {
            Ok(d) => d,
            Err(e) => {
                self.out.error(Some(&id), e);
                return;
            }
        };
        if !self.idle() {
            self.out.error(
                Some(&id),
                fail(
                    codes::BUSY,
                    "This runner is already fetching or running a server.",
                ),
            );
            return;
        }
        if let Some(old) = self.job.take() {
            let _ = old.task.join();
        }
        let stop = StopSignal::default();
        let live = Arc::new(Mutex::new(Live {
            state: "starting".into(),
            engine: None,
            console: None,
        }));
        let (s, l, out, server_id) = (stop.clone(), live.clone(), self.out.clone(), id.clone());
        let audit = self.network.clone();
        audit.clear();
        let task = thread::Builder::new()
            .name("game-lifecycle".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let result = run(command, &d, &s, &l, &out, &audit);
                if let Err(e) = result {
                    if s.should_stop() {
                        l.lock().unwrap().state = "stopped".into();
                        out.send(Event::ServerStopped {
                            server_id: server_id.clone(),
                            code: None,
                        });
                    } else {
                        out.error(Some(&server_id), e);
                        l.lock().unwrap().state = "crashed".into();
                    }
                }
                l.lock().unwrap().engine = None;
            });
        match task {
            Ok(task) => {
                self.job = Some(Job {
                    id,
                    stop,
                    live,
                    task,
                })
            }
            Err(_) => self.out.error(
                Some(&id),
                fail(
                    codes::SPAWN_FAILED,
                    "This computer could not start a server worker.",
                ),
            ),
        }
    }
}

fn run(
    command: Command,
    d: &engine::GameDescriptor,
    stop: &StopSignal,
    live: &Arc<Mutex<Live>>,
    out: &Output,
    audit: &crate::network::Audit,
) -> Result<()> {
    let (id, root, version) = match &command {
        Command::Fetch {
            server_id,
            runtime_root,
            runtime_version,
            ..
        }
        | Command::Start {
            server_id,
            runtime_root,
            runtime_version,
            ..
        } => (
            server_id.clone(),
            prepare::absolute(runtime_root)?,
            prepare::runtime_version(d, runtime_version.as_deref())?,
        ),
        _ => unreachable!(),
    };
    let mut runtime_owner = crate::runtime::Runtime::acquire(d, &root, version.as_deref())?;
    let runtime = prepare::fetch(d, &root, version.as_deref(), &id, out, stop)?;
    let Command::Start {
        server_dir,
        server_name,
        settings,
        secrets,
        bind_address,
        ..
    } = command
    else {
        live.lock().unwrap().state = "stopped".into();
        return Ok(());
    };
    if stop.should_stop() {
        live.lock().unwrap().state = "stopped".into();
        out.send(Event::ServerStopped {
            server_id: id,
            code: None,
        });
        return Ok(());
    }
    let server = prepare::absolute(&server_dir)?;
    let mut p = prepare::launch(
        d,
        &runtime,
        &server,
        &server_name,
        &settings,
        &secrets,
        bind_address.as_deref(),
    )?;
    runtime_owner.install(d, &server)?;
    if let Some(job) = runtime_owner.process_job() {
        p.engine.require_job(job);
    } else {
        #[cfg(windows)]
        p.engine.require_job(Arc::new(
            ProcessJob::required_process().map_err(|e| fail(codes::SPAWN_FAILED, e))?,
        ));
    }
    let engine = Arc::new(p.engine);
    {
        let mut l = live.lock().unwrap();
        l.engine = Some(engine.clone());
        l.console = p.console.clone();
    }
    let ready = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let exposed = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let tail = Mutex::new(VecDeque::new());
    let observed_ports = Arc::new(Mutex::new(Vec::new()));
    let network_failure = Arc::new(Mutex::new(None));
    let sign_in = Arc::new(Mutex::new(SignInState::default()));
    let monitor = {
        let (done, timed_out, exposed, started) = (
            done.clone(),
            timed_out.clone(),
            exposed.clone(),
            started.clone(),
        );
        let (engine, stop, d) = (engine.clone(), stop.clone(), d.clone());
        let observed_ports = observed_ports.clone();
        let network_failure = network_failure.clone();
        let audit = audit.clone();
        thread::spawn(move || {
            let begin = Instant::now();

            let mut next_port_check = Instant::now();
            while !done.load(Ordering::SeqCst) {
                if Instant::now() >= next_port_check {
                    next_port_check = Instant::now() + Duration::from_millis(250);
                    let snapshot = engine.network_snapshot();
                    let observed = match snapshot {
                        Ok(Some(ref endpoints)) => {
                            if let Err((code, message)) =
                                audit.check(&d, endpoints, begin.elapsed())
                            {
                                exposed.store(true, Ordering::SeqCst);
                                *network_failure.lock().unwrap() = Some(fail(code, message));
                                if let Err(e) = engine.terminate_owned_tree() {
                                    audit.inspection_failed(&e);
                                    *network_failure.lock().unwrap() =
                                        Some(fail(codes::PORT_INSPECTION_FAILED, "The game could not be inspected or stopped safely. This run cannot continue."));
                                }
                                stop.request_stop();
                                break;
                            }
                            endpoints.iter().map(|(_, l)| *l).collect::<Vec<_>>()
                        }
                        Ok(None) => Vec::new(),
                        Err(e) => {
                            audit.inspection_failed(&e);
                            exposed.store(true, Ordering::SeqCst);
                            *network_failure.lock().unwrap() =
                                Some(fail(codes::PORT_INSPECTION_FAILED, "The game could not be inspected or stopped safely. This run cannot continue."));
                            if let Err(e) = engine.terminate_owned_tree() {
                                audit.inspection_failed(&e);
                                *network_failure.lock().unwrap() =
                                    Some(fail(codes::PORT_INSPECTION_FAILED, "The game could not be inspected or stopped safely. This run cannot continue."));
                            }
                            stop.request_stop();
                            break;
                        }
                    };
                    *observed_ports.lock().unwrap() = observed;
                }

                if !started.load(Ordering::SeqCst)
                    && !stop.should_stop()
                    && begin.elapsed()
                        >= Duration::from_millis(engine::control::ready_timeout_ms(&d))
                {
                    timed_out.store(true, Ordering::SeqCst);
                    stop.request_stop();
                }
                thread::sleep(Duration::from_millis(25));
            }
        })
    };
    let sampler = {
        let (engine, stop, out, id, done, started, d, console, ready, live, exposed) = (
            engine.clone(),
            stop.clone(),
            out.clone(),
            id.clone(),
            done.clone(),
            started.clone(),
            d.clone(),
            p.console.clone(),
            ready.clone(),
            live.clone(),
            exposed.clone(),
        );
        let ports = p.ports.clone();
        let observed_ports = observed_ports.clone();
        let sign_in = sign_in.clone();
        thread::spawn(move || {
            let mut sampled = Instant::now();
            while !done.load(Ordering::SeqCst) {
                // A server that is up and says it is not signed in is asked to
                // start a sign-in, once per round. It prints where to sign in,
                // which observe_sign_in passes on; the person does the rest.
                if let Some(s) = &d.server_sign_in {
                    let due = started.load(Ordering::SeqCst) && !stop.should_stop() && {
                        let mut st = sign_in.lock().unwrap();
                        let due = st.needed && !st.sent;
                        if due {
                            st.sent = true;
                        }
                        due
                    };
                    if due {
                        let sent = match &console {
                            Some(target) => rcon::command(target, &s.command).map(|_| ()),
                            None => engine.command(&s.command),
                        };
                        out.send(Event::ServerLog {
                            server_id: id.clone(),
                            line: match sent {
                                Ok(()) => {
                                    "Homerun asked the server to sign in to its game account."
                                        .into()
                                }
                                Err(e) => {
                                    format!("Homerun could not ask the server to sign in: {e}")
                                }
                            },
                            stream: "host".into(),
                        });
                    }
                }
                if ready.load(Ordering::SeqCst) && !started.load(Ordering::SeqCst) {
                    let all_bound = d.ports.iter().all(|p| {
                        observed_ports
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|o| o.port == p.port && o.protocol == p.proto)
                    });
                    if all_bound {
                        let mut l = live.lock().unwrap();
                        if !stop.should_stop()
                            && !done.load(Ordering::SeqCst)
                            && !exposed.load(Ordering::SeqCst)
                        {
                            out.send(Event::ServerPorts {
                                server_id: id.clone(),
                                ports: ports.clone(),
                            });
                            l.state = "running".into();
                            started.store(true, Ordering::SeqCst);
                            out.send(Event::ServerStarted {
                                server_id: id.clone(),
                            });
                        }
                    }
                }

                if started.load(Ordering::SeqCst)
                    && !stop.should_stop()
                    && sampled.elapsed() >= Duration::from_secs(2)
                {
                    if let Some((rss_kb, cpu_seconds)) = engine.usage() {
                        out.send(Event::Stats {
                            server_id: id.clone(),
                            rss_kb,
                            cpu_seconds,
                        });
                    }
                    if matches!(d.observe.players, PlayersVia::LogRegex) {
                        if let Some((players, max)) = engine.players() {
                            out.send(Event::Players {
                                server_id: id.clone(),
                                count: players.len() as u32,
                                max,
                                players: vec![],
                            });
                        }
                    } else if matches!(d.observe.players, PlayersVia::Rcon) {
                        if let (Some(target), Some(command)) =
                            (&console, &d.observe.players_command)
                        {
                            if let Ok(reply) =
                                rcon::command_with_timeout(target, command, Duration::from_secs(2))
                            {
                                if let Some(players) = player_list(&reply) {
                                    out.send(Event::Players {
                                        server_id: id.clone(),
                                        count: players.len() as u32,
                                        max: None,
                                        players,
                                    });
                                }
                            }
                        }
                    }
                    sampled = Instant::now();
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let outcome = engine.run_streamed(
        &RunRequest {
            server_id: id.clone(),
            data_dir: p.cwd.to_string_lossy().into(),
            java_port: 0,
            local_network: false,
            settings: None,
        },
        stop.clone(),
        &|line, stream| {
            let mut lines = tail.lock().unwrap();
            // Do not publish secrets a vendor echoes with its launch arguments.
            let mut line = line;
            for secret in secrets.values().filter(|s| !s.is_empty()) {
                line = line.replace(secret, "[redacted]");
            }
            if lines.len() == 100 {
                lines.pop_front();
            }
            lines.push_back(line.clone());
            if let Some(s) = &d.server_sign_in {
                observe_sign_in(s, &line, &sign_in, out, &id);
            }
            out.send(Event::ServerLog {
                server_id: id.clone(),
                line,
                stream: stream.into(),
            });
        },
        &|| ready.store(true, Ordering::SeqCst),
    );
    // Root exit does not mean its descendants have exited. Drain ownership before
    // ending enforcement or publishing any terminal event.
    #[cfg(windows)]
    if let Err(e) = engine.terminate_owned_tree() {
        audit.inspection_failed(&e);
        exposed.store(true, Ordering::SeqCst);
        *network_failure.lock().unwrap() = Some(fail(
            codes::PORT_INSPECTION_FAILED,
            "The game could not be inspected or stopped safely. This run cannot continue.",
        ));
    }
    done.store(true, Ordering::SeqCst);
    let _ = monitor.join();
    let _ = sampler.join();
    if let Some(failure) = network_failure.lock().unwrap().take() {
        out.error(Some(&id), failure);
    } else if timed_out.load(Ordering::SeqCst) {
        out.error(
            Some(&id),
            fail(
                codes::READY_TIMEOUT,
                "The game did not become ready on its declared ports in time.",
            ),
        );
    }
    // A launch Homerun refused is neither a stop the player asked for nor a
    // server that failed to start, so it takes the same road as a ready
    // timeout: the error has already been sent, and what follows must not
    // report a clean stop over the top of it.
    let refused = timed_out.load(Ordering::SeqCst) || exposed.load(Ordering::SeqCst);
    let requested = stop.should_stop() && !refused;
    if !refused
        && (requested || (started.load(Ordering::SeqCst) && matches!(outcome, RunOutcome::Stopped)))
    {
        live.lock().unwrap().state = "stopped".into();
        out.send(Event::ServerStopped {
            server_id: id,
            code: None,
        });
    } else {
        live.lock().unwrap().state = "crashed".into();
        if !ready.load(Ordering::SeqCst) && !refused {
            out.error(
                Some(&id),
                fail(
                    codes::SPAWN_FAILED,
                    "The game exited before it became ready. Check its last console lines.",
                ),
            );
        }
        out.send(Event::ServerCrashed {
            server_id: id,
            code: None,
            tail: tail.into_inner().unwrap().into_iter().collect(),
        });
    }
    Ok(())
}

/// Only report a roster when the reply has an explicit supported shape.
/// No UUID fabrication; unknown vendor formats remain unknown.
fn player_list(reply: &str) -> Option<Vec<Player>> {
    let values: Vec<serde_json::Value> = serde_json::from_str(reply).ok()?;
    values
        .iter()
        .map(|v| {
            Some(Player {
                name: v
                    .get("DisplayName")
                    .or_else(|| v.get("name"))?
                    .as_str()?
                    .into(),
                platform_id: v
                    .get("SteamID")
                    .or_else(|| v.get("platformId"))
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                    }),
                platform: v.get("SteamID").map(|_| "steam".into()).or_else(|| {
                    v.get("platform")
                        .and_then(|v| v.as_str().map(str::to_owned))
                }),
            })
        })
        .collect()
}

fn report_malformed(out: &Output, line: &[u8]) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        eprintln!("Ignoring invalid runner JSON.");
        return;
    };
    let Some(cmd) = value.get("cmd").and_then(|v| v.as_str()) else {
        return;
    };
    if !matches!(
        cmd,
        "hello" | "fetch" | "start" | "start-tunnel" | "console" | "stop" | "status" | "shutdown"
    ) {
        return;
    }
    let id = value.get("serverId").and_then(|v| v.as_str());
    // Do not echo serde's error or field values: either may contain a secret.
    let message = format!("The {cmd} request has missing or invalid fields.");
    out.error(id, fail(codes::DESCRIPTOR_INVALID, &message));
    if cmd == "console" {
        if let (Some(id), Some(req_id)) = (id, value.get("reqId").and_then(|v| v.as_str())) {
            out.send(Event::ConsoleResponse {
                server_id: id.into(),
                req_id: Some(req_id.into()),
                response: message,
            });
        }
    }
}

pub fn supervise() -> std::result::Result<(), String> {
    let output = Arc::new(Mutex::new(std::io::stdout()));
    let out = Output::new(move |event| {
        let mut w = output.lock().unwrap();
        let _ = w.write_all(event.line().as_bytes());
        let _ = w.flush();
    });
    let mut runner = Runner::new(out.clone())?;
    let (tx, rx) = mpsc::sync_channel(32);
    thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            // A bounded line prevents a broken host exhausting the runner.
            match std::io::Read::take(&mut input, 1024 * 1024 + 1).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > 1024 * 1024 => {
                    eprintln!("Runner command exceeds one MiB.");
                    break;
                }
                Ok(_) => match serde_json::from_slice::<Command>(&line) {
                    Ok(command) => {
                        if tx.send(command).is_err() {
                            break;
                        }
                    }
                    Err(_) => report_malformed(&out, &line),
                },
            }
        }
    });
    runner.ready();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => {
                if !runner.command(command) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => runner.tick(),
        }
    }
    runner.shutdown();
    Ok(())
}

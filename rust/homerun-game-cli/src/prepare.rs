//! Compose a launch using core decisions, with confined filesystem effects.
use crate::protocol::{codes, Event};
use crate::runner::Output;
use homerun_core::{
    engine::{
        self,
        descriptor::ConfigFormat,
        template::{Bindings, Filled},
        GameDescriptor,
    },
    tunnel::Protocol,
};
use homerun_supervisor::{
    fetcher, platform,
    process_engine::{
        ConsoleRoute, Invocation, Presence, ProcessEngine, Readiness, Rung, Supervision,
    },
    rcon,
};
use std::{
    collections::BTreeMap,
    fs,
    net::{TcpListener, UdpSocket},
    path::{Component, Path, PathBuf},
};

/// The only address a descriptor-driven server is bound to, today.
///
/// Named rather than spelled out at each use so that `{bindAddress}`, the
/// port preflight and the readiness check cannot drift apart -- the last of
/// those refuses a launch on the strength of what the other two promised.
pub const LOOPBACK: &str = "127.0.0.1";

pub type Failure = (&'static str, String);
pub type Result<T> = std::result::Result<T, Failure>;
pub fn fail(code: &'static str, text: impl Into<String>) -> Failure {
    (code, text.into())
}

pub fn descriptor(value: serde_json::Value, accepted: bool) -> Result<GameDescriptor> {
    // The protocol requires explicit acceptance even for descriptors with no terms.
    if !accepted {
        return Err(fail(
            codes::LICENCE_NOT_ACCEPTED,
            "Accept this game's terms before downloading or starting its server.",
        ));
    }
    let d: GameDescriptor = serde_json::from_value(value).map_err(|_| {
        fail(
            codes::DESCRIPTOR_INVALID,
            "This game's descriptor cannot be read. Update Homerun Desktop and try again.",
        )
    })?;
    let report = engine::validate::report(&d);
    if !report.ok() {
        return Err(fail(codes::DESCRIPTOR_INVALID, report.problems.join(" ")));
    }
    if !d.runs_on(platform::HOST) {
        return Err(fail(
            codes::REQUIRES_UNMET,
            "This game cannot be hosted on this computer.",
        ));
    }
    Ok(d)
}

pub fn absolute(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if path.is_empty() {
        return Err(fail(
            codes::SPAWN_FAILED,
            "Choose a folder for this server.",
        ));
    }
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| fail(codes::SPAWN_FAILED, "The current folder cannot be read."))?
            .join(p)
    })
}

/// Reject symlinks as well as lexical escapes before writing managed files.
pub fn confined(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.contains(':') || relative.contains('\\') {
        return Err(fail(
            codes::DESCRIPTOR_INVALID,
            "A game file must use a relative path inside its server folder.",
        ));
    }
    let mut p = root.to_path_buf();
    for part in Path::new(relative).components() {
        match part {
            Component::Normal(name) => p.push(name),
            Component::CurDir => continue,
            _ => {
                return Err(fail(
                    codes::DESCRIPTOR_INVALID,
                    "A game file would leave its server folder.",
                ))
            }
        }
        if fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(fail(
                codes::DESCRIPTOR_INVALID,
                "A game file cannot be written through a symbolic link.",
            ));
        }
    }
    Ok(p)
}

pub fn fetch(
    d: &GameDescriptor,
    root: &Path,
    id: &str,
    out: &Output,
    stop: &homerun_supervisor::engine::StopSignal,
) -> Result<PathBuf> {
    let dir = root.join(&d.id);
    let mut present = fetcher::present(&dir);
    if !platform::executable(&dir, &d.platform(platform::HOST).unwrap().launch.exe).is_file() {
        present.build_id = None;
    }
    let plan = engine::fetch::plan(d, platform::HOST, &root.to_string_lossy(), &present)
        .map_err(|e| fail(codes::FETCH_FAILED, e.to_string()))?;
    let machine = platform::machine_capacity(root);
    let verdict = engine::doctor::doctor(d, &machine, &present, true);
    if !verdict.ok {
        return Err(fail(codes::REQUIRES_UNMET, verdict.problems.join(" ")));
    }
    if (d.requires.ram_mb > 0 && machine.ram_mb == 0)
        || (d.requires.disk_mb > 0 && machine.disk_mb == 0 && !verdict.runtime_present)
    {
        return Err(fail(codes::REQUIRES_UNMET, "This computer's available resources could not be checked. Check disk access and try again."));
    }
    let ctx = fetcher::Context {
        tools_dir: root.parent().unwrap_or(root).to_path_buf(),
        cancelled: &|| stop.should_stop(),
        on_progress: &|progress| {
            let (phase, received, total, message) = match progress {
                fetcher::Progress::Note { phase, message } => (phase, None, None, Some(message)),
                fetcher::Progress::Bytes {
                    phase,
                    received,
                    total,
                } => (phase, Some(received), total, None),
            };
            out.send(Event::FetchProgress {
                server_id: id.into(),
                phase: phase.into(),
                received,
                total,
                message,
            });
        },
    };
    let fetched = fetcher::fetch(&plan, &ctx).map_err(|e| fail(codes::FETCH_FAILED, e))?;
    if stop.should_stop() {
        return Err(fail(codes::FETCH_FAILED, "The download was cancelled."));
    }
    out.send(Event::FetchComplete {
        server_id: id.into(),
        runtime_dir: fetched.dir.to_string_lossy().into(),
        build_id: fetched.build_id,
    });
    Ok(fetched.dir)
}

pub struct Prepared {
    pub engine: ProcessEngine,
    pub cwd: PathBuf,
    pub ports: BTreeMap<String, u16>,
    pub console: Option<rcon::Target>,
}

pub fn launch(
    d: &GameDescriptor,
    runtime: &Path,
    server: &Path,
    name: &str,
    settings: &serde_json::Map<String, serde_json::Value>,
    secrets: &BTreeMap<String, String>,
    bind: Option<&str>,
) -> Result<Prepared> {
    // v1 binds descriptor games on loopback and nowhere else: the tunnel
    // connects to loopback, and a port the descriptor marks `expose: false`
    // has no business anywhere wider. Widening this is a contract change, not
    // a flag. What is new is that the address is now *passed to the game*
    // through `{bindAddress}` rather than validated and dropped -- see
    // `engine::template`.
    let bind = bind.unwrap_or(LOOPBACK);
    if bind != LOOPBACK {
        return Err(fail(
            codes::DESCRIPTOR_INVALID,
            "Game servers must bind to loopback behind the gateway.",
        ));
    }
    fs::create_dir_all(server)
        .map_err(|_| fail(codes::SPAWN_FAILED, "The server folder cannot be created."))?;
    let resolved = engine::settings::resolve(d, settings, name)
        .map_err(|e| fail(codes::DESCRIPTOR_INVALID, e.to_string()))?;
    // Reserve all preferred ports together so duplicate/conflicting declarations
    // fail before spawning. v1 refuses occupied ports instead of guessing a remap.
    let mut tcp = Vec::new();
    let mut udp = Vec::new();
    let mut ports = BTreeMap::new();
    for p in &d.ports {
        let error = |_| {
            fail(
                codes::PORT_UNAVAILABLE,
                format!(
                    "Port {} is already in use. Stop the other server and try again.",
                    p.port
                ),
            )
        };
        match p.proto {
            Protocol::Tcp => tcp.push(TcpListener::bind((LOOPBACK, p.port)).map_err(error)?),
            Protocol::Udp => udp.push(UdpSocket::bind((LOOPBACK, p.port)).map_err(error)?),
        }
        ports.insert(p.name.clone(), p.port);
    }
    let bindings = Bindings {
        settings: &resolved,
        ports: &ports,
        secrets,
        server_name: name,
        server_dir: &server.to_string_lossy(),
        bind_address: bind,
        runtime_dir: &runtime.to_string_lossy(),
    };
    let inv = engine::invocation::compose(d, platform::HOST, &bindings)
        .map_err(|e| fail(codes::DESCRIPTOR_INVALID, e.to_string()))?;
    let cwd_root = match inv.cwd_base {
        engine::descriptor::CwdBase::Server => server,
        engine::descriptor::CwdBase::Runtime => runtime,
    };
    let cwd = confined(cwd_root, &inv.cwd)?;
    fs::create_dir_all(&cwd).map_err(|_| {
        fail(
            codes::SPAWN_FAILED,
            "The game's working folder cannot be created.",
        )
    })?;
    for config in &d.config {
        let path = confined(server, &config.file)?;
        // A managed key reflects a setting, so an unset one has to leave the
        // file rather than keep the value from the launch before. Skipping it
        // left a cleared seed generating the previous world's map, with
        // nothing on screen naming the number responsible. Same rule core
        // applies to an argument, and the same reason.
        let mut keys = Vec::new();
        let mut cleared = Vec::new();
        for (key, template) in &config.keys {
            match engine::template::fill(template, &bindings)
                .map_err(|e| fail(codes::DESCRIPTOR_INVALID, e.to_string()))?
            {
                Filled::Text(value) => keys.push((key.clone(), value)),
                Filled::Dropped => cleared.push(key.clone()),
            }
        }
        let existing = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => {
                return Err(fail(
                    codes::SPAWN_FAILED,
                    "A game configuration file cannot be read.",
                ))
            }
        };
        let contents = match config.format {
            ConfigFormat::Properties => {
                if keys.iter().any(|(k,v)| k.contains(['\n','\r','=']) || v.contains(['\n','\r'])) {
                    return Err(fail(codes::DESCRIPTOR_INVALID, "A setting contains a newline that this configuration format cannot store."));
                }
                let kept = homerun_core::properties::remove(&existing, &cleared);
                homerun_core::properties::merge(&kept, &keys)
            }
            ConfigFormat::Json => {
                let mut object: serde_json::Map<String, serde_json::Value> = if existing.is_empty() { Default::default() }
                    else { serde_json::from_str(&existing).map_err(|_| fail(codes::SPAWN_FAILED, "The existing game configuration is not a JSON object."))? };
                for key in &cleared { object.remove(key); }
                for (key, value) in keys { object.insert(key, value.into()); }
                serde_json::to_string_pretty(&object).unwrap()
            }
            _ => return Err(fail(codes::DESCRIPTOR_INVALID, "This runner supports JSON and properties configuration files. This game's format needs an engine extension.")),
        };
        write(&path, &contents)?;
    }
    for file in engine::licence::gate(d, true)
        .map_err(|e| fail(codes::LICENCE_NOT_ACCEPTED, e.to_string()))?
    {
        write(&confined(server, &file.path)?, &file.contents)?;
    }
    let console = match engine::control::console(d)
        .map_err(|e| fail(codes::DESCRIPTOR_INVALID, e.to_string()))?
    {
        engine::control::ConsoleKind::Rcon {
            protocol,
            port,
            secret,
        } => Some(rcon::Target {
            protocol,
            address: format!("{LOOPBACK}:{}", ports[&port]),
            password: secrets
                .get(&secret)
                .filter(|s| !s.is_empty())
                .cloned()
                .ok_or_else(|| {
                    fail(
                        codes::DESCRIPTOR_INVALID,
                        "The host has not provided the server's console secret.",
                    )
                })?,
        }),
        _ => None,
    };
    let route = if let Some(target) = &console {
        ConsoleRoute::Rcon(target.clone())
    } else if matches!(d.console.via, engine::descriptor::ConsoleVia::Stdin) {
        ConsoleRoute::Stdin
    } else {
        ConsoleRoute::None
    };
    let executable = platform::executable(runtime, &inv.exe);
    if !executable.is_file() {
        return Err(fail(
            codes::SPAWN_FAILED,
            "The downloaded runtime does not contain this game's server executable.",
        ));
    }
    let supervision = Supervision {
        readiness: Readiness::Marker(d.ready.marker.clone()),
        presence: d
            .observe
            .presence
            .as_ref()
            .map(|p| Presence::Markers {
                join: p.join.clone(),
                leave: p.leave.clone(),
            })
            .unwrap_or(Presence::None),
        console: route,
        ladder: engine::control::stop_ladder(d)
            .iter()
            .map(Rung::from)
            .collect(),
    };
    Ok(Prepared {
        engine: ProcessEngine::supervised(
            Invocation {
                program: executable.to_string_lossy().into(),
                args: inv.args,
                env: inv.env,
            },
            supervision,
        ),
        cwd,
        ports,
        console,
    })
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| {
            fail(
                codes::SPAWN_FAILED,
                "A game configuration folder cannot be created.",
            )
        })?;
    }
    fs::write(path, text).map_err(|_| {
        fail(
            codes::SPAWN_FAILED,
            "A game configuration file cannot be saved.",
        )
    })
}

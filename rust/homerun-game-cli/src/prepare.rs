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

/// The version a vendor runtime is fetched and launched in.
///
/// `None` for every other source, whatever was asked for: a version means
/// nothing to a pinned download or to steamcmd. For a vendor source the
/// host must name one -- it resolves the player's choice, including
/// "latest"; the runner never does -- and it must be dotted digits, because
/// it becomes part of a URL and a directory name.
pub fn runtime_version(d: &GameDescriptor, requested: Option<&str>) -> Result<Option<String>> {
    if !engine::fetch::is_vendor(d, platform::HOST) {
        return Ok(None);
    }
    let Some(version) = requested else {
        return Err(fail(
            codes::DESCRIPTOR_INVALID,
            "This game is downloaded in the version a player chooses, and no version \
             was given. Update Homerun Desktop and try again.",
        ));
    };
    engine::fetch::check_runtime_version(version)
        .map_err(|e| fail(codes::DESCRIPTOR_INVALID, e.to_string()))?;
    Ok(Some(version.to_string()))
}

/// This game's runtime directory: `<root>/<id>`, or `<root>/<id>/<version>`
/// for a vendor runtime. The one place the runner asks, so the fetch, the
/// executable check, a runtime working directory and save mounts agree.
pub fn install_dir(d: &GameDescriptor, root: &Path, version: Option<&str>) -> Result<PathBuf> {
    engine::fetch::install_dir(d, platform::HOST, &root.to_string_lossy(), version)
        .map(PathBuf::from)
        .map_err(|e| fail(codes::DESCRIPTOR_INVALID, e.to_string()))
}

pub fn fetch(
    d: &GameDescriptor,
    root: &Path,
    version: Option<&str>,
    id: &str,
    out: &Output,
    stop: &homerun_supervisor::engine::StopSignal,
) -> Result<PathBuf> {
    let dir = install_dir(d, root, version)?;
    let host = d.platform(platform::HOST).unwrap();
    let exe = &host.launch.exe;
    // A stamp alone does not prove a runtime is whole: the program has to be
    // there too. When `exe` lives inside a component, that component answers
    // for it rather than the main download.
    let exe_part = exe
        .split('/')
        .next()
        .filter(|first| exe.contains('/') && host.components.iter().any(|c| c.name == *first));
    let exe_missing = !platform::executable(&dir, exe).is_file();
    let mut present = fetcher::present(&dir);
    if exe_part.is_none() && exe_missing {
        present.build_id = None;
    }
    let plan = engine::fetch::plan_version(
        d,
        platform::HOST,
        &root.to_string_lossy(),
        &present,
        version,
    )
    .map_err(|e| fail(codes::FETCH_FAILED, e.to_string()))?;
    let mut parts = Vec::new();
    for component in &host.components {
        let mut part_present = fetcher::present(&dir.join(&component.name));
        if exe_part == Some(component.name.as_str()) && exe_missing {
            part_present.build_id = None;
        }
        parts.push(
            engine::fetch::plan_component(
                d,
                platform::HOST,
                &root.to_string_lossy(),
                &component.name,
                &part_present,
                version,
            )
            .map_err(|e| fail(codes::FETCH_FAILED, e.to_string()))?,
        );
    }
    let machine = platform::machine_capacity(root);
    let verdict = engine::doctor::doctor_version(d, &machine, &present, true, version);
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
    // Components first: each lives in its own folder, and the main download
    // only ever adds files around them, so the order cannot clobber either.
    for part in &parts {
        fetcher::fetch(part, &ctx).map_err(|e| fail(codes::FETCH_FAILED, e))?;
        if stop.should_stop() {
            return Err(fail(codes::FETCH_FAILED, "The download was cancelled."));
        }
    }
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
        let mut typed = Vec::new();
        let mut cleared = Vec::new();
        for (key, template) in &config.keys {
            let invalid = |e: homerun_core::Error| fail(codes::DESCRIPTOR_INVALID, e.to_string());
            if matches!(config.format, ConfigFormat::Json) {
                // JSON keeps a setting's type: a number stays a number.
                match engine::template::fill_value(template, &bindings).map_err(invalid)? {
                    Some(value) => typed.push((key.clone(), value)),
                    None => cleared.push(key.clone()),
                }
                continue;
            }
            match engine::template::fill(template, &bindings).map_err(invalid)? {
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
            ConfigFormat::Json => homerun_core::json_config::merge(&existing, &typed, &cleared)
                .map_err(|e| fail(codes::SPAWN_FAILED, format!("This server's configuration cannot be updated: {e}.")))?,
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

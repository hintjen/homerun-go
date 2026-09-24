//! Standalone fronts over the same lifecycle used by Electron.
use crate::{
    prepare,
    protocol::{codes, Command, Event},
    runner::{Output, Runner},
};
use homerun_core::engine;
use homerun_supervisor::{fetcher, platform};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const HELP: &str = "homerun-game supervise
homerun-game doctor|fetch|launch|probe|verify <game.json or slug> [options]
homerun-game stop --server-dir <folder> [--json]

Options:
  --accept-licence          Record your own acceptance of the game's terms
  --runtime-root <folder>   Default: runtime/games
  --runtime-version <v>     The version to fetch and run, for a game downloaded
                            from its vendor's site in a chosen version (e.g. 1.4.5.8)
  --java <path>             The java program to run, for a game that asks its host
                            for Java (requires.java); checked with java -version
  --server-dir <folder>     Default: servers/<game>
  --server-id <id>          Default: descriptor game id
  --server-name <name>      Default: descriptor name
  --settings-file <json>    Object of setting values
  --secrets-file <json>     Object of host-generated secret strings
  --observe-seconds <n>    Probe/verify time after readiness (default 5)
  --evidence <folder>       Default: evidence/<host>
  --json                   Emit the same NDJSON events as supervise
  --features               Print this build's capability names as JSON

A slug resolves to games/<slug>/game.json. launch stays in the foreground.
stop requests shutdown of a standalone launch owning that server directory.
Electron controls supervise exclusively through stdin; it uses no control file.";

fn read_json(path: &str) -> std::result::Result<Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(|_| format!("Cannot read {path}."))?)
        .map_err(|_| format!("{path} must contain valid JSON."))
}

pub fn run() -> std::result::Result<(), String> {
    let mut args = std::env::args().skip(1);
    let Some(verb) = args.next() else {
        println!("{HELP}");
        return Ok(());
    };
    if matches!(verb.as_str(), "--help" | "-h" | "help") {
        println!("{HELP}");
        return Ok(());
    }
    if verb == "--version" {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // Asked of the built binary by the publish script, so a manifest cannot
    // claim a capability the artifact beside it does not have. One JSON array
    // on one line, for the same reason Pumpkin's `--minecraft-version` is
    // parseable: the reader is a script, not a person.
    if verb == "--features" {
        println!("{}", serde_json::to_string(crate::protocol::FEATURES).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if verb == "supervise" {
        if args.next().is_some() {
            return Err("supervise accepts commands on stdin, not command-line options.".into());
        }
        return crate::runner::supervise();
    }
    if !["doctor", "fetch", "launch", "stop", "probe", "verify"].contains(&verb.as_str()) {
        return Err(HELP.into());
    }
    let mut options = BTreeMap::new();
    let (mut accepted, mut as_json, mut descriptor_path) = (false, false, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--accept-licence" => accepted = true,
            "--json" => as_json = true,
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--runtime-root" | "--runtime-version" | "--java" | "--server-dir" | "--server-id"
            | "--server-name" | "--settings-file" | "--secrets-file" | "--observe-seconds"
            | "--evidence" => {
                let value = args.next().ok_or_else(|| format!("{arg} needs a value."))?;
                if options.insert(arg.clone(), value).is_some() {
                    return Err(format!("{arg} was supplied twice."));
                }
            }
            _ if !arg.starts_with('-') && descriptor_path.is_none() => descriptor_path = Some(arg),
            _ => return Err(format!("Unknown argument: {arg}")),
        }
    }
    if verb == "stop" {
        let dir = options
            .get("--server-dir")
            .ok_or("stop needs --server-dir from the original launch.")?;
        let dir = Path::new(dir);
        let token = fs::read_to_string(dir.join(".homerun-runner.lock"))
            .map_err(|_| "No standalone runner owns this server folder.".to_string())?;
        fs::write(dir.join(".homerun-stop"), &token)
            .map_err(|_| "The stop request could not be saved.".to_string())?;
        let deadline = Instant::now() + Duration::from_secs(180);
        while dir.join(".homerun-runner.lock").exists() {
            if Instant::now() >= deadline {
                return Err("The server has not confirmed shutdown. Check the launch console; no unrelated process was killed.".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if as_json {
            print!(
                "{}",
                Event::ServerStopped {
                    server_id: options
                        .get("--server-id")
                        .cloned()
                        .unwrap_or_else(|| "standalone".into()),
                    code: None
                }
                .line()
            );
        } else {
            println!("The standalone runner has stopped.");
        }
        return Ok(());
    }
    let path = descriptor_path.ok_or("Choose a game descriptor or slug.")?;
    let path = if Path::new(&path).is_file() {
        PathBuf::from(path)
    } else {
        Path::new("games").join(path).join("game.json")
    };
    let value = read_json(&path.to_string_lossy())?;
    let d: engine::GameDescriptor = serde_json::from_value(value.clone())
        .map_err(|_| "This game's descriptor cannot be read.".to_string())?;
    let id = options
        .get("--server-id")
        .cloned()
        .unwrap_or_else(|| d.id.clone());
    let root = options
        .get("--runtime-root")
        .cloned()
        .unwrap_or_else(|| "runtime/games".into());
    let dir = options
        .get("--server-dir")
        .cloned()
        .unwrap_or_else(|| format!("servers/{}", d.id));
    let version = options.get("--runtime-version").cloned();
    let java = options.get("--java").cloned();
    if verb == "doctor" {
        let machine = platform::machine_capacity(Path::new(&root));
        // A version that is not one is reported by the verdict itself; the
        // directory it would name is then simply not looked in.
        let present = engine::fetch::install_dir(&d, platform::HOST, &root, version.as_deref())
            .map(|dir| fetcher::present(Path::new(&dir)))
            .unwrap_or_default();
        let mut verdict =
            engine::doctor::doctor_version(&d, &machine, &present, accepted, version.as_deref());
        // A game that runs the host's Java is not ready until a Java is
        // named and answers with the right major -- the same check a launch
        // makes, so doctor cannot pass a machine that start would refuse.
        if let Err((_, message)) = prepare::host_java(&d, java.as_deref()) {
            verdict.problems.push(message);
            verdict.ok = false;
        }
        // No success event exists for doctor in v1: report via stderr in JSON
        // mode and use protocol errors for refusals, rather than inventing events.
        if as_json {
            // Every doctor problem used to go out as `requires_unmet`,
            // including "nobody has accepted the terms" -- which is not a
            // fact about this computer and has a code of its own in the
            // contract. A caller cannot offer the licence to a person when
            // the only thing it was told is that the machine is not ready.
            for message in &verdict.problems {
                let code = if verdict.licence.as_deref() == Some(message.as_str()) {
                    codes::LICENCE_NOT_ACCEPTED
                } else {
                    codes::REQUIRES_UNMET
                };
                print!(
                    "{}",
                    Event::Error {
                        server_id: Some(id.clone()),
                        code: code.into(),
                        message: message.clone()
                    }
                    .line()
                );
            }
            for warning in &verdict.warnings {
                eprintln!("{warning}");
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&verdict).unwrap());
        }
        return if verdict.ok {
            Ok(())
        } else {
            Err("This computer is not ready to host this game.".into())
        };
    }
    if let Err((code, message)) = prepare::descriptor(value.clone(), accepted) {
        if as_json {
            print!(
                "{}",
                Event::Error {
                    server_id: Some(id.clone()),
                    code: code.into(),
                    message: message.clone()
                }
                .line()
            );
        }
        return Err(message);
    }
    let probing = verb == "probe" || verb == "verify";
    let observe: u64 = options
        .get("--observe-seconds")
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| "Observation time must be a whole number of seconds.")?
        .unwrap_or(5);
    if observe > 86400 {
        return Err("Observation time must be at most one day.".into());
    }
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = events.clone();
    let output = Output::new(move |event| {
        if probing {
            let mut captured = seen.lock().unwrap();
            if captured.len() == 10000 {
                // Preserve lifecycle facts; discard the oldest sampled log.
                if let Some(i) = captured
                    .iter()
                    .position(|e| e["event"] == "server-log" || e["event"] == "stats")
                {
                    captured.remove(i);
                } else {
                    captured.remove(0);
                }
            }
            captured.push(serde_json::from_str(&event.line()).unwrap());
        }
        if as_json {
            print!("{}", event.line());
        } else {
            match &event {
                Event::ServerLog { line, .. } => println!("{line}"),
                Event::Error { message, .. } => eprintln!("{message}"),
                _ => print!("{}", event.line()),
            }
        }
        let _ = std::io::stdout().flush();
    });
    let error_seen = Arc::new(AtomicBool::new(false));
    let error_flag = error_seen.clone();
    let ready_time = Arc::new(Mutex::new(None::<Instant>));
    let ready_flag = ready_time.clone();
    let output = Output::new(move |event| {
        if matches!(event, Event::Error { .. } | Event::ServerCrashed { .. }) {
            error_flag.store(true, Ordering::SeqCst);
        }
        if matches!(event, Event::ServerStarted { .. }) {
            *ready_flag.lock().unwrap() = Some(Instant::now());
        }
        output.send(event);
    });
    let stop = Arc::new(AtomicBool::new(false));
    let interrupt = stop.clone();
    ctrlc::set_handler(move || interrupt.store(true, Ordering::SeqCst))
        .map_err(|_| "The runner could not install its shutdown handler.".to_string())?;
    let mut runner = Runner::new(output)?;
    runner.network.strict = probing;
    let _owner = if verb != "fetch" {
        Some(Owner::claim(Path::new(&dir))?)
    } else {
        None
    };
    let command = if verb == "fetch" {
        Command::Fetch {
            server_id: id.clone(),
            descriptor: value,
            runtime_root: root,
            licence_accepted: accepted,
            runtime_version: version,
            java_path: java.clone(),
        }
    } else {
        Command::Start {
            server_id: id.clone(),
            descriptor: value,
            runtime_root: root,
            licence_accepted: accepted,
            server_dir: dir.clone(),
            server_name: options
                .get("--server-name")
                .cloned()
                .unwrap_or_else(|| d.name.clone()),
            settings: serde_json::from_value(
                options
                    .get("--settings-file")
                    .map(|p| read_json(p))
                    .transpose()?
                    .unwrap_or(json!({})),
            )
            .map_err(|_| "Settings must be an object.".to_string())?,
            secrets: serde_json::from_value(
                options
                    .get("--secrets-file")
                    .map(|p| read_json(p))
                    .transpose()?
                    .unwrap_or(json!({})),
            )
            .map_err(|_| "Secrets must be an object of strings.".to_string())?,
            bind_address: Some("127.0.0.1".into()),
            runtime_version: version,
            java_path: java,
        }
    };
    runner.command(command);
    let mut stopping = false;
    while !runner.idle() {
        runner.tick();
        if !stopping
            && (stop.load(Ordering::SeqCst)
                || _owner.as_ref().is_some_and(Owner::stop_requested)
                || (probing
                    && ready_time
                        .lock()
                        .unwrap()
                        .is_some_and(|t| t.elapsed() >= Duration::from_secs(observe))))
        {
            stopping = true;
            runner.command(Command::Stop {
                server_id: id.clone(),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    runner.shutdown();
    if probing {
        let events = events.lock().unwrap();
        let ready = events.iter().any(|e| e["event"] == "server-started");
        let stopped = events.iter().any(|e| e["event"] == "server-stopped");
        let evidence = options
            .get("--evidence")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new("evidence").join(platform::HOST));
        fs::create_dir_all(&evidence)
            .map_err(|_| "The evidence folder could not be created.".to_string())?;
        let report = json!({ "host": platform::HOST, "game": d.id, "ready": ready, "stopped": stopped,
            "ok": ready && stopped && !error_seen.load(Ordering::SeqCst), "events": *events,
            "network": runner.network.report(),
            "unverified": ["world persistence after restart", "writes outside the server directory", "gateway reachability", "unsupported player-query formats"] });
        fs::write(
            evidence.join("probe.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .map_err(|_| "Probe evidence could not be saved.".to_string())?;
        if !ready || !stopped {
            return Err("The game did not complete the readiness and stop checks.".into());
        }
    }
    if error_seen.load(Ordering::SeqCst) {
        Err("The game operation failed; see the reported error.".into())
    } else {
        Ok(())
    }
}

/// Local standalone control only. Exclusive creation prevents two CLI owners
/// writing the same world; stale locks are never interpreted as permission to
/// kill a PID, which could now belong to an unrelated process.
struct Owner {
    dir: PathBuf,
    token: String,
}
impl Owner {
    fn claim(dir: &Path) -> std::result::Result<Self, String> {
        fs::create_dir_all(dir).map_err(|_| "The server folder cannot be created.".to_string())?;
        let mut lock = fs::OpenOptions::new().write(true).create_new(true).open(dir.join(".homerun-runner.lock"))
            .map_err(|_| "A standalone runner already owns this folder. If it crashed, confirm it is stopped before removing .homerun-runner.lock.".to_string())?;
        let token = format!("{}:{:?}", std::process::id(), std::time::SystemTime::now());
        lock.write_all(token.as_bytes())
            .map_err(|_| "The ownership record cannot be written.".to_string())?;
        Ok(Self {
            dir: dir.to_path_buf(),
            token,
        })
    }
    fn stop_requested(&self) -> bool {
        fs::read_to_string(self.dir.join(".homerun-stop"))
            .ok()
            .as_deref()
            == Some(&self.token)
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join(".homerun-stop"));
        let _ = fs::remove_file(self.dir.join(".homerun-runner.lock"));
    }
}

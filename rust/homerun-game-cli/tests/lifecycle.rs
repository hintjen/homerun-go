use homerun_supervisor::{fetcher, platform};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

// Reinvoke this test executable as a real game. No vendor software or licence
// acceptance is involved. Its save proves the runner sent quit, not just kill.
#[test]
fn fake_game() {
    if std::env::var("HOMERUN_TEST_GAME").as_deref() != Ok("1") {
        return;
    }
    let port: u16 = std::env::var("HOMERUN_TEST_PORT").unwrap().parse().unwrap();

    /// Re-invoke this executable as one more process in the chain.
    fn spawn_self(port: &str, role: &str) {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fake_game", "--nocapture", "--test-threads=1"])
            .env("HOMERUN_TEST_GAME", "1")
            .env("HOMERUN_TEST_PORT", port)
            .env("HOMERUN_TEST_MODE", "silent")
            // Every role is cleared before one is set. Inheriting the
            // caller's role makes each process start the next, which is an
            // unbounded chain rather than the three links intended.
            .env_remove("HOMERUN_TEST_GRANDCHILD")
            .env_remove("HOMERUN_TEST_RELAY")
            .env_remove("HOMERUN_TEST_LINGER")
            .env_remove("HOMERUN_TEST_BIND")
            .env(role, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
    }

    // A launcher: start the real thing and leave. It binds nothing, so by the
    // time a stop arrives the real thing's recorded parent is a process that
    // no longer exists. `taskkill /T` walks parent links and cannot see past
    // that; a job object holds what its members started regardless.
    if std::env::var("HOMERUN_TEST_RELAY").is_ok() {
        spawn_self(&port.to_string(), "HOMERUN_TEST_LINGER");
        return;
    }

    // A game that ignores the address it was told to bind. Default loopback,
    // because that is what a well-behaved one does with {bindAddress}.
    let bind = std::env::var("HOMERUN_TEST_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let _socket = TcpListener::bind((bind.as_str(), port)).unwrap();

    // A program that holds a port and outlives whoever started it: nothing
    // asks it to stop, and it keeps the port either way.
    if std::env::var("HOMERUN_TEST_LINGER").is_ok() {
        thread::sleep(Duration::from_secs(30));
        return;
    }

    if let Ok(grandchild) = std::env::var("HOMERUN_TEST_GRANDCHILD") {
        spawn_self(&grandchild, "HOMERUN_TEST_RELAY");
    }

    fs::write("pid", std::process::id().to_string()).unwrap();
    if std::env::var("HOMERUN_TEST_MODE").as_deref() != Ok("silent") {
        eprintln!("FAKE READY"); // Deliberately stderr, with stdout otherwise quiet.
    }
    // A server that does not take end-of-stdin as a reason to stop -- which
    // is most of them, since a dedicated server's stdin being closed is
    // ordinary. Without this the fake game exits the moment the runner dies,
    // which would make the orphan test pass for a reason that is nothing to
    // do with owning anything.
    if std::env::var("HOMERUN_TEST_MODE").as_deref() == Ok("orphan") {
        thread::sleep(Duration::from_secs(30));
        return;
    }
    let deaf = std::env::var("HOMERUN_TEST_MODE").as_deref() == Ok("deaf");
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        if line == "quit" && !deaf {
            fs::write("saved", "world flushed").unwrap();
            return;
        }
        if line == "crash" {
            std::process::exit(23);
        }
        println!("reply: {line}");
        std::io::stdout().flush().unwrap();
    }
}

struct Fixture {
    root: PathBuf,
    d: Value,
    port: u16,
}
impl Fixture {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "homerun-runner-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(root.join("runtime/fake")).unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let name = if cfg!(windows) { "fake.exe" } else { "fake" };
        fs::copy(
            std::env::current_exe().unwrap(),
            root.join("runtime/fake").join(name),
        )
        .unwrap();
        let digest = fetcher::digest_of(&root.join("runtime/fake").join(name)).unwrap();
        fs::write(root.join("runtime/fake/.homerun-build"), &digest[..12]).unwrap();
        let d = json!({
            "id":"fake", "name":"Test server", "hosts":[platform::HOST],
            "platforms":{(platform::HOST):{
                "runtime":{"source":"direct","url":format!("http://127.0.0.1:1/{name}"),"sha256":digest,"extract":"none"},
                "launch":{"exe":name,"args":["--exact","fake_game","--nocapture","--test-threads=1"],
                    "env":{"HOMERUN_TEST_GAME":"1","HOMERUN_TEST_PORT":"{port:game}"},"cwd":"."}
            }},
            "ready":{"marker":"FAKE READY","timeoutMs":10000},
            "console":{"via":"stdin"}, "stop":{"via":"console","command":"quit","graceMs":1000},
            "ports":[{"name":"game","proto":"tcp","port":port,"expose":true}],
            "config":[{"file":"settings.json","format":"json","keys":{"hostname":"{setting:hostname}"}}],
            "settings":[{"key":"hostname","type":"string","default":"{serverName}"}]
        });
        Self { root, d, port }
    }
    fn start(&self) -> Value {
        json!({"cmd":"start","serverId":"s1","descriptor":self.d,
        "runtimeRoot":self.root.join("runtime"), "serverDir":self.root.join("server"),
        "serverName":"{secret:rcon}","settings":{},"secrets":{"rcon":"do-not-print-this"},"licenceAccepted":true})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Host {
    child: Child,
    events: mpsc::Receiver<Value>,
    seen: Vec<Value>,
}
impl Host {
    fn new() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_homerun-game"))
            .arg("supervise")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let (tx, events) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let value = serde_json::from_str(&line).expect("stdout must contain NDJSON only");
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut h = Self {
            child,
            events,
            seen: vec![],
        };
        let ready = h.until("ready");
        assert_eq!(ready["protocol"], 1);
        assert_eq!(ready["build"].as_str().unwrap().len(), 12);
        h
    }
    fn send(&mut self, value: Value) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{value}").unwrap();
    }
    fn until(&mut self, event: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let value = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("waiting for {event}; observed {:?}", self.seen));
            self.seen.push(value.clone());
            if value["event"] == event {
                return value;
            }
        }
    }
    fn eof(&mut self) {
        self.child.stdin.take();
        self.until("shutdown-complete");
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.child.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(12);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port nothing is using, learned the only way there is.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Whether the port has actually been let go, which is the thing a player
/// meets: an orphan holding one makes the next start fail `port_unavailable`.
fn port_freed(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Windows only, and not a gap elsewhere. Unix has process groups and
/// signals; Android's ladder ends in `SIGKILL` to a pid and its servers are
/// not launcher-style. The Job Object exists because Windows has no
/// equivalent, so these are the tests for the thing that only Windows needed.
///
/// The runner is ended the way Task Manager, an Electron crash and a
/// force-quit all end it: `TerminateProcess`, with no chance to run any
/// cleanup at all. Everything it was supervising has to go with it, or the
/// game is left holding its ports and writing to its save directory -- and
/// the next start either fails `port_unavailable` or, past the preflight,
/// becomes a second server on the same world.
#[test]
#[cfg(windows)]
fn an_abrupt_runner_death_takes_the_game_with_it() {
    let f = Fixture::new();
    let mut start = f.start();
    start["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_MODE"] =
        json!("orphan");

    let mut h = Host::new();
    h.send(start);
    h.until("server-started");

    assert!(
        TcpListener::bind(("127.0.0.1", f.port)).is_err(),
        "the game is not actually holding its port, so this proves nothing"
    );

    h.child.kill().unwrap();
    h.child.wait().unwrap();

    assert!(
        port_freed(f.port),
        "the runner was killed and the game kept running on port {}",
        f.port
    );
}

/// A launcher-style server -- which is what a great many vendors ship --
/// starts the real server and exits, so the pid the runner holds is not the
/// pid doing the work. `taskkill /PID n /F` ended a process that had already
/// gone; even `/T` walks parent links the launcher broke on its way out.
#[test]
#[cfg(windows)]
fn a_game_that_starts_another_program_has_all_of_it_stopped() {
    let f = Fixture::new();
    let grandchild = free_port();
    let mut start = f.start();
    let env = &mut start["descriptor"]["platforms"][platform::HOST]["launch"]["env"];
    env["HOMERUN_TEST_GRANDCHILD"] = json!(grandchild.to_string());
    // Deaf, so the console rung is ignored and the ladder climbs to the rung
    // that cannot be refused -- which is the one under test.
    env["HOMERUN_TEST_MODE"] = json!("deaf");

    let mut h = Host::new();
    h.send(start);
    h.until("server-started");
    assert!(
        TcpListener::bind(("127.0.0.1", grandchild)).is_err(),
        "the grandchild never started, so this proves nothing"
    );

    h.send(json!({"cmd":"stop","serverId":"s1"}));
    h.until("server-stopped");

    assert!(
        port_freed(grandchild),
        "the server was stopped and the program it started kept running on port {grandchild}"
    );
    h.eof();
}

#[test]
fn stderr_readiness_console_and_eof_save_the_world() {
    let f = Fixture::new();
    let mut h = Host::new();
    h.send(f.start());
    h.until("server-started");
    let ports = h
        .seen
        .iter()
        .position(|v| v["event"] == "server-ports")
        .unwrap();
    let started = h
        .seen
        .iter()
        .position(|v| v["event"] == "server-started")
        .unwrap();
    assert!(
        ports < started,
        "a host must know ports before it opens tunnels"
    );
    assert_eq!(h.seen[ports]["ports"]["game"], f.port);
    assert!(h.seen.iter().any(|v| v["event"] == "server-log"
        && v["stream"] == "stderr"
        && v["line"] == "FAKE READY"));
    h.send(json!({"cmd":"console","serverId":"s1","command":"hello","reqId":"c1"}));
    assert_eq!(h.until("console-response")["reqId"], "c1");
    let config: Value =
        serde_json::from_slice(&fs::read(f.root.join("server/settings.json")).unwrap()).unwrap();
    assert_eq!(
        config["hostname"], "{secret:rcon}",
        "player text must not be re-templated"
    );
    h.eof();
    assert_eq!(
        fs::read_to_string(f.root.join("server/saved")).expect(
            "EOF must let the game save: the save file is absent, suggesting a forced kill"
        ),
        "world flushed",
        "EOF must send the stop verb before exiting"
    );
}

/// `expose: false` says a port stays on this computer. Nothing checked it:
/// the bind address was validated and then dropped, and the observation
/// carried no address to check against, so `127.0.0.1:28016` and
/// `0.0.0.0:28016` were the same answer. This game ignores the address it was
/// given and opens a private port to the network; the runner has to stop it
/// rather than leave an administrative console reachable from outside.
#[test]
fn a_private_port_bound_to_every_interface_stops_the_server() {
    let f = Fixture::new();
    let mut start = f.start();
    start["descriptor"]["ports"][0]["expose"] = json!(false);
    start["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_BIND"] =
        json!("0.0.0.0");

    let mut h = Host::new();
    h.send(start);

    let error = h.until("error");
    assert_eq!(error["code"], "port_exposed", "{error}");
    assert_eq!(error["serverId"], "s1");
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("game"), "written for a player: {message}");
    assert!(
        !message.contains("0.0.0.0:") && !message.contains("expose"),
        "reads as a verdict rather than a diagnostic: {message}"
    );
    assert!(
        !h.seen.iter().any(|v| v["event"] == "server-started"),
        "a server that was refused must never be reported running: {:?}",
        h.seen
    );
    h.eof();
}

/// The same game binding the same private port on loopback is exactly what
/// the check is meant to allow through, so it must still reach `running`.
#[test]
fn a_private_port_on_loopback_is_not_refused() {
    let f = Fixture::new();
    let mut start = f.start();
    start["descriptor"]["ports"][0]["expose"] = json!(false);

    let mut h = Host::new();
    h.send(start);
    h.until("server-started");
    assert!(
        !h.seen.iter().any(|v| v["code"] == "port_exposed"),
        "{:?}",
        h.seen
    );
    h.eof();
}

/// The runner has no API in front of it, so core's backstop is the only
/// thing between a server name and a game's own argument parser. A name that
/// *is* a switch must be refused before anything is spawned, not passed along
/// as one argv element for a `+key value` parser to read as a new switch.
#[test]
fn a_server_name_that_is_a_switch_is_refused_before_anything_is_spawned() {
    let f = Fixture::new();
    let mut h = Host::new();
    let mut start = f.start();
    start["serverName"] = json!("+rcon.web");
    h.send(start);

    let error = h.until("error");
    assert_eq!(error["serverId"], "s1");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("cannot start with"),
        "{error}"
    );
    assert!(
        !f.root.join("server").exists() || !f.root.join("server/settings.json").exists(),
        "nothing should have been written for a launch that was refused"
    );
    h.eof();
}

/// A setting a player cleared has to leave the managed file. Skipping the key
/// kept the value from the launch before, so a cleared seed went on
/// generating the old world with nothing on screen naming the number doing
/// it. Everything the file holds that Homerun does not manage stays put.
#[test]
fn a_cleared_setting_leaves_its_managed_key_out_of_the_config_file() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("server")).unwrap();
    fs::write(
        f.root.join("server/settings.json"),
        r#"{"hostname":"from the launch before","keep":"mine"}"#,
    )
    .unwrap();

    let mut h = Host::new();
    let mut start = f.start();
    start["settings"] = json!({ "hostname": "" });
    h.send(start);
    h.until("server-started");

    let config: Value =
        serde_json::from_slice(&fs::read(f.root.join("server/settings.json")).unwrap()).unwrap();
    assert!(
        config.get("hostname").is_none(),
        "a cleared setting must not keep the previous launch's value: {config}"
    );
    assert_eq!(
        config["keep"], "mine",
        "a key Homerun does not manage must survive: {config}"
    );
    h.eof();
}

#[test]
fn malformed_known_commands_reply_without_starting_and_keep_stdin_usable() {
    let f = Fixture::new();
    let mut h = Host::new();
    let mut missing_dir = f.start();
    missing_dir.as_object_mut().unwrap().remove("serverDir");
    let mut null_settings = f.start();
    null_settings["settings"] = Value::Null;
    let mut wrong_acceptance = f.start();
    wrong_acceptance["licenceAccepted"] = json!("true");
    for command in [
        missing_dir,
        null_settings,
        wrong_acceptance,
        json!({"cmd":"fetch", "serverId":"s1"}),
    ] {
        h.send(command);
        let error = h.until("error");
        assert_eq!(error["serverId"], "s1");
        assert_eq!(error["code"], "descriptor_invalid");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("missing or invalid fields"));
        assert!(!error.to_string().contains("do-not-print-this"));
    }
    h.send(json!({"cmd":"console", "serverId":"s1", "reqId":"bad-console", "command":null}));
    h.until("error");
    assert_eq!(h.until("console-response")["reqId"], "bad-console");
    h.send(json!({"cmd":"status"}));
    assert_eq!(h.until("status")["servers"], json!([]));
    assert!(!f.root.join("server/pid").exists());
    h.eof();
}

#[test]
fn missing_acceptance_wins_over_bad_descriptor_and_never_fetches() {
    let f = Fixture::new();
    let mut h = Host::new();
    h.send(json!({"cmd":"fetch","serverId":"s1","descriptor":{},"runtimeRoot":f.root.join("untouched")}));
    assert_eq!(h.until("error")["code"], "licence_not_accepted");
    assert!(!f.root.join("untouched").exists());
    h.send(json!({"cmd":"fetch","serverId":"s1","descriptor":{},"runtimeRoot":f.root.join("untouched"),"licenceAccepted":true}));
    assert_eq!(h.until("error")["code"], "descriptor_invalid");
    h.eof();
}

#[test]
fn occupied_port_is_refused_before_spawn() {
    let f = Fixture::new();
    let _occupied = TcpListener::bind(("127.0.0.1", f.port)).unwrap();
    let mut h = Host::new();
    h.send(f.start());
    assert_eq!(h.until("error")["code"], "port_unavailable");
    assert!(!f.root.join("server/pid").exists());
    h.eof();
}

#[test]
fn busy_status_and_restart_use_one_lifecycle() {
    let f = Fixture::new();
    let mut h = Host::new();
    h.send(f.start());
    h.until("server-started");
    let mut second = f.start();
    second["serverId"] = "s2".into();
    h.send(second);
    assert_eq!(h.until("error")["code"], "busy");
    h.send(json!({"cmd":"status"}));
    assert_eq!(h.until("status")["servers"][0]["state"], "running");
    h.send(json!({"cmd":"stop","serverId":"s1"}));
    h.until("server-stopped");
    thread::sleep(Duration::from_millis(150));
    h.send(f.start());
    h.until("server-started");
    h.eof();
}

#[test]
fn ready_timeout_stops_the_process_and_does_not_claim_running() {
    let mut f = Fixture::new();
    f.d["ready"]["timeoutMs"] = 1000.into();
    f.d["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_MODE"] = "silent".into();
    let mut h = Host::new();
    h.send(f.start());
    assert_eq!(h.until("error")["code"], "ready_timeout");
    h.until("server-crashed");
    assert!(!h.seen.iter().any(|v| v["event"] == "server-started"));
    assert!(
        f.root.join("server/saved").exists(),
        "a timeout must still try to save"
    );
    h.eof();
}

#[test]
fn start_fetches_a_missing_runtime_from_a_local_fixture_server() {
    let mut f = Fixture::new();
    let source = std::env::current_exe().unwrap();
    let bytes = fs::read(source).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let name = f.d["platforms"][platform::HOST]["launch"]["exe"]
        .as_str()
        .unwrap()
        .to_owned();
    f.d["platforms"][platform::HOST]["runtime"]["url"] =
        format!("http://{}/{name}", listener.local_addr().unwrap()).into();
    fs::remove_dir_all(f.root.join("runtime/fake")).unwrap();
    let http = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok((mut client, _)) = listener.accept() {
                client
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0; 4096];
                let _ = client.read(&mut request);
                write!(
                    client,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                )
                .unwrap();
                client.write_all(&bytes).unwrap();
                break;
            }
            assert!(
                Instant::now() < deadline,
                "runner never fetched its missing runtime"
            );
            thread::sleep(Duration::from_millis(20));
        }
    });
    let mut h = Host::new();
    h.send(f.start());
    h.until("server-started");
    assert!(h.seen.iter().any(|v| v["event"] == "fetch-progress"));
    assert!(h.seen.iter().any(|v| v["event"] == "fetch-complete"));
    h.eof();
    http.join().unwrap();
}

/// `doctor` reported every refusal as `requires_unmet`, including "nobody has
/// accepted the terms". A caller cannot put the licence in front of a person
/// when all it has been told is that the computer is not ready, and the
/// contract has a code for exactly this. The other refusals must keep theirs.
#[test]
fn doctor_reports_an_unaccepted_licence_under_its_own_code() {
    let f = Fixture::new();
    let mut d = f.d.clone();
    d["licence"] = json!({
        "name": "the Test Server Terms",
        "url": "https://example.invalid/terms"
    });
    // Out of reach on any machine, so the verdict carries a second refusal
    // that must NOT be relabelled as a licence problem.
    d["requires"] = json!({ "ramMb": 1024 * 1024 });
    let descriptor = f.root.join("game.json");
    fs::write(&descriptor, d.to_string()).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_homerun-game"))
        .args(["doctor", &descriptor.to_string_lossy(), "--json"])
        .arg("--runtime-root")
        .arg(f.root.join("runtime"))
        .output()
        .unwrap();

    let events: Vec<Value> = String::from_utf8_lossy(&result.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout must contain NDJSON only"))
        .collect();

    let licence = events
        .iter()
        .find(|e| e["code"] == "licence_not_accepted")
        .unwrap_or_else(|| panic!("no licence_not_accepted event: {events:?}"));
    assert!(
        licence["message"]
            .as_str()
            .unwrap()
            .contains("the Test Server Terms"),
        "{licence}"
    );
    assert!(
        events.iter().any(|e| e["code"] == "requires_unmet"),
        "the memory refusal must keep its own code: {events:?}"
    );
}

#[test]
fn probe_writes_evidence_and_verify_checks_the_same_real_process() {
    let f = Fixture::new();
    let descriptor = f.root.join("game.json");
    fs::write(&descriptor, f.d.to_string()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_homerun-game"))
        .args([
            "verify",
            &descriptor.to_string_lossy(),
            "--accept-licence",
            "--observe-seconds",
            "0",
            "--json",
        ])
        .arg("--runtime-root")
        .arg(f.root.join("runtime"))
        .arg("--server-dir")
        .arg(f.root.join("server"))
        .arg("--evidence")
        .arg(f.root.join("evidence"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(f.root.join("evidence/probe.json")).unwrap()).unwrap();
    assert_eq!(report["ok"], true);
    assert!(f.root.join("server/saved").exists());
}

#[test]
fn eof_cancels_a_download_even_when_the_http_peer_never_sends_headers() {
    let mut f = Fixture::new();
    fs::remove_file(f.root.join("runtime/fake/.homerun-build")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    f.d["platforms"][platform::HOST]["runtime"]["url"] =
        format!("http://{}/fake.exe", listener.local_addr().unwrap()).into();
    let (tx, rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok((client, _)) = listener.accept() {
                tx.send(client).unwrap();
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
    });
    let mut h = Host::new();
    h.send(f.start());
    let _silent_peer = rx.recv_timeout(Duration::from_secs(20)).unwrap();
    // A status reply while the fetch is blocked proves stdin is responsive.
    h.send(json!({"cmd":"status"}));
    assert_eq!(h.until("status")["servers"][0]["state"], "starting");
    let begin = Instant::now();
    h.eof();
    assert!(
        begin.elapsed() < Duration::from_secs(4),
        "EOF waited for an unresponsive download peer"
    );
    assert!(!f.root.join("server/pid").exists());
    peer.join().unwrap();
}

#[test]
fn crashes_have_a_tail_and_never_claim_a_requested_stop() {
    let f = Fixture::new();
    let mut h = Host::new();
    h.send(f.start());
    h.until("server-started");
    h.send(json!({"cmd":"console","serverId":"s1","command":"crash","reqId":"crash"}));
    let crash = h.until("server-crashed");
    assert!(crash["tail"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "FAKE READY"));
    h.send(json!({"cmd":"status"}));
    assert_eq!(h.until("status")["servers"][0]["state"], "crashed");
    h.eof();
}

#[test]
fn incompatible_handshake_shuts_down_and_unknown_commands_do_not() {
    let mut h = Host::new();
    h.send(json!({"cmd":"newer-command","extra":true}));
    h.send(json!({"cmd":"status","extra":true}));
    assert!(h.until("status")["servers"].as_array().unwrap().is_empty());
    h.send(json!({"cmd":"hello","protocol":999}));
    h.until("shutdown-complete");
    assert!(h.child.wait().unwrap().success());
}

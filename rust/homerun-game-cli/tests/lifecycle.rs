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
    let save = std::env::var("HOMERUN_TEST_SAVE").unwrap_or_else(|_| "saved".into());
    if let Ok(runtime) = std::env::var("HOMERUN_TEST_RUNTIME") {
        assert_eq!(
            fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
            fs::canonicalize(runtime).unwrap()
        );
        assert_eq!(
            fs::read_to_string("Bundles/asset").unwrap(),
            "runtime asset"
        );
        // The game writes to its cwd-relative save path; it knows no junctions.
        let previous = fs::read_to_string(&save).unwrap_or_default();
        fs::write(&save, format!("{previous}boot\n")).unwrap();
    }

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
            .env(
                "HOMERUN_TEST_BIND",
                std::env::var("HOMERUN_TEST_DESCENDANT_BIND").unwrap_or("127.0.0.1".into()),
            )
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
    let mut socket = Some(TcpListener::bind((bind.as_str(), port)).unwrap());

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
    // A vendor that prints its own launch line. Rust does exactly this: its
    // RCON password is a `+rcon.password` argument and the line is echoed at
    // startup, so the host would publish it to the player's console.
    if let Ok(echo) = std::env::var("HOMERUN_TEST_ECHO") {
        println!("Command line: -batchmode +rcon.password {echo}");
        std::io::stdout().flush().unwrap();
    }
    if std::env::var("HOMERUN_TEST_MODE").as_deref() != Ok("silent") {
        eprintln!("FAKE READY"); // Deliberately stderr, with stdout otherwise quiet.
    }
    if std::env::var("HOMERUN_TEST_MODE").as_deref() == Ok("flood-then-widen") {
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            drop(socket.take());
            let _wide = TcpListener::bind(("0.0.0.0", port)).unwrap();
            fs::write("wide-seen", "bound").unwrap();
            thread::sleep(Duration::from_secs(30));
        });
        loop {
            println!("{}", "flood".repeat(1000));
        }
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
    if let Ok(extra_port) = std::env::var("HOMERUN_TEST_EXTRA_PORT") {
        let extra_bind = std::env::var("HOMERUN_TEST_EXTRA_BIND").unwrap_or("0.0.0.0".into());
        thread::spawn(move || {
            // It must appear after readiness so verify cannot pass with a one-shot audit.
            thread::sleep(Duration::from_millis(750));
            let _extra =
                TcpListener::bind((extra_bind.as_str(), extra_port.parse::<u16>().unwrap()))
                    .unwrap();
            thread::sleep(Duration::from_secs(30));
        });
    }
    let deaf = std::env::var("HOMERUN_TEST_MODE").as_deref() == Ok("deaf");
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        if line == "widen" {
            drop(socket.take());
            socket = Some(TcpListener::bind(("0.0.0.0", port)).unwrap());
            println!("WIDE NOW");
        }

        if line == "quit" && std::env::var("HOMERUN_TEST_MODE").as_deref() == Ok("wide-on-stop") {
            drop(socket.take());
            socket = Some(TcpListener::bind(("0.0.0.0", port)).unwrap());
            continue;
        }
        if line == "quit" && !deaf {
            fs::write(&save, "world flushed").unwrap();
            return;
        }
        if line == "crash" {
            std::process::exit(23);
        }
        println!("reply: {line}");
        std::io::stdout().flush().unwrap();
    }
}

/// One runtime, at one path per machine, for fixtures whose game listens on
/// every interface on purpose.
///
/// Windows Firewall asks about a program the first time it listens beyond
/// loopback, and it remembers the answer by the program's *path*. A fresh
/// per-test copy is a fresh path, so every wide-binding test raised a prompt
/// on every run. The tests only need the listener to exist -- they pass
/// whether the prompt is allowed or cancelled -- so running those games from a
/// single fixed path leaves at most one prompt, ever.
///
/// Why a whole shared runtime root rather than only a shared exe: the engine
/// resolves `launch.exe` inside `<runtimeRoot>/<id>` and rightly refuses
/// absolute or escaping paths, and a directory junction does not help because
/// Windows reports (and the firewall keys on) the path the process was
/// launched through, not the junction's target. The runner holds an exclusive
/// lease on a runtime root, so the fixtures sharing this one are serialised by
/// `.fixture.lock` -- across threads and across concurrent `cargo test` runs.
/// Only launch-only fixtures use it: anything that stamps, breaks, fetches
/// into or mounts through its runtime keeps its own per-test copy.
///
/// The copy is refreshed (copy to a temp name, then rename) only when its
/// digest differs from this test binary. If it cannot be replaced -- another
/// build's game is still running from it -- or the lock is not had in time,
/// the fixture falls back to a per-test copy for that test rather than fail.
struct FixedGame {
    runtime: PathBuf,
    _lock: fs::File,
}
impl FixedGame {
    #[cfg(windows)]
    fn acquire() -> Option<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        let runtime = std::env::temp_dir().join("homerun-runner-fake-game");
        let dir = runtime.join("fake");
        fs::create_dir_all(&dir).ok()?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let lock = loop {
            let opened = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(runtime.join(".fixture.lock"));
            match opened {
                Ok(file) => break file,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        };
        let source = std::env::current_exe().ok()?;
        let digest = fetcher::digest_of(&source).ok()?;
        let exe = dir.join("fake.exe");
        if fetcher::digest_of(&exe).ok().as_deref() != Some(digest.as_str()) {
            let staging = dir.join(format!("fake.exe.{}.tmp", std::process::id()));
            if fs::copy(&source, &staging)
                .and_then(|_| fs::rename(&staging, &exe))
                .is_err()
            {
                let _ = fs::remove_file(&staging);
                return None;
            }
        }
        fs::write(dir.join(".homerun-build"), &digest[..12]).ok()?;
        Some(Self {
            runtime,
            _lock: lock,
        })
    }
    /// The prompt is a Windows Firewall behaviour; elsewhere nothing changes.
    #[cfg(not(windows))]
    fn acquire() -> Option<Self> {
        None
    }
}

struct Fixture {
    root: PathBuf,
    /// `<root>/runtime`, or the fixed runtime for `binding_wide` fixtures.
    runtime: PathBuf,
    d: Value,
    port: u16,
    // Dropped after `Drop` below, i.e. once the test's runner has exited.
    _fixed: Option<FixedGame>,
}
impl Fixture {
    /// A fixture whose game binds beyond loopback. It launches from the one
    /// fixed path (see `FixedGame`) and must not change its runtime.
    fn binding_wide() -> Self {
        Self::create(FixedGame::acquire())
    }
    fn with_runtime_saves() -> Self {
        let mut f = Self::new();
        let launch = &mut f.d["platforms"][platform::HOST]["launch"];
        launch["cwdBase"] = json!("runtime");
        launch["env"]["HOMERUN_TEST_RUNTIME"] = json!("{runtimeDir}");
        launch["env"]["HOMERUN_TEST_SAVE"] = json!("server/world.save");
        f.d["saves"] = json!({"paths":["world"],"mounts":[{"runtime":"server","server":"world"}]});
        fs::create_dir_all(f.root.join("runtime/fake/Bundles")).unwrap();
        fs::write(f.root.join("runtime/fake/Bundles/asset"), "runtime asset").unwrap();
        f
    }
    fn new() -> Self {
        Self::create(None)
    }
    fn create(fixed: Option<FixedGame>) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "homerun-runner-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let name = if cfg!(windows) { "fake.exe" } else { "fake" };
        let runtime = match &fixed {
            Some(fixed) => fixed.runtime.clone(),
            None => {
                let runtime = root.join("runtime");
                fs::create_dir_all(runtime.join("fake")).unwrap();
                fs::copy(
                    std::env::current_exe().unwrap(),
                    runtime.join("fake").join(name),
                )
                .unwrap();
                runtime
            }
        };
        let digest = fetcher::digest_of(&runtime.join("fake").join(name)).unwrap();
        fs::write(runtime.join("fake/.homerun-build"), &digest[..12]).unwrap();
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
        Self {
            root,
            runtime,
            d,
            port,
            _fixed: fixed,
        }
    }
    fn start(&self) -> Value {
        json!({"cmd":"start","serverId":"s1","descriptor":self.d,
        "runtimeRoot":self.runtime, "serverDir":self.root.join("server"),
        "serverName":"{secret:rcon}","settings":{},"secrets":{"rcon":"do-not-print-this"},"licenceAccepted":true})
    }
}

#[cfg(windows)]
#[test]
fn runtime_assets_and_server_saves_work_together_and_unmount_on_eof() {
    let f = Fixture::with_runtime_saves();
    let mut h = Host::new();
    h.send(f.start());
    h.until("server-started");
    assert_eq!(
        fs::read_to_string(f.root.join("server/world/world.save")).unwrap(),
        "boot\n"
    );
    assert!(platform::is_mount(&f.root.join("runtime/fake/server")));
    h.eof();
    assert_eq!(
        fs::read_to_string(f.root.join("server/world/world.save")).unwrap(),
        "world flushed"
    );
    assert!(
        !f.root.join("runtime/fake/server").exists(),
        "the updater must never inherit an active save mount"
    );
    assert!(!f.root.join("runtime/.homerun-mounts-fake.json").exists());
    assert!(f.root.join("server/settings.json").exists());
}

#[cfg(windows)]
#[test]
fn shared_runtime_lease_refuses_a_second_runner_without_touching_saves() {
    let f = Fixture::with_runtime_saves();
    let mut first = Host::new();
    first.send(f.start());
    first.until("server-started");
    let mut second = Host::new();
    second.send(f.start());
    assert_eq!(second.until("error")["code"], "busy");
    second.eof();
    assert!(platform::is_mount(&f.root.join("runtime/fake/server")));
    first.eof();
}

#[test]
fn existing_real_runtime_save_directory_is_never_replaced() {
    let f = Fixture::with_runtime_saves();
    fs::create_dir(f.root.join("runtime/fake/server")).unwrap();
    fs::write(
        f.root.join("runtime/fake/server/vendor-world"),
        "do not remove",
    )
    .unwrap();
    let mut h = Host::new();
    h.send(f.start());
    assert_eq!(h.until("error")["code"], "fetch_failed");
    h.eof();
    assert_eq!(
        fs::read_to_string(f.root.join("runtime/fake/server/vendor-world")).unwrap(),
        "do not remove"
    );
}

#[cfg(windows)]
#[test]
fn stale_save_link_is_removed_before_downloader_runs() {
    let mut f = Fixture::with_runtime_saves();
    let target = f.root.join("server/world");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("keep"), "world data").unwrap();
    let link = f.root.join("runtime/fake/server");
    platform::mount_dir(&link, &target).unwrap();
    let journal = f.root.join("runtime/.homerun-mounts-fake.json");
    fs::write(
        &journal,
        serde_json::to_vec(
            &json!({"mounts": f.d["saves"]["mounts"], "job": "Global\\HomerunSave-fixture-absent"}),
        )
        .unwrap(),
    )
    .unwrap();
    f.d["saves"]["mounts"] = json!([]);
    fs::remove_file(f.root.join("runtime/fake/.homerun-build")).unwrap();
    let exe = if cfg!(windows) { "fake.exe" } else { "fake" };
    let bytes = fs::read(f.root.join("runtime/fake").join(exe)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    f.d["platforms"][platform::HOST]["runtime"]["url"] =
        json!(format!("http://{}/{exe}", listener.local_addr().unwrap()));
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut socket, _) = loop {
            if let Ok(connection) = listener.accept() {
                break connection;
            }
            assert!(
                Instant::now() < deadline,
                "runner never requested fixture download"
            );
            thread::sleep(Duration::from_millis(5));
        };
        // Windows accepts can inherit the listener's nonblocking mode.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).unwrap();
        let clean_before_fetch = !platform::is_mount(&link) && !journal.exists();
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        )
        .unwrap();
        socket.write_all(&bytes).unwrap();
        clean_before_fetch
    });
    let mut h = Host::new();
    h.send(json!({"cmd":"fetch","serverId":"s1","runtimeRoot":f.root.join("runtime"),"descriptor":f.d,"licenceAccepted":true}));
    h.until("fetch-complete");
    h.eof();
    assert!(
        server.join().unwrap(),
        "the downloader was started while a stale save link still existed"
    );
    assert_eq!(
        fs::read_to_string(target.join("keep")).unwrap(),
        "world data"
    );
}

#[cfg(windows)]
#[test]
fn runtime_save_mounts_are_removed_on_game_crash() {
    let f = Fixture::with_runtime_saves();
    let mut h = Host::new();
    h.send(f.start());
    h.until("server-started");
    h.send(json!({"cmd":"console","serverId":"s1","command":"crash"}));
    h.until("server-crashed");
    h.eof();
    assert!(!f.root.join("runtime/fake/server").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("server/world/world.save")).unwrap(),
        "boot\n"
    );
}

#[cfg(windows)]
#[test]
fn a_spawn_failure_rolls_back_created_save_mounts() {
    let f = Fixture::with_runtime_saves();
    let exe = if cfg!(windows) { "fake.exe" } else { "fake" };
    fs::write(f.root.join("runtime/fake").join(exe), "not an executable").unwrap();
    let mut h = Host::new();
    h.send(f.start());
    h.until("server-crashed");
    h.eof();
    assert!(
        !f.root.join("runtime/fake/server").exists(),
        "failed spawn leaked a save junction"
    );
    assert!(!f.root.join("runtime/.homerun-mounts-fake.json").exists());
}

#[test]
fn save_mount_parents_cannot_redirect_either_end() {
    for runtime_side in [true, false] {
        let mut f = Fixture::with_runtime_saves();
        let outside = f.root.join("unrelated");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "unchanged").unwrap();
        let base = if runtime_side {
            f.root.join("runtime/fake")
        } else {
            f.root.join("server")
        };
        fs::create_dir_all(&base).unwrap();
        platform::mount_dir(&base.join("alias"), &outside).unwrap();
        let side = if runtime_side { "runtime" } else { "server" };
        f.d["saves"]["mounts"][0][side] = json!("alias/world");
        let mut h = Host::new();
        h.send(f.start());
        assert_eq!(h.until("error")["code"], "descriptor_invalid");
        h.eof();
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "unchanged"
        );
        assert!(!outside.join("world").exists());
        platform::unmount_dir(&base.join("alias")).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn hard_kill_recovers_stale_mount_before_another_server_uses_runtime() {
    let f = Fixture::with_runtime_saves();
    let mut first = Host::new();
    let mut command = f.start();
    command["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_MODE"] =
        json!("orphan");
    first.send(command);
    first.until("server-started");
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    // Immediately recover: production does not wait for the child's port.
    assert!(
        platform::is_mount(&f.root.join("runtime/fake/server")),
        "hard kill must actually exercise stale-junction recovery"
    );
    // Recovery is not tied to the current descriptor: even a fetch after
    // removing saves.mounts must remove the old link before touching runtime.
    let mut recovery = Host::new();
    let mut descriptor = f.d.clone();
    descriptor["saves"]["mounts"] = json!([]);
    recovery.send(json!({"cmd":"fetch","serverId":"s1","runtimeRoot":f.root.join("runtime"),"descriptor":descriptor,"licenceAccepted":true}));
    recovery.until("fetch-complete");
    recovery.eof();
    assert!(
        !f.root.join("runtime/fake/server").exists(),
        "fetch kept a stale mount from an older descriptor"
    );
    let mut second = Host::new();
    let mut command = f.start();
    command["serverDir"] = json!(f.root.join("server-b"));
    second.send(command);
    second.until("server-started");
    second.eof();
    assert_eq!(
        fs::read_to_string(f.root.join("server/world/world.save")).unwrap(),
        "boot\n",
        "server B wrote into server A's world"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("server-b/world/world.save")).unwrap(),
        "world flushed"
    );
    assert!(!f.root.join("runtime/fake/server").exists());
}

#[cfg(windows)]
#[test]
fn recovery_drains_a_still_live_job_before_removing_its_mounts() {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::{
        JobObjects::OpenJobObjectW, SystemServices::JOB_OBJECT_QUERY,
    };
    let f = Fixture::with_runtime_saves();
    let mut first = Host::new();
    let mut command = f.start();
    command["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_MODE"] =
        json!("orphan");
    let descendant_port = free_port();
    command["descriptor"]["platforms"][platform::HOST]["launch"]["env"]
        ["HOMERUN_TEST_GRANDCHILD"] = json!(descendant_port.to_string());
    first.send(command);
    first.until("server-started");
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpListener::bind(("127.0.0.1", descendant_port)).is_ok() {
        assert!(
            Instant::now() < deadline,
            "fixture descendant did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let record: Value = serde_json::from_slice(
        &fs::read(f.root.join("runtime/.homerun-mounts-fake.json")).unwrap(),
    )
    .unwrap();
    let name: Vec<u16> = record["job"]
        .as_str()
        .unwrap()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // Keep one extra handle: last-handle-close cannot kill the game yet.
    // This deterministically exercises recovery while old code is still alive.
    let handle = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, name.as_ptr()) };
    assert!(!handle.is_null());
    let _retained = unsafe { OwnedHandle::from_raw_handle(handle) };
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    assert!(
        TcpListener::bind(("127.0.0.1", f.port)).is_err(),
        "fixture must still be alive before recovery"
    );
    let mut recovery = Host::new();
    let mut descriptor = f.d.clone();
    descriptor["saves"]["mounts"] = json!([]);
    recovery.send(json!({"cmd":"fetch","serverId":"s1","runtimeRoot":f.root.join("runtime"),"descriptor":descriptor,"licenceAccepted":true}));
    recovery.until("fetch-complete");
    assert!(
        TcpListener::bind(("127.0.0.1", f.port)).is_ok(),
        "updating was allowed while the old game still owned its port"
    );
    assert!(
        TcpListener::bind(("127.0.0.1", descendant_port)).is_ok(),
        "recovery left a descendant using the old runtime"
    );
    assert!(!f.root.join("runtime/fake/server").exists());
    recovery.eof();
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
    // Readiness must actually wait for the descendant used by this assertion.
    start["descriptor"]["ports"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"descendant","proto":"tcp","port":grandchild,"expose":false}));
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
    let f = Fixture::binding_wide();
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

/// Serve `body` over plain HTTP on loopback for a few requests. Loopback only,
/// so no firewall is involved and nothing leaves the machine.
fn serve(body: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    port
}

/// A component is downloaded, verified and stamped in its own folder before
/// the game starts, and can hold the program itself -- a Java server's
/// `java` lives in its JRE component, not in the vendor's download.
#[test]
fn a_component_is_fetched_into_its_own_folder_and_can_hold_the_program() {
    let mut f = Fixture::new();
    let name = if cfg!(windows) { "fake.exe" } else { "fake" };
    let exe = fs::read(f.root.join("runtime/fake").join(name)).unwrap();
    let digest = fetcher::digest_of(&f.root.join("runtime/fake").join(name)).unwrap();
    let port = serve(exe);
    let launch_exe = if cfg!(windows) {
        "part/fake.exe"
    } else {
        "part/fake"
    };
    let host = &mut f.d["platforms"][platform::HOST];
    host["components"] = json!([{ "name": "part", "runtime": {
        "source": "direct", "url": format!("http://127.0.0.1:{port}/{name}"),
        "sha256": digest, "extract": "none" } }]);
    host["launch"]["exe"] = json!(launch_exe);

    let mut h = Host::new();
    h.send(f.start());
    h.until("server-started");

    let part = f.root.join("runtime/fake/part");
    assert!(
        part.join(name).is_file(),
        "the component's program must be in its own folder"
    );
    assert_eq!(
        fs::read_to_string(part.join(".homerun-build"))
            .unwrap()
            .trim(),
        &digest[..12],
        "a component carries its own stamp"
    );
    assert!(
        h.seen
            .iter()
            .any(|v| v["event"] == "fetch-progress" && v["phase"] == "download"),
        "the component was downloaded, not assumed: {:?}",
        h.seen
    );
    h.eof();
}

/// A component whose download does not match its pin is never used.
#[test]
fn a_component_that_fails_its_checksum_stops_the_start() {
    let mut f = Fixture::new();
    let port = serve(b"not the pinned bytes".to_vec());
    f.d["platforms"][platform::HOST]["components"] = json!([{ "name": "part", "runtime": {
        "source": "direct", "url": format!("http://127.0.0.1:{port}/thing.zip"),
        "sha256": "0".repeat(64), "extract": "zip" } }]);

    let mut h = Host::new();
    h.send(f.start());
    let error = h.until("error");
    assert_eq!(error["code"], "fetch_failed", "{error}");
    assert!(!f.root.join("runtime/fake/part/.homerun-build").exists());
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

/// A JSON config keeps each setting's type and can reach nested members.
/// Hytale's server refused `"MaxPlayers": "10"` at config load and never
/// became ready; its default game mode lives one object down.
#[test]
fn json_config_values_keep_their_type_and_dotted_keys_nest() {
    let mut f = Fixture::new();
    f.d["settings"] = json!([
        {"key":"hostname","type":"string","default":"{serverName}"},
        {"key":"maxPlayers","type":"int","default":10},
        {"key":"pvp","type":"bool","default":false},
        {"key":"mode","type":"string","default":"Adventure"}
    ]);
    f.d["config"] = json!([{"file":"settings.json","format":"json","keys":{
        "hostname":"{setting:hostname}", "MaxPlayers":"{setting:maxPlayers}",
        "Pvp":"{setting:pvp}", "Defaults.GameMode":"{setting:mode}",
        "Motd":"{setting:maxPlayers} slots"
    }}]);
    fs::create_dir_all(f.root.join("server")).unwrap();
    fs::write(
        f.root.join("server/settings.json"),
        r#"{"Defaults":{"World":"default","GameMode":"Creative"},"keep":"mine"}"#,
    )
    .unwrap();

    let mut h = Host::new();
    let mut start = f.start();
    start["settings"] = json!({ "maxPlayers": 12, "pvp": true });
    h.send(start);
    h.until("server-started");

    let config: Value =
        serde_json::from_slice(&fs::read(f.root.join("server/settings.json")).unwrap()).unwrap();
    assert_eq!(
        config["MaxPlayers"],
        json!(12),
        "an int setting must be a JSON number: {config}"
    );
    assert_eq!(
        config["Pvp"],
        json!(true),
        "a bool setting must be a JSON boolean: {config}"
    );
    assert_eq!(
        config["Motd"],
        json!("12 slots"),
        "text around a placeholder stays text: {config}"
    );
    assert_eq!(config["Defaults"]["GameMode"], "Adventure", "{config}");
    assert_eq!(
        config["Defaults"]["World"], "default",
        "a nested sibling must survive: {config}"
    );
    assert_eq!(config["keep"], "mine", "{config}");
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

/// A vendor archive shaped like Terraria's: everything under one top folder
/// named for the version's digits.
fn vendor_zip(exe: &str, body: &[u8]) -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
        writer.add_directory("1458/", options).unwrap();
        writer.start_file(format!("1458/{exe}"), options).unwrap();
        writer.write_all(body).unwrap();
        writer
            .start_file("1458/Linux/TerrariaServer", options)
            .unwrap();
        writer.write_all(b"not this platform").unwrap();
        writer.finish().unwrap();
    }
    buffer.into_inner()
}

/// Serves each body in turn, one connection each, and records the paths
/// asked for.
fn serve_in_turn(bodies: Vec<Vec<u8>>) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = asked.clone();
    thread::spawn(move || {
        for body in bodies {
            let Ok((mut client, _)) = listener.accept() else {
                return;
            };
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            log.lock()
                .unwrap()
                .push(first.split(' ').nth(1).unwrap_or("").to_string());
            write!(
                client,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            client.write_all(&body).unwrap();
        }
    });
    (port, asked)
}

/// The vendor source end to end: the host names a version, the runner
/// fetches it from the descriptor's address into its own directory with the
/// archive's top folder stripped, a second fetch of that version reuses it
/// without a request, and a vendor that later serves different bytes for
/// the same version is refused -- with the first download's digest, recorded
/// on this machine, as the reason.
#[test]
fn a_vendor_runtime_is_fetched_per_version_and_a_changed_file_is_refused() {
    let mut f = Fixture::new();
    let exe = f.d["platforms"][platform::HOST]["launch"]["exe"]
        .as_str()
        .unwrap()
        .to_owned();
    let first = vendor_zip(&exe, b"version one");
    let changed = vendor_zip(&exe, b"version one, but not the same bytes");
    let (port, asked) = serve_in_turn(vec![first.clone(), changed]);
    f.d["platforms"][platform::HOST]["runtime"] = json!({
        "source": "vendor",
        "url": format!("http://127.0.0.1:{port}/api/download/terraria-server-{{versionDigits}}.zip"),
        "extract": "zip",
        "stripComponents": 1,
        "versionSetting": "version"
    });
    f.d["settings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key":"version","type":"string","label":"Version","default":"latest"}));
    let fetch = |version: Option<&str>| {
        let mut command = json!({"cmd":"fetch","serverId":"s1","runtimeRoot":f.root.join("runtime"),
            "descriptor":f.d,"licenceAccepted":true});
        if let Some(version) = version {
            command["runtimeVersion"] = json!(version);
        }
        command
    };
    let version_dir = f.root.join("runtime/fake/1.4.5.8");
    let record = f.root.join("runtime/fake/.vendor-hashes.json");

    // The host has to say which version; the runner never picks one.
    for missing_or_bad in [None, Some("latest"), Some("1.4/../../x")] {
        let mut h = Host::new();
        h.send(fetch(missing_or_bad));
        let error = h.until("error");
        assert_eq!(error["code"], "descriptor_invalid", "{error}");
        h.eof();
    }
    assert!(asked.lock().unwrap().is_empty(), "nothing was requested");

    let mut h = Host::new();
    h.send(fetch(Some("1.4.5.8")));
    let complete = h.until("fetch-complete");
    h.eof();
    assert_eq!(complete["buildId"], "v1.4.5.8");
    assert_eq!(
        fs::canonicalize(complete["runtimeDir"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&version_dir).unwrap()
    );
    assert_eq!(
        asked.lock().unwrap().as_slice(),
        ["/api/download/terraria-server-1458.zip"]
    );
    assert_eq!(
        fs::read(version_dir.join(&exe)).unwrap(),
        b"version one",
        "the archive's top folder is stripped"
    );
    assert!(!version_dir.join("1458").exists());
    let recorded: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    let served = f.root.join("served.zip");
    fs::write(&served, &first).unwrap();
    assert_eq!(
        recorded["1.4.5.8"],
        fetcher::digest_of(&served).unwrap(),
        "the first download of a version is what later ones are held to"
    );

    // Already on disk: no second request. Asked through the standalone CLI,
    // whose --runtime-version is the same field.
    let descriptor = f.root.join("game.json");
    fs::write(&descriptor, f.d.to_string()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_homerun-game"))
        .args([
            "fetch",
            &descriptor.to_string_lossy(),
            "--accept-licence",
            "--json",
        ])
        .args(["--runtime-version", "1.4.5.8"])
        .arg("--runtime-root")
        .arg(f.root.join("runtime"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(result.status.success(), "{stdout}");
    let complete: Value = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|e| e["event"] == "fetch-complete")
        .unwrap_or_else(|| panic!("{stdout}"));
    assert_eq!(complete["buildId"], "v1.4.5.8");
    assert_eq!(
        asked.lock().unwrap().len(),
        1,
        "a present version is reused"
    );

    // The version's directory is gone and the vendor now serves other bytes
    // under the same version.
    fs::remove_dir_all(&version_dir).unwrap();
    let mut h = Host::new();
    h.send(fetch(Some("1.4.5.8")));
    let error = h.until("error");
    h.eof();
    assert_eq!(error["code"], "fetch_failed", "{error}");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("changed") && message.contains("1.4.5.8"),
        "{message}"
    );
    assert_eq!(asked.lock().unwrap().len(), 2);
    assert!(
        !version_dir.join(&exe).exists(),
        "a refused download is never unpacked"
    );
    assert!(!version_dir.join(".homerun-build").exists());
    let still: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    assert_eq!(still, recorded, "a refusal does not rewrite the record");
}

/// Launching uses the version's directory for everything that names the
/// runtime: the executable, a `runtime` working directory, `{runtimeDir}`
/// and save mounts. The fake game asserts its cwd is `{runtimeDir}` and
/// finds its asset there.
#[cfg(windows)]
#[test]
fn a_vendor_runtime_is_launched_from_its_versions_directory() {
    let mut f = Fixture::with_runtime_saves();
    let game = f.root.join("runtime/fake");
    let version_dir = game.join("1.4.5.8");
    fs::create_dir_all(&version_dir).unwrap();
    for entry in fs::read_dir(&game).unwrap() {
        let entry = entry.unwrap();
        if entry.path() != version_dir {
            fs::rename(entry.path(), version_dir.join(entry.file_name())).unwrap();
        }
    }
    fs::write(version_dir.join(".homerun-build"), "v1.4.5.8").unwrap();
    f.d["platforms"][platform::HOST]["runtime"] = json!({
        "source": "vendor",
        // Never reached: the version is already on disk.
        "url": "http://127.0.0.1:1/terraria-server-{versionDigits}.zip",
        "extract": "zip",
        "versionSetting": "version"
    });
    f.d["settings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key":"version","type":"string","label":"Version","default":"latest"}));
    let mut start = f.start();
    start["runtimeVersion"] = json!("1.4.5.8");
    let mut h = Host::new();
    h.send(start);
    let complete = h.until("fetch-complete");
    assert_eq!(complete["buildId"], "v1.4.5.8");
    h.until("server-started");
    assert!(platform::is_mount(&version_dir.join("server")));
    assert_eq!(
        fs::read_to_string(f.root.join("server/world/world.save")).unwrap(),
        "boot\n"
    );
    h.eof();
    assert!(
        !version_dir.join("server").exists(),
        "the save mount is removed"
    );
    assert!(!f.root.join("runtime/.homerun-mounts-fake.json").exists());
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

/// The host publishes `server-log` straight into a player's console and files
/// `tail` with a crash report, so a secret the *game* prints is a secret the
/// host would hand out. Redaction is the only thing between the two: the
/// password has to be on the vendor's command line for the vendor to accept
/// it, and a vendor that echoes it is not doing anything wrong.
#[test]
fn a_secret_the_game_echoes_never_leaves_the_runner() {
    let mut f = Fixture::new();
    f.d["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_ECHO"] = json!("{secret:rcon}");
    let mut h = Host::new();
    h.send(f.start());
    h.until("server-started");

    let logs: Vec<String> = h
        .seen
        .iter()
        .filter(|e| e["event"] == "server-log")
        .map(|e| e["line"].as_str().unwrap_or_default().to_string())
        .collect();
    // The line arrived -- otherwise this passes because nothing was echoed.
    assert!(
        logs.iter().any(|l| l.contains("+rcon.password [redacted]")),
        "the echoed launch line should arrive redacted; saw {logs:?}"
    );
    assert!(
        !logs.iter().any(|l| l.contains("do-not-print-this")),
        "a secret reached the host as a log line; saw {logs:?}"
    );

    // The same guarantee through the other sink: the crash tail is filled from
    // the redacted line, not the raw one.
    h.send(json!({"cmd":"console","serverId":"s1","command":"crash","reqId":"crash"}));
    let crash = h.until("server-crashed");
    let tail = crash["tail"].as_array().unwrap();
    assert!(
        tail.iter()
            .any(|v| v.as_str().unwrap_or_default().contains("[redacted]")),
        "the crash tail should carry the redacted line; saw {tail:?}"
    );
    assert!(
        !tail
            .iter()
            .any(|v| v.as_str().unwrap_or_default().contains("do-not-print-this")),
        "a secret reached the host in a crash tail; saw {tail:?}"
    );
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

#[test]
fn network_private_exposure_without_marker_skips_save_grace() {
    let f = Fixture::binding_wide();
    let mut start = f.start();
    start["descriptor"]["ports"][0]["expose"] = json!(false);
    start["descriptor"]["stop"]["graceMs"] = json!(180000);
    start["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_BIND"] =
        json!("0.0.0.0");
    start["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_MODE"] =
        json!("silent");
    let mut h = Host::new();
    let now = Instant::now();
    h.send(start);
    assert_eq!(h.until("error")["code"], "port_exposed");
    h.until("server-crashed");
    assert!(
        now.elapsed() < Duration::from_secs(5),
        "exposure waited for readiness or save grace"
    );
    assert!(!h.seen.iter().any(|e| e["event"] == "server-started"));
    assert!(
        !f.root.join("server/saved").exists(),
        "a security refusal must bypass ordinary quit"
    );
    assert!(port_freed(f.port));
    h.eof();
}

#[test]
fn network_private_port_widening_after_ready_is_refused() {
    let f = Fixture::binding_wide();
    let mut start = f.start();
    start["descriptor"]["ports"][0]["expose"] = json!(false);
    let mut h = Host::new();
    h.send(start);
    h.until("server-started");
    h.send(json!({"cmd":"console","serverId":"s1","command":"widen"}));
    assert_eq!(h.until("error")["code"], "port_exposed");
    h.until("server-crashed");
    assert_eq!(
        h.seen
            .iter()
            .filter(|e| e["event"] == "server-started")
            .count(),
        1
    );
    assert!(port_freed(f.port));
    h.eof();
}

#[test]
fn network_private_port_widening_during_stop_is_refused() {
    let f = Fixture::binding_wide();
    let mut start = f.start();
    start["descriptor"]["ports"][0]["expose"] = json!(false);
    start["descriptor"]["stop"]["graceMs"] = json!(180000);
    start["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_MODE"] =
        json!("wide-on-stop");
    let mut h = Host::new();
    h.send(start);
    h.until("server-started");
    let now = Instant::now();
    h.send(json!({"cmd":"stop","serverId":"s1"}));
    assert_eq!(h.until("error")["code"], "port_exposed");
    h.until("server-crashed");
    assert!(
        now.elapsed() < Duration::from_secs(5),
        "shutdown disabled the network guard"
    );
    assert!(
        !h.seen.iter().any(|e| e["event"] == "server-stopped"),
        "refusal mislabeled a clean stop"
    );
    h.eof();
}

#[cfg(windows)]
#[test]
fn network_private_descendant_is_inspected_and_terminated() {
    let f = Fixture::binding_wide();
    let extra = free_port();
    let mut start = f.start();
    start["descriptor"]["ports"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"admin","proto":"tcp","port":extra,"expose":false}));
    let env = &mut start["descriptor"]["platforms"][platform::HOST]["launch"]["env"];
    env["HOMERUN_TEST_GRANDCHILD"] = json!(extra.to_string());
    env["HOMERUN_TEST_DESCENDANT_BIND"] = json!("0.0.0.0");
    let mut h = Host::new();
    h.send(start);
    assert_eq!(h.until("error")["code"], "port_exposed");
    h.until("server-crashed");
    assert!(
        port_freed(extra),
        "owned descendant retained its exposed socket"
    );
    h.eof();
}

#[test]
fn network_verify_records_loopback_and_refuses_undeclared_wildcard() {
    for (bind, success) in [("127.0.0.1", true), ("0.0.0.0", false)] {
        let mut f = Fixture::binding_wide();
        let extra = free_port();
        let env = &mut f.d["platforms"][platform::HOST]["launch"]["env"];
        env["HOMERUN_TEST_EXTRA_PORT"] = json!(extra.to_string());
        env["HOMERUN_TEST_EXTRA_BIND"] = json!(bind);
        let path = f.root.join("game.json");
        fs::write(&path, f.d.to_string()).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_homerun-game"))
            .arg("verify")
            .arg(path)
            .args(["--accept-licence", "--observe-seconds", "2", "--json"])
            .arg("--runtime-root")
            .arg(&f.runtime)
            .arg("--server-dir")
            .arg(f.root.join("server"))
            .arg("--evidence")
            .arg(f.root.join("evidence"))
            .output()
            .unwrap();
        let report: Value =
            serde_json::from_slice(&fs::read(f.root.join("evidence/probe.json")).unwrap()).unwrap();
        assert_eq!(result.status.success(), success, "{report}");
        assert_eq!(report["ok"], success);
        let row = report["network"]["inventory"]["listeners"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["port"] == extra)
            .expect("undeclared listener absent from evidence");
        assert_eq!(row["declared"], false);
        assert_eq!(row["address"], bind);
        assert_eq!(row["confined"], success);
        if !success {
            assert!(report["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["code"] == "port_exposed"));
        }
        assert!(port_freed(extra));
    }
}

#[cfg(windows)]
#[test]
fn network_refusal_kills_even_when_runner_output_is_not_drained() {
    let f = Fixture::binding_wide();
    let mut start = f.start();
    start["descriptor"]["ports"][0]["expose"] = json!(false);
    start["descriptor"]["stop"]["graceMs"] = json!(180000);
    start["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_MODE"] =
        json!("flood-then-widen");
    let mut host = Command::new(env!("CARGO_BIN_EXE_homerun-game"))
        .arg("supervise")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(host.stdin.as_mut().unwrap(), "{start}").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !f.root.join("server/wide-seen").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let widened = f.root.join("server/wide-seen").exists();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut closed = false;
    while Instant::now() < deadline {
        if TcpListener::bind(("127.0.0.1", f.port)).is_ok() {
            closed = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Always clean the fixture, including when the deliberately broken watchdog fails.
    let _ = host.kill();
    let _ = host.wait();
    assert!(widened, "fixture never bound wide");
    assert!(
        closed,
        "a blocked output pipe prevented the watchdog from closing the exposed socket"
    );
}

#[cfg(windows)]
#[test]
fn network_descendants_are_gone_before_clean_stop_is_reported() {
    let f = Fixture::new();
    let extra = free_port();
    let mut start = f.start();
    start["descriptor"]["ports"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"admin","proto":"tcp","port":extra,"expose":false}));
    start["descriptor"]["platforms"][platform::HOST]["launch"]["env"]["HOMERUN_TEST_GRANDCHILD"] =
        json!(extra.to_string());
    let mut h = Host::new();
    h.send(start);
    h.until("server-started");
    h.send(json!({"cmd":"stop","serverId":"s1"}));
    h.until("server-stopped");
    assert!(
        TcpListener::bind(("127.0.0.1", extra)).is_ok(),
        "runner reported clean stop while an owned descendant still listened"
    );
    h.eof();
}

//! Driving an extension without a runner: its `begin`, `Run` and `on_stop`
//! against a recording `Output`, a real `Prompts` a helper thread answers,
//! and stores under a temporary runtime root. Most of an extension's tests
//! need nothing more. Test builds with `test-extensions` only.

use super::*;
use std::sync::Mutex as StdMutex;

/// A fake of Hytale's services on loopback, shared with the lifecycle tests.
#[path = "../../tests/support/fake_hytale.rs"]
pub mod fake_hytale;

/// A fake of Palworld's REST API, shared with the lifecycle tests' fake game.
#[path = "../../tests/support/fake_palworld.rs"]
pub mod fake_palworld;

/// A runtime root of its own, under a fresh temporary directory, so each
/// harness's machine store is its own.
pub fn runtime_root() -> std::path::PathBuf {
    use std::sync::atomic::AtomicUsize;
    static N: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "homerun-ext-harness-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(root.join("runtime")).unwrap();
    root.join("runtime")
}

use serde_json::json;

pub struct Harness {
    spec: &'static ExtensionSpec,
    config: Value,
    descriptor: GameDescriptor,
    stop: StopSignal,
    out: Output,
    seen: Arc<StdMutex<Vec<Event>>>,
    prompts: Prompts,
    root: std::path::PathBuf,
}

impl Harness {
    /// The reference extension, `fixture`.
    pub fn new(config: Value) -> Self {
        Self::for_extension("fixture", config)
    }

    /// Any registered extension, with this config.
    pub fn for_extension(name: &str, config: Value) -> Self {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let s = seen.clone();
        Self {
            spec: specs::spec(name).unwrap(),
            config,
            descriptor: GameDescriptor::default(),
            stop: StopSignal::default(),
            out: Output::new(move |e| s.lock().unwrap().push(e)),
            seen,
            prompts: Prompts::default(),
            root: runtime_root(),
        }
    }

    pub fn begin(&self) -> std::result::Result<Begun, ExtError> {
        let mut ctx = StartContext {
            spec: self.spec,
            config: &self.config,
            descriptor: &self.descriptor,
            server_id: "s1",
            stop: &self.stop,
            out: &self.out,
            machine: self.machine(),
            server: self.server(),
            policy: policy(self.spec, &self.config),
            prompts: &self.prompts,
        };
        registry()
            .into_iter()
            .find(|e| e.name() == self.spec.name)
            .unwrap()
            .begin(&mut ctx)
    }

    pub fn machine(&self) -> MachineStore {
        MachineStore::new(&crate::prepare::tools_dir(&self.root), self.spec.name)
    }

    pub fn server(&self) -> ServerStore {
        ServerStore::new(&self.root.join("server"), self.spec.name)
    }

    /// `forget` and `status`'s context, on this harness's machine store.
    pub fn machine_context(&self) -> MachineContext {
        machine_context(self.spec, &self.root)
    }

    /// Run `on_stop`, as the runner does once the server has exited.
    pub fn stop(&self, run: &mut Box<dyn Run>, outcome: Outcome) {
        let ctx = StopContext {
            config: self.config.clone(),
            server_id: "s1".into(),
            deadline: Instant::now() + STOP_BUDGET,
            out: self.out.clone(),
            machine: self.machine(),
            server: self.server(),
            policy: policy(self.spec, &self.config),
        };
        run.on_stop(&ctx, outcome);
    }

    /// Run the extension's `stop`, as the ladder's polite rung does, with
    /// what it may reach on loopback resolved against `d`, the bound `ports`
    /// and the host's `secrets`. What it returned, and what it noted.
    pub fn stop_rung(
        &self,
        d: &GameDescriptor,
        ports: &[(&str, u16)],
        secrets: &[(&str, &str)],
    ) -> (std::result::Result<(), ExtError>, Vec<String>) {
        let ports = ports.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let secrets = secrets
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let local = resolve_local(self.spec, &self.config, d, &ports, &secrets).unwrap();
        let notes = StdMutex::new(Vec::new());
        let note = |text: String| notes.lock().unwrap().push(text);
        let begun = Instant::now();
        let cancelled = || begun.elapsed() > Duration::from_secs(10);
        let asking = Asking::new(&cancelled, &note);
        let ctx = StopRungContext {
            name: self.spec.name,
            config: &self.config,
            server_id: "s1",
            local: &local,
            asking: &asking,
        };
        let result = registry()
            .into_iter()
            .find(|e| e.name() == self.spec.name)
            .unwrap()
            .stop(&ctx);
        (result, notes.into_inner().unwrap())
    }

    /// Ask the extension's `probe_ready` once, as the runner's probe thread
    /// does, with what it may reach resolved as for `stop_rung`.
    pub fn probe(
        &self,
        d: &GameDescriptor,
        ports: &[(&str, u16)],
        secrets: &[(&str, &str)],
    ) -> std::result::Result<bool, ExtError> {
        let ports = ports.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let secrets = secrets
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let local = resolve_local(self.spec, &self.config, d, &ports, &secrets).unwrap();
        let begun = Instant::now();
        let cancelled = || begun.elapsed() > Duration::from_secs(10);
        let ctx = ProbeContext {
            name: self.spec.name,
            config: &self.config,
            server_id: "s1",
            local: &local,
            cancelled: &cancelled,
        };
        registry()
            .into_iter()
            .find(|e| e.name() == self.spec.name)
            .unwrap()
            .probe_ready(&ctx)
    }

    /// Every event sent so far.
    pub fn events(&self) -> Vec<Event> {
        self.seen.lock().unwrap().clone()
    }

    /// Ask for a stop, as the player's Stop does.
    pub fn request_stop(&self) {
        self.stop.request_stop();
    }

    /// Answer the next prompt with `value`, from another thread.
    pub fn answer_with(&self, value: &'static str) -> JoinHandle<()> {
        let (prompts, seen) = (self.prompts.clone(), self.seen.clone());
        thread::spawn(move || loop {
            let open = seen.lock().unwrap().iter().rev().find_map(|e| match e {
                Event::Prompt { prompt_id, .. } => Some(prompt_id.clone()),
                _ => None,
            });
            if let Some(id) = open {
                prompts.answer("s1", &id, value.into()).unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(10));
        })
    }
}

#[test]
fn a_prompt_is_asked_once_per_server_and_its_answer_supplied() {
    let h = Harness::new(json!({ "host": "vendor.example", "askProfile": ["a", "b"] }));
    let answering = h.answer_with("b");
    let begun = h.begin().unwrap();
    answering.join().unwrap();
    assert_eq!(begun.supplied["profile"], "b");
    // Asked once: the second run remembers the answer, and asks nothing.
    let prompts_before = h.seen.lock().unwrap().len();
    assert_eq!(h.begin().unwrap().supplied["profile"], "b");
    assert_eq!(h.seen.lock().unwrap().len(), prompts_before);
}

#[test]
fn a_remembered_sign_in_survives_between_runs_and_is_forgotten_on_sign_out() {
    let h = Harness::new(json!({ "host": "vendor.example", "remember": true }));
    h.begin().unwrap();
    h.begin().unwrap();
    let store = MachineStore::new(&crate::prepare::tools_dir(&h.root), "fixture");
    assert_eq!(store.read().unwrap().unwrap()["starts"], 2);
    let ctx = machine_context(h.spec, &h.root);
    let fixture = registry()
        .into_iter()
        .find(|e| e.name() == "fixture")
        .unwrap();
    assert_eq!(fixture.status(&ctx).signed_in, Some(true));
    fixture.forget(&ctx).unwrap();
    assert_eq!(fixture.status(&ctx).signed_in, Some(false));
}

/// The output pump's budget: an observer that returned slowly would
/// stall `server-log`. Ten thousand lines in well under a second.
#[test]
fn an_observer_keeps_up_with_a_line_storm() {
    let h = Harness::new(json!({
        "host": "vendor.example", "consoleOn": "never", "command": "x"
    }));
    let mut run = h.begin().unwrap().run;
    let begun = Instant::now();
    for i in 0..10_000 {
        run.on_line(&format!("[world] generating chunk {i}"), "stdout");
    }
    assert!(
        begun.elapsed() < Duration::from_secs(1),
        "{:?}",
        begun.elapsed()
    );
}

#[test]
fn http_to_a_host_the_spec_does_not_name_is_refused_in_words() {
    let h = Harness::new(json!({
        "host": "vendor.example", "vendorUrl": "https://evil.example/hello"
    }));
    let error = h.begin().err().unwrap();
    assert_eq!(error.code, codes::EXTENSION_FAILED);
    assert!(
        error.message.contains("does not trust"),
        "{}",
        error.message
    );
}

// ─── Palworld's stop, against a fake of its REST API ─────────────────────────

mod palworld {
    use super::fake_palworld::{serve, Behaviour};
    use super::*;
    use std::net::TcpListener;

    /// The descriptor the stop is resolved against: a private REST port.
    fn descriptor() -> GameDescriptor {
        serde_json::from_value(json!({
            "id": "palworld",
            "ports": [{ "name": "rest", "proto": "tcp", "port": 8212, "expose": false }],
            "stop": { "via": "extension" },
            "extension": { "name": "palworld" }
        }))
        .unwrap()
    }

    /// What the fake saw, and whether it saved.
    type Seen = Arc<StdMutex<(Vec<String>, bool)>>;

    /// Serve the fake on a thread of its own. A stop that failed leaves it
    /// waiting on its port, which ends with the test process.
    fn fake(behaviour: Behaviour) -> (u16, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Seen = Arc::default();
        let (lines, saved) = (seen.clone(), seen.clone());
        thread::spawn(move || {
            serve(
                listener,
                &behaviour,
                &mut |line| lines.lock().unwrap().0.push(line),
                &mut || saved.lock().unwrap().1 = true,
            )
        });
        (port, seen)
    }

    #[test]
    fn it_saves_then_shuts_down_as_admin_with_the_hosts_password() {
        let (port, served) = fake(Behaviour {
            password: "hunter2".into(),
            ..Behaviour::default()
        });
        let h = Harness::for_extension(
            "palworld",
            json!({ "shutdownWait": 3, "shutdownMessage": "Back soon" }),
        );
        let (result, notes) =
            h.stop_rung(&descriptor(), &[("rest", port)], &[("admin", "hunter2")]);
        assert_eq!(result, Ok(()));
        assert_eq!(notes, ["Palworld saved the world"]);
        let (seen, saved) = served.lock().unwrap().clone();
        assert!(saved);
        assert_eq!(
            seen,
            [
                "POST /v1/api/save HTTP/1.1 auth=true json=true {}",
                r#"POST /v1/api/shutdown HTTP/1.1 auth=true json=true {"message":"Back soon","waittime":3}"#
            ]
        );
    }

    #[test]
    fn a_refused_password_asks_nothing_more_and_says_why() {
        let (port, served) = fake(Behaviour {
            password: "hunter2".into(),
            ..Behaviour::default()
        });
        let h = Harness::for_extension("palworld", json!({}));
        let (result, _) = h.stop_rung(&descriptor(), &[("rest", port)], &[("admin", "wrong")]);
        let error = result.unwrap_err();
        assert!(
            error.message.contains("admin password"),
            "{}",
            error.message
        );
        assert!(!error.message.contains("wrong"), "{}", error.message);
        let (seen, _) = served.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(
            seen[0].starts_with("POST /v1/api/save HTTP/1.1 auth=false"),
            "{seen:?}"
        );
    }

    /// A shutdown after a failed save would be a stop that loses the world.
    #[test]
    fn a_failed_save_is_never_followed_by_a_shutdown() {
        let (port, served) = fake(Behaviour {
            password: "hunter2".into(),
            refuse_save: Some(500),
        });
        let h = Harness::for_extension("palworld", json!({}));
        let (result, notes) =
            h.stop_rung(&descriptor(), &[("rest", port)], &[("admin", "hunter2")]);
        assert_eq!(
            result.unwrap_err().message,
            "Palworld answered 500 when asked to save the world."
        );
        assert!(notes.is_empty(), "{notes:?}");
        let (seen, saved) = served.lock().unwrap().clone();
        assert!(!saved);
        assert_eq!(seen.len(), 1, "the stop sent only the save: {seen:?}");
    }

    #[test]
    fn nothing_listening_is_a_stop_that_did_not_happen() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let h = Harness::for_extension("palworld", json!({}));
        let (result, _) = h.stop_rung(&descriptor(), &[("rest", port)], &[("admin", "x")]);
        assert!(
            result.unwrap_err().message.contains("Nothing answered"),
            "a closed port answered"
        );
    }

    // ─── readiness ───────────────────────────────────────────────────────

    /// Ready once its REST API answers, asked the way the stop asks: a GET
    /// of the server's info as `admin` with the host's password, and nothing
    /// that changes the server.
    #[test]
    fn it_is_ready_once_the_rest_api_answers() {
        let (port, served) = fake(Behaviour {
            password: "hunter2".into(),
            ..Behaviour::default()
        });
        let h = Harness::for_extension("palworld", json!({}));
        let probed = h.probe(&descriptor(), &[("rest", port)], &[("admin", "hunter2")]);
        assert_eq!(probed, Ok(true));
        let (seen, saved) = served.lock().unwrap().clone();
        assert_eq!(seen, ["GET /v1/api/info HTTP/1.1 auth=true json=false "]);
        assert!(!saved);
    }

    /// A refused password is an answer, but not a yes.
    #[test]
    fn a_refused_probe_is_not_ready() {
        let (port, served) = fake(Behaviour {
            password: "hunter2".into(),
            ..Behaviour::default()
        });
        let h = Harness::for_extension("palworld", json!({}));
        let probed = h.probe(&descriptor(), &[("rest", port)], &[("admin", "wrong")]);
        assert_eq!(probed, Ok(false));
        let (seen, _) = served.lock().unwrap().clone();
        assert_eq!(seen, ["GET /v1/api/info HTTP/1.1 auth=false json=false "]);
    }

    /// Before the API is up there is nothing listening: not ready, and not
    /// a yes by mistake. The runner takes the error as "not yet".
    #[test]
    fn nothing_listening_yet_is_not_ready() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let h = Harness::for_extension("palworld", json!({}));
        let probed = h.probe(&descriptor(), &[("rest", port)], &[("admin", "x")]);
        assert_ne!(probed, Ok(true));
    }
}

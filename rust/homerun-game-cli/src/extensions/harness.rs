//! Driving an extension without a runner: its `begin`, `Run` and `on_stop`
//! against a recording `Output`, a real `Prompts` a helper thread answers,
//! and stores under a temporary runtime root. Most of an extension's tests
//! need nothing more. Test builds with `test-extensions` only.

use super::*;
use std::sync::Mutex as StdMutex;

/// A fake of Hytale's services on loopback, shared with the lifecycle tests.
#[path = "../../tests/support/fake_hytale.rs"]
pub mod fake_hytale;

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

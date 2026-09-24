//! The reference extension's effects half, for tests only.
//!
//! Built only with the `test-extensions` feature, which `npm run test:game`
//! turns on and no release build does. Its config says which behaviour to
//! show, so each lifecycle test can drive one hook at a time -- and it is
//! the example a new extension starts from, since it touches every hook and
//! every capability the context offers.
//!
//! | Key | Does |
//! |---|---|
//! | `host` | the vendor host (the pure half requires it) |
//! | `signIn` | send a server sign-in to `https://<host>/device`, then `signed-in` |
//! | `signInUrl` | send the sign-in to this URL instead, trusted or not |
//! | `awaitStop` | wait in `begin` until a stop is asked for |
//! | `panicIn` | `"begin"` or `"line"`: panic there |
//! | `consoleOn` / `command` | send `command` whenever a line contains `consoleOn` |
//! | `failOn` | fail the run when a line contains it |
//! | `supplyUndeclared` | also supply a key the spec does not declare |
//! | `askProfile` | a list of profiles: ask which one, once per server, and supply it |
//! | `remember` | count starts in the sealed machine store, as a sign-in would |
//! | `vendorUrl` | `GET` it in `begin` and note what came back |
//! | `vendorOnStop` | `DELETE` it in `on_stop` and note the status |
//!
//! It always supplies `token` (secret, [`TOKEN`]) and `profile` (plain).

use std::{collections::BTreeMap, time::Duration};

use serde_json::{json, Value};

use super::{
    Action, Begun, Choice, ExtError, ExtensionStatus, GameExtension, MachineContext, Outcome,
    Prompt, Purpose, Request, Run, RunState, SignIn, StartContext, StopContext,
};
use crate::protocol::codes;

/// The secret the fixture supplies. Tests look for it in places it must not be.
pub const TOKEN: &str = "fixture-secret-token-7c1d";

/// The account name the fixture keeps once it has been started with
/// `remember`, and reports from `status`.
const ACCOUNT: &str = "fixture account";

pub struct Fixture;

impl GameExtension for Fixture {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn begin(&self, ctx: &mut StartContext) -> Result<Begun, ExtError> {
        let config = ctx.config().clone();
        let flag = |key: &str| config.get(key).and_then(Value::as_bool).unwrap_or(false);
        let text = |key: &str| config.get(key).and_then(Value::as_str).map(str::to_string);

        if text("panicIn").as_deref() == Some("begin") {
            panic!("the fixture was told to panic in begin");
        }
        if flag("awaitStop") {
            ctx.note("waiting for you to sign in");
            while !ctx.wait(Duration::from_secs(1)) {}
            return Err(ExtError::cancelled());
        }
        let host = text("host").unwrap_or_default();
        if flag("signIn") || text("signInUrl").is_some() {
            ctx.sign_in(SignIn {
                purpose: Purpose::Server,
                url: text("signInUrl").unwrap_or(format!("https://{host}/device?code=FIX-1")),
                code: Some("FIX-1".into()),
                expires_in_secs: Some(900),
            })?;
            ctx.signed_in(Purpose::Server);
        }
        if let Some(url) = text("vendorUrl") {
            let response = ctx.http(&Request::get(url))?;
            if !response.ok() {
                return Err(ExtError::new(
                    codes::VENDOR_UNAVAILABLE,
                    format!("The fixture vendor answered {}.", response.status),
                ));
            }
            ctx.note(format!("vendor said: {}", response.text()));
        }
        if flag("remember") {
            ctx.machine_store().update(|old| {
                let starts = old.as_ref().and_then(|v| v["starts"].as_u64()).unwrap_or(0);
                Ok(Some(json!({ "account": ACCOUNT, "starts": starts + 1 })))
            })?;
        }
        let profile = match config.get("askProfile").and_then(Value::as_array) {
            None => "profile-1".to_string(),
            Some(profiles) => match ctx
                .server_store()
                .read()
                .and_then(|v| v["profile"].as_str().map(str::to_string))
            {
                Some(chosen) => chosen,
                None => {
                    let chosen = ctx.prompt(Prompt {
                        title: "Which profile hosts this server?".into(),
                        message: None,
                        options: profiles
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|p| Choice {
                                value: p.into(),
                                label: format!("Profile {p}"),
                            })
                            .collect(),
                    })?;
                    ctx.server_store().write(&json!({ "profile": chosen }))?;
                    chosen
                }
            },
        };
        let mut supplied = BTreeMap::from([
            ("token".to_string(), TOKEN.to_string()),
            ("profile".to_string(), profile),
        ]);
        if flag("supplyUndeclared") {
            supplied.insert("undeclared".into(), "x".into());
        }
        Ok(Begun {
            supplied,
            run: Box::new(FixtureRun {
                panic_on_line: text("panicIn").as_deref() == Some("line"),
                console_on: text("consoleOn"),
                command: text("command"),
                fail_on: text("failOn"),
                vendor_on_stop: text("vendorOnStop"),
            }),
        })
    }

    fn forget(&self, ctx: &MachineContext) -> Result<(), ExtError> {
        ctx.machine_store().clear()
    }

    fn status(&self, ctx: &MachineContext) -> ExtensionStatus {
        match ctx.machine_store().read() {
            Ok(Some(kept)) => ExtensionStatus {
                signed_in: Some(true),
                account: kept["account"].as_str().map(str::to_string),
            },
            Ok(None) => ExtensionStatus {
                signed_in: Some(false),
                account: None,
            },
            Err(_) => ExtensionStatus::default(),
        }
    }
}

struct FixtureRun {
    panic_on_line: bool,
    console_on: Option<String>,
    command: Option<String>,
    fail_on: Option<String>,
    vendor_on_stop: Option<String>,
}

impl Run for FixtureRun {
    fn on_line(&mut self, line: &str, _stream: &str) -> Vec<Action> {
        if self.panic_on_line {
            panic!("the fixture was told to panic on a line");
        }
        let mut actions = vec![];
        if let (Some(marker), Some(command)) = (&self.console_on, &self.command) {
            if line.contains(marker.as_str()) {
                actions.push(Action::Console(command.clone()));
            }
        }
        if let Some(marker) = &self.fail_on {
            if line.contains(marker.as_str()) {
                actions.push(Action::Fail(ExtError::new(
                    codes::ACCOUNT_NOT_ALLOWED,
                    "The fixture account is not allowed to host this server.",
                )));
            }
        }
        actions
    }

    fn on_state(&mut self, state: RunState) -> Vec<Action> {
        vec![Action::Note(format!("fixture saw {state:?}"))]
    }

    fn on_stop(&mut self, ctx: &StopContext, outcome: Outcome) {
        if let Some(url) = &self.vendor_on_stop {
            match ctx.http(&Request::delete(url.clone())) {
                Ok(response) => ctx.note(format!("vendor told: {}", response.status)),
                Err(e) => ctx.note(format!("vendor not told: {}", e.message)),
            }
        }
        ctx.note(format!("fixture stopped: {outcome:?}"));
    }
}

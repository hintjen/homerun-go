//! Palworld's stop — the pure half.
//!
//! A Palworld dedicated server reads nothing on stdin, so a console stop has
//! nowhere to go, and on Windows a piped child has no console for a control
//! event either. Without this extension the only stop it gets is a
//! terminate, and whatever it had not saved is lost.
//!
//! The vendor's supported way to ask one to save and exit is its REST API,
//! on a port it can be told to bind on loopback (`RESTAPIPort`), with basic
//! authentication as `admin` and the server's admin password:
//!
//! 1. `POST /v1/api/save` with `{}`;
//! 2. then `POST /v1/api/shutdown` with `{"waittime": <s>, "message": <m>}`.
//!
//! Any answer but a 2xx, or none, is a stop that did not happen, and the
//! ladder goes on to terminate. A shutdown is never sent after a failed save:
//! that would be a stop that loses the world it was meant to keep.
//!
//! **Not RCON.** Palworld's RCON binds every interface with no setting to
//! keep it on loopback, the vendor has deprecated it, and its `Shutdown`
//! alone did not save.
//!
//! Observed on the real Windows server, v1.0.5 (2026-09-29): both requests
//! answered 200 with an empty body, the process exited about 2.4 s after the
//! shutdown, and the world files were written after the request.
//!
//! The admin password is the host's: a generated `{secret:<name>}` the
//! descriptor also writes into `PalWorldSettings.ini`. The extension names
//! it and the runner resolves it; the extension never sees it.
//!
//! What this half decides: the config and its defaults, the two requests,
//! and how a refusal is explained. The runner's `extensions/palworld.rs`
//! only sends them.

use serde_json::{json, Value};

use super::{ExtensionSpec, Loopback};
use crate::engine::descriptor::{GameDescriptor, StopVia};
use crate::engine::validate::Report;

pub const SPEC: ExtensionSpec = ExtensionSpec {
    name: "palworld",
    supplies: &[],
    validate,
    hosts,
    config_schema,
    stops: true,
    loopback,
};

/// The user Palworld's REST API admits. Fixed by the game.
pub const USER: &str = "admin";

/// Writes the world.
pub const SAVE: &str = "/v1/api/save";

/// Exits after `waittime` seconds, telling players `message`.
pub const SHUTDOWN: &str = "/v1/api/shutdown";

/// The longest `shutdownWait`. It is spent inside the stop's grace.
pub const MAX_WAIT_SECS: u64 = 300;

/// The longest `shutdownMessage`, in characters. Players read it in game.
pub const MAX_MESSAGE: usize = 200;

/// The extension's config, defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The declared private port that carries the REST API.
    pub port: String,
    /// The host secret that is the admin password.
    pub secret: String,
    /// `waittime`: seconds between the shutdown answer and the exit.
    pub wait_secs: u64,
    /// `message`: what players are shown as it goes.
    pub message: String,
}

/// Read the config, filling what it leaves out. `validate` has refused a
/// value of the wrong type; one that slips through reads as the default.
pub fn settings(config: &Value) -> Settings {
    let text = |key: &str, default: &str| {
        config
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    Settings {
        port: text("restPort", "rest"),
        secret: text("adminSecret", "admin"),
        wait_secs: config
            .get("shutdownWait")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        message: text("shutdownMessage", "The server is shutting down."),
    }
}

/// The shutdown's body.
pub fn shutdown_body(settings: &Settings) -> Value {
    json!({ "waittime": settings.wait_secs, "message": settings.message })
}

/// One of the stop's two requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Save,
    Shutdown,
}

/// Why a request that was answered did not count, for the stop's own lines.
pub fn refusal(step: Step, status: u16) -> String {
    let asked = match step {
        Step::Save => "to save the world",
        Step::Shutdown => "to shut down",
    };
    match status {
        401 | 403 => format!(
            "Palworld did not accept the admin password when asked {asked}. Check that its \
             settings file carries the same password."
        ),
        _ => format!("Palworld answered {status} when asked {asked}."),
    }
}

// ─── the descriptor's config ───────────────────────────────────────────────

fn validate(config: &Value, descriptor: &GameDescriptor, report: &mut Report) {
    for key in ["restPort", "adminSecret", "shutdownMessage"] {
        if config.get(key).is_some_and(|v| !v.is_string()) {
            report.problems.push(format!(
                "the Palworld extension's \"{key}\" has to be text."
            ));
        }
    }
    let settings = settings(config);
    for (key, name) in [
        ("restPort", &settings.port),
        ("adminSecret", &settings.secret),
    ] {
        if !is_name(name) {
            report.problems.push(format!(
                "the Palworld extension's \"{key}\" has to be a name alone, and \"{}\" is \
                 not.",
                name.escape_debug()
            ));
        }
    }
    match config.get("shutdownWait") {
        None => {}
        Some(wait) if wait.as_u64().is_some_and(|w| w <= MAX_WAIT_SECS) => {}
        Some(wait) => report.problems.push(format!(
            "the Palworld extension's \"shutdownWait\" has to be a whole number of seconds \
             from 0 to {MAX_WAIT_SECS}, and {wait} is not."
        )),
    }
    if settings.message.chars().count() > MAX_MESSAGE
        || settings.message.chars().any(char::is_control)
    {
        report.problems.push(format!(
            "the Palworld extension's \"shutdownMessage\" has to be one line of at most \
             {MAX_MESSAGE} characters."
        ));
    }
    if descriptor.stop.via != StopVia::Extension {
        report.warnings.push(
            "this game names the Palworld extension, whose only job is its stop, and does \
             not stop through it (stop.via \"extension\")."
                .into(),
        );
    }
    // The wait is spent inside the stop's grace; a wait as long as the grace
    // is a stop that always reaches the terminate.
    let grace_ms = match descriptor.stop.grace_ms {
        0 => 30_000,
        ms => ms,
    };
    if settings.wait_secs.saturating_mul(1000) >= grace_ms {
        report.warnings.push(format!(
            "the Palworld extension asks the server to wait {} s before it exits, which is \
             no shorter than the stop's grace of {} ms, so the stop will reach the \
             terminate.",
            settings.wait_secs, grace_ms
        ));
    }
}

/// A name the descriptor declares elsewhere: a port's or a secret's.
fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Palworld's stop reaches nothing but loopback.
fn hosts(_config: &Value) -> Vec<String> {
    vec![]
}

fn loopback(config: &Value) -> Loopback {
    let settings = settings(config);
    Loopback {
        ports: vec![settings.port],
        secrets: vec![settings.secret],
    }
}

fn config_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "restPort": {
                "type": "string", "default": "rest",
                "description": "The declared TCP port, expose false, that carries Palworld's REST API."
            },
            "adminSecret": {
                "type": "string", "default": "admin",
                "description": "The generated secret that is the admin password, by name. The descriptor also gives it to the game (AdminPassword)."
            },
            "shutdownWait": {
                "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS, "default": 1,
                "description": "waittime: seconds between the shutdown answer and the exit."
            },
            "shutdownMessage": {
                "type": "string", "maxLength": MAX_MESSAGE,
                "default": "The server is shutting down.",
                "description": "What players are shown as the server goes."
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(stop: Value) -> GameDescriptor {
        serde_json::from_value(json!({ "id": "palworld", "stop": stop })).unwrap()
    }

    fn check(config: Value) -> Report {
        let mut report = Report::default();
        validate(
            &config,
            &descriptor(json!({ "via": "extension", "graceMs": 30000 })),
            &mut report,
        );
        report
    }

    #[test]
    fn an_empty_config_reads_as_the_defaults() {
        assert_eq!(
            settings(&json!({})),
            Settings {
                port: "rest".into(),
                secret: "admin".into(),
                wait_secs: 1,
                message: "The server is shutting down.".into(),
            }
        );
        assert!(check(json!({})).ok(), "{:#?}", check(json!({})));
    }

    #[test]
    fn the_shutdown_carries_the_wait_and_the_message() {
        let s = settings(&json!({ "shutdownWait": 5, "shutdownMessage": "Back soon" }));
        assert_eq!(
            shutdown_body(&s),
            json!({ "waittime": 5, "message": "Back soon" })
        );
    }

    /// Everything it may reach is what the config names, and no host at all.
    #[test]
    fn it_reaches_its_port_and_secret_and_nothing_else() {
        let config = json!({ "restPort": "api", "adminSecret": "pw" });
        assert_eq!(
            loopback(&config),
            Loopback {
                ports: vec!["api".into()],
                secrets: vec!["pw".into()],
            }
        );
        assert!(hosts(&config).is_empty());
        const { assert!(SPEC.stops) };
        assert!(SPEC.supplies.is_empty());
    }

    #[test]
    fn a_name_is_a_name_and_never_a_placeholder_or_a_literal() {
        for bad in [
            json!({ "adminSecret": "{secret:admin}" }),
            json!({ "adminSecret": "" }),
            json!({ "restPort": "127.0.0.1:8212" }),
            json!({ "restPort": 8212 }),
        ] {
            let r = check(bad.clone());
            assert!(!r.ok(), "{bad} was accepted");
        }
    }

    #[test]
    fn the_wait_and_the_message_are_bounded() {
        for bad in [
            json!({ "shutdownWait": -1 }),
            json!({ "shutdownWait": MAX_WAIT_SECS + 1 }),
            json!({ "shutdownWait": "1" }),
            json!({ "shutdownMessage": "x".repeat(MAX_MESSAGE + 1) }),
            json!({ "shutdownMessage": "two\nlines" }),
        ] {
            assert!(!check(bad.clone()).ok(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_wait_as_long_as_the_grace_is_warned_about() {
        let r = check(json!({ "shutdownWait": 30 }));
        assert!(r.ok());
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("will reach the terminate")),
            "{:#?}",
            r.warnings
        );
    }

    #[test]
    fn naming_it_without_stopping_through_it_is_warned_about() {
        let mut report = Report::default();
        validate(
            &json!({}),
            &descriptor(json!({ "via": "console", "command": "x" })),
            &mut report,
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("does not stop through it")),
            "{:#?}",
            report.warnings
        );
    }

    #[test]
    fn a_refused_password_is_named_as_such() {
        assert!(refusal(Step::Save, 401).contains("admin password"));
        assert_eq!(
            refusal(Step::Shutdown, 500),
            "Palworld answered 500 when asked to shut down."
        );
    }
}

//! The reference extension, for tests only.
//!
//! Never in a release build and never in [`super::PUBLISHED`]: it exists so
//! the interface is exercised end to end before any real game depends on it,
//! and so a new extension has a small, complete example to copy.
//!
//! Its config names one vendor host, and it supplies a secret `token` (which
//! may only go in `launch.env`) and a plain `profile`. It can stop a server:
//! its config's `stop` names the private port and the host secret its stop
//! uses, which is everything it may reach on loopback. Its readiness probe
//! (`readyPath`) reaches the same.

use serde_json::{json, Value};

use super::{ExtensionSpec, Loopback, Supply};
use crate::engine::descriptor::GameDescriptor;
use crate::engine::validate::Report;

pub const SPEC: ExtensionSpec = ExtensionSpec {
    name: "fixture",
    supplies: &[
        Supply {
            key: "token",
            secret: true,
        },
        Supply {
            key: "profile",
            secret: false,
        },
    ],
    validate,
    hosts,
    config_schema,
    stops: true,
    probes_ready: true,
    loopback,
};

fn validate(config: &Value, _descriptor: &GameDescriptor, report: &mut Report) {
    match config.get("host").and_then(Value::as_str) {
        None => report
            .problems
            .push("the fixture extension needs the vendor's \"host\".".into()),
        Some(host) if host.is_empty() || host.contains(['/', ':', '@']) => report.problems.push(
            format!("the fixture extension's host \"{host}\" is not a bare host name."),
        ),
        Some(_) => {}
    }
}

/// `stop.port` and `stop.secret`, when the config has a stop.
fn loopback(config: &Value) -> Loopback {
    let named = |key: &str| {
        config["stop"][key]
            .as_str()
            .map(|name| vec![name.to_string()])
            .unwrap_or_default()
    };
    Loopback {
        ports: named("port"),
        secrets: named("secret"),
    }
}

fn hosts(config: &Value) -> Vec<String> {
    config
        .get("host")
        .and_then(Value::as_str)
        .map(|h| vec![h.to_string()])
        .unwrap_or_default()
}

fn config_schema() -> Value {
    json!({
        "type": "object",
        "required": ["host"],
        "properties": {
            "host": { "type": "string", "description": "The vendor host, e.g. vendor.example." },
            "stop": { "type": "object", "description": "How the fixture stops a server." }
        }
    })
}

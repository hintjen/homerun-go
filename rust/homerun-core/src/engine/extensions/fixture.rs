//! The reference extension, for tests only.
//!
//! Never in a release build and never in [`super::PUBLISHED`]: it exists so
//! the interface is exercised end to end before any real game depends on it,
//! and so a new extension has a small, complete example to copy.
//!
//! Its config names one vendor host, and it supplies a secret `token` (which
//! may only go in `launch.env`) and a plain `profile`.

use serde_json::{json, Value};

use super::{ExtensionSpec, Supply};
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
            "host": { "type": "string", "description": "The vendor host, e.g. vendor.example." }
        }
    })
}

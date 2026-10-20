//! Palworld's stop and readiness — the pure half.
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
//! **Readiness.** The same API also says when the server is up: while it
//! starts, the runner has the extension ask `GET /v1/api/info` (as `admin`,
//! the same way) every few seconds, and the first 2xx makes the server
//! ready, whether or not the descriptor's `ready.marker` was printed. The
//! marker alone is not enough on Windows: some of the server's startup lines
//! go through a wide-character path and arrive as mojibake or not at all,
//! and a server that never prints its marker stays "Starting" with no tunnel.
//!
//! **Players.** The same API says who is playing: `GET /v1/api/players`,
//! asked on every players sample. The log cannot be trusted for that either.
//! The server's own `[LOG]` lines -- "joined the server", "left the server"
//! -- go through a stdout that a pipe makes block-buffered, so a join reaches
//! the runner only once about 4 KB of later lines have pushed it out: minutes
//! on a quiet server, or never (observed 2026-10-19, v1.0.5). The REST answer
//! also names each player and carries their platform id, which a join line's
//! count did not. The server logs every request it serves, so the runner
//! keeps its own `players` requests out of the log ([`echoes`]).
//!
//! What this half decides: the config and its defaults, the requests, what
//! a players answer means, and how a refusal is explained. The runner's `extensions/palworld.rs` only
//! sends them.

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
    probes_ready: true,
    observes_players: true,
    echoes,
    loopback,
};

/// The user Palworld's REST API admits. Fixed by the game.
pub const USER: &str = "admin";

/// Writes the world.
pub const SAVE: &str = "/v1/api/save";

/// Exits after `waittime` seconds, telling players `message`.
pub const SHUTDOWN: &str = "/v1/api/shutdown";

/// The server's name and version. Any 2xx means its REST API is up, which is
/// the readiness probe's whole question.
pub const INFO: &str = "/v1/api/info";

/// Who is playing, as `{"players": [{"name", "userId", ...}]}`.
pub const PLAYERS: &str = "/v1/api/players";

/// One player the server says is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Present {
    pub name: String,
    /// `steam` or `xbox`, from the `userId` prefix. `None` for a prefix this
    /// has not seen, and then there is no id either.
    pub platform: Option<String>,
    pub platform_id: Option<String>,
}

/// Read a `/v1/api/players` answer. `None` for a body that is not one, so
/// the runner falls back to the log's count rather than reporting nobody.
pub fn players(body: &str) -> Option<Vec<Present>> {
    let value: Value = serde_json::from_str(body).ok()?;
    let list = value.get("players")?.as_array()?;
    Some(
        list.iter()
            .filter_map(|p| {
                let name = p.get("name")?.as_str()?.trim();
                if name.is_empty() {
                    return None;
                }
                let (platform, platform_id) = p
                    .get("userId")
                    .and_then(Value::as_str)
                    .and_then(platform_of)
                    .map_or((None, None), |(platform, id)| {
                        (Some(platform.to_string()), Some(id.to_string()))
                    });
                Some(Present {
                    name: name.to_string(),
                    platform,
                    platform_id,
                })
            })
            .collect(),
    )
}

/// `steam_<SteamID64>` and `gdk_<id>` (the Xbox and Microsoft Store build),
/// as the join lines and the REST answer both write them.
fn platform_of(user_id: &str) -> Option<(&'static str, &str)> {
    let (prefix, id) = user_id.split_once('_')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    match prefix {
        "steam" => Some(("steam", id)),
        "gdk" => Some(("xbox", id)),
        _ => None,
    }
}

/// The line Palworld logs for each `players` request: `[<time>] [LOG] REST
/// accessed endpoint /v1/api/players OK`. One every few seconds would bury
/// everything a person reads the console for. The other requests -- the
/// readiness probe's, the stop's -- are rare, and stay.
pub fn echoes(line: &str) -> bool {
    line.contains("[LOG] REST accessed endpoint ")
        && line
            .split("REST accessed endpoint ")
            .nth(1)
            .is_some_and(|rest| rest.split_whitespace().next() == Some(PLAYERS))
}

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
            "this game names the Palworld extension, whose main job is its stop, and does \
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
        const { assert!(SPEC.stops && SPEC.probes_ready && SPEC.observes_players) };
        assert!(SPEC.supplies.is_empty());
    }

    /// The shape observed on v1.0.5, with made-up values.
    #[test]
    fn a_players_answer_names_each_player_and_their_platform() {
        let body = r#"{"players":[
            {"name":"Craig","accountName":"craig","playerId":"0A1B","userId":"steam_76561198000000001","iP":"127.0.0.1","ping":12.5,"location_x":1.0,"location_y":2.0,"level":3},
            {"name":"Dana","userId":"gdk_2535400000000000","level":1},
            {"name":"Eli","userId":"psn_123"},
            {"name":"  ","userId":"steam_76561198000000009"}
        ]}"#;
        assert_eq!(
            players(body),
            Some(vec![
                Present {
                    name: "Craig".into(),
                    platform: Some("steam".into()),
                    platform_id: Some("76561198000000001".into()),
                },
                Present {
                    name: "Dana".into(),
                    platform: Some("xbox".into()),
                    platform_id: Some("2535400000000000".into()),
                },
                // A platform this has not seen: named, with no id rather
                // than a guessed one.
                Present {
                    name: "Eli".into(),
                    platform: None,
                    platform_id: None,
                },
            ])
        );
        assert_eq!(players(r#"{"players":[]}"#), Some(vec![]));
    }

    /// Not an answer is not "nobody": the runner falls back to the count.
    #[test]
    fn a_body_that_is_not_an_answer_is_none() {
        for body in ["", "Unauthorized", "{}", r#"{"players":{}}"#, "[]"] {
            assert_eq!(players(body), None, "{body:?}");
        }
    }

    #[test]
    fn only_its_own_players_requests_are_hidden_from_the_log() {
        assert!(echoes(
            "[2026-10-19 15:57:31] [LOG] REST accessed endpoint /v1/api/players OK"
        ));
        for kept in [
            "[2026-10-19 15:54:04] [LOG] REST accessed endpoint /v1/api/info OK",
            "[2026-10-19 15:54:04] [LOG] REST accessed endpoint /v1/api/save OK",
            "[2026-10-19 15:55:28] [LOG] Craig joined the server. (User id: steam_1)",
            // Typed in chat: not the server's record of a request.
            "[2026-10-19 15:56:00] [CHAT] <Craig> REST accessed endpoint /v1/api/players OK",
        ] {
            assert!(!echoes(kept), "{kept}");
        }
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

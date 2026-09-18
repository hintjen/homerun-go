//! Readiness, the console, and the stop ladder — all as data.
//!
//! # Readiness is a substring, deliberately
//!
//! Not a regex. Four console defects surfaced in a single PowerNukkitX
//! bring-up — an ANSI stripper eating `[main]`, a bare timestamp, a thread
//! tag, an operator list whose case made `/deop` a no-op — and every one of
//! them was a parser being cleverer than the stream deserved. A substring is
//! the shape a person can check against a real log by eye, and `game verify`
//! re-checks it against the real binary, which is the only check that has
//! ever caught this class of bug.
//!
//! # The console *kind* is a decision; the transport is not
//!
//! This module says "speak WebSocket RCON on the port named `rcon`, with the
//! secret named `rcon`". Opening the socket is the supervisor's job. That
//! split is what keeps this crate free of sockets and its suite free of
//! devices.
//!
//! # A stop ladder always ends somewhere it cannot be ignored
//!
//! The polite rung is what flushes a save, so it gets the long window. Every
//! ladder ends in a kill, because a stop that can be refused is not a stop —
//! and the rung before it exists so that the kill is not what a normal
//! shutdown reaches.

use serde::{Deserialize, Serialize};

use super::descriptor::{ConsoleVia, GameDescriptor, RconProtocol, StopVia};
use crate::{Error, Result};

/// Whether this line means the server is up.
///
/// An empty marker never matches. A descriptor with no marker is refused by
/// [`super::validate`]; this is what happens if one reaches a running system
/// anyway, and "never ready" fails visibly at the start timeout rather than
/// reporting a server that is not up as up.
pub fn is_ready(descriptor: &GameDescriptor, line: &str) -> bool {
    !descriptor.ready.marker.is_empty() && line.contains(&descriptor.ready.marker)
}

/// How long to wait for that line before giving up.
///
/// Zero means the descriptor did not say. Fifteen minutes is the fallback,
/// which is Rust's cold map generation with room to spare — a cold start that
/// is merely slow must not be reported as a failure.
pub fn ready_timeout_ms(descriptor: &GameDescriptor) -> u64 {
    match descriptor.ready.timeout_ms {
        0 => 900_000,
        ms => ms,
    }
}

/// What a line means for the player count, when the count comes from the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Presence {
    pub joined: bool,
    pub left: bool,
}

/// Classify a line against the descriptor's presence markers.
pub fn presence(descriptor: &GameDescriptor, line: &str) -> Presence {
    let Some(markers) = descriptor.observe.presence.as_ref() else {
        return Presence::default();
    };
    Presence {
        joined: !markers.join.is_empty() && line.contains(&markers.join),
        left: !markers.leave.is_empty() && line.contains(&markers.leave),
    }
}

/// How a command reaches the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConsoleKind {
    /// Write the line to the process's stdin.
    Stdin,
    /// Speak RCON to a port this server bound.
    Rcon {
        protocol: RconProtocol,
        /// The *name* of a declared port. The supervisor looks up what was
        /// actually bound — the runner may have moved off the preferred port.
        port: String,
        /// The name of a generated secret.
        secret: String,
    },
    /// There is no console. A server can still be stopped, but nothing can be
    /// asked of it.
    None,
}

/// What this game's console is.
pub fn console(descriptor: &GameDescriptor) -> Result<ConsoleKind> {
    match descriptor.console.via {
        ConsoleVia::Stdin => Ok(ConsoleKind::Stdin),
        ConsoleVia::None => Ok(ConsoleKind::None),
        ConsoleVia::Rcon => {
            let rcon = descriptor.console.rcon.as_ref().ok_or_else(|| {
                Error::Malformed(format!(
                    "{}'s descriptor says its console is RCON but does not say where. \
                     The file is part of the app, so this is a bug in Homerun rather \
                     than something you can fix.",
                    display_name(descriptor)
                ))
            })?;
            Ok(ConsoleKind::Rcon {
                protocol: rcon.protocol,
                port: rcon.port.clone(),
                secret: rcon.secret.clone(),
            })
        }
    }
}

/// One rung of a stop.
///
/// The same shape as `minecraft::jvm::stop_ladder`'s, arrived at
/// independently and kept identical on purpose: the supervisor walks one
/// ladder whether the server is a JVM it has always known about or a game it
/// learned from a descriptor. Separate types because the generic layer must
/// not depend on the Minecraft one — see `minecraft`'s module header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rung {
    /// Flattened, so a rung reads as `{"action":"console","command":"quit",
    /// "waitMs":60000}` rather than nesting an object with its own `action`
    /// key inside an `action` key.
    #[serde(flatten)]
    pub action: Action,
    /// How long to wait for the server to go before the next rung. Zero on
    /// the last one: nothing follows a kill.
    pub wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
    /// Send this verb over whatever [`console`] said.
    Console { command: String },
    /// A console control event — `SIGINT`, or its Windows equivalent.
    Interrupt,
    /// Ask the process to exit. A JVM runs its shutdown hook here; many
    /// games flush a save.
    Terminate,
    /// The rung that cannot be refused.
    Kill,
}

/// How long the polite rung gets when the descriptor does not say.
const DEFAULT_GRACE_MS: u64 = 30_000;

/// How long the process is given to exit after being asked, before the kill.
///
/// Short, because by this point the save has already had its window: this is
/// only the time between "please exit" and "you are exiting".
const TERMINATE_WAIT_MS: u64 = 8_000;

/// The stop sequence for this game.
pub fn stop_ladder(descriptor: &GameDescriptor) -> Vec<Rung> {
    let grace = match descriptor.stop.grace_ms {
        0 => DEFAULT_GRACE_MS,
        ms => ms,
    };
    let mut ladder = Vec::new();

    match descriptor.stop.via {
        StopVia::Console => {
            // A console stop needs both a verb and somewhere to send it. With
            // either missing there is nothing polite to do, and the ladder
            // starts at the rung below rather than pretending.
            let verb = descriptor.stop.command.clone().unwrap_or_default();
            let reachable = !matches!(console(descriptor), Ok(ConsoleKind::None) | Err(_));
            if !verb.is_empty() && reachable {
                ladder.push(Rung {
                    action: Action::Console { command: verb },
                    wait_ms: grace,
                });
            }
        }
        StopVia::Interrupt => ladder.push(Rung {
            action: Action::Interrupt,
            wait_ms: grace,
        }),
    }

    ladder.push(Rung {
        action: Action::Terminate,
        wait_ms: TERMINATE_WAIT_MS,
    });
    ladder.push(Rung {
        action: Action::Kill,
        wait_ms: 0,
    });
    ladder
}

fn display_name(descriptor: &GameDescriptor) -> String {
    if descriptor.name.is_empty() {
        "This game".to_string()
    } else {
        descriptor.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rust() -> GameDescriptor {
        GameDescriptor::parse(include_str!("testdata/rust.json")).unwrap()
    }

    #[test]
    fn the_ready_marker_matches_the_line_it_names_and_nothing_else() {
        let d = rust();
        assert!(is_ready(&d, "12:01:44 Server startup complete"));
        assert!(!is_ready(&d, "12:01:02 Loading world"));
        assert!(!is_ready(&d, ""));
    }

    /// The failure mode this guards: a descriptor with no marker must never
    /// look like a server that is instantly up.
    #[test]
    fn no_marker_never_reports_ready() {
        let d = GameDescriptor::default();
        assert!(!is_ready(&d, "anything at all"));
        assert!(!is_ready(&d, ""));
    }

    #[test]
    fn a_descriptor_that_forgot_its_timeout_still_gets_a_generous_one() {
        assert_eq!(ready_timeout_ms(&rust()), 900_000);
        assert_eq!(ready_timeout_ms(&GameDescriptor::default()), 900_000);
    }

    #[test]
    fn the_pilots_console_is_websocket_rcon_on_a_named_port() {
        assert_eq!(
            console(&rust()).unwrap(),
            ConsoleKind::Rcon {
                protocol: RconProtocol::Webrcon,
                port: "rcon".into(),
                secret: "rcon".into()
            }
        );
    }

    #[test]
    fn a_stdin_console_needs_nothing_else() {
        let d: GameDescriptor =
            serde_json::from_value(json!({ "id": "g", "console": { "via": "stdin" } })).unwrap();
        assert_eq!(console(&d).unwrap(), ConsoleKind::Stdin);
    }

    #[test]
    fn rcon_without_somewhere_to_send_it_is_refused_in_words() {
        let d: GameDescriptor =
            serde_json::from_value(json!({ "id": "g", "name": "G", "console": { "via": "rcon" } }))
                .unwrap();
        let err = console(&d).unwrap_err().to_string();
        assert!(err.contains("does not say where"), "{err}");
    }

    #[test]
    fn the_pilots_ladder_asks_before_it_insists() {
        let ladder = stop_ladder(&rust());
        assert_eq!(
            ladder,
            vec![
                Rung {
                    action: Action::Console {
                        command: "quit".into()
                    },
                    wait_ms: 60_000
                },
                Rung {
                    action: Action::Terminate,
                    wait_ms: 8_000
                },
                Rung {
                    action: Action::Kill,
                    wait_ms: 0
                },
            ]
        );
    }

    #[test]
    fn an_interrupt_game_starts_at_the_interrupt() {
        let d: GameDescriptor = serde_json::from_value(
            json!({ "id": "g", "stop": { "via": "interrupt", "graceMs": 5000 } }),
        )
        .unwrap();
        assert_eq!(stop_ladder(&d)[0].action, Action::Interrupt);
    }

    /// With nothing to say and nowhere to say it, the ladder does not invent
    /// a polite rung that would only ever time out.
    #[test]
    fn a_console_stop_with_no_console_starts_lower() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "console": { "via": "none" },
            "stop": { "via": "console", "command": "quit" }
        }))
        .unwrap();
        assert_eq!(
            stop_ladder(&d)
                .iter()
                .map(|r| r.action.clone())
                .collect::<Vec<_>>(),
            vec![Action::Terminate, Action::Kill]
        );
    }

    #[test]
    fn a_console_stop_with_no_verb_starts_lower_too() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "console": { "via": "stdin" }, "stop": { "via": "console" }
        }))
        .unwrap();
        assert_eq!(stop_ladder(&d)[0].action, Action::Terminate);
    }

    #[test]
    fn every_ladder_ends_somewhere_it_cannot_be_ignored() {
        for descriptor in [
            rust(),
            GameDescriptor::default(),
            serde_json::from_value(json!({ "id": "g", "stop": { "via": "interrupt" } })).unwrap(),
        ] {
            let ladder = stop_ladder(&descriptor);
            assert_eq!(ladder.last().unwrap().action, Action::Kill);
            assert_eq!(
                ladder.last().unwrap().wait_ms,
                0,
                "nothing waits after a kill"
            );
            assert!(ladder.len() >= 2, "a kill must never be the first rung");
        }
    }

    // ─── the wire shapes a host reads ──────────────────────────────────────
    //
    // Both of these were wrong first time and both compiled: a rung nested an
    // object with its own `action` key inside an `action` key, and nothing in
    // the unit tests above noticed, because they compare Rust values. The
    // bridge's tests caught it. These pin the JSON so the next change does not
    // have to.

    #[test]
    fn a_rung_is_one_flat_object_a_host_can_read_keys_off() {
        let ladder = serde_json::to_value(stop_ladder(&rust())).unwrap();
        let first = &ladder[0];
        assert_eq!(first["action"], "console", "not a nested object: {first}");
        assert_eq!(first["command"], "quit");
        assert_eq!(first["waitMs"], 60_000);

        let last = &ladder[ladder.as_array().unwrap().len() - 1];
        assert_eq!(last["action"], "kill");
        assert_eq!(last["waitMs"], 0);
    }

    #[test]
    fn an_interrupt_rung_carries_no_command() {
        let d: GameDescriptor =
            serde_json::from_value(json!({ "id": "g", "stop": { "via": "interrupt" } })).unwrap();
        let ladder = serde_json::to_value(stop_ladder(&d)).unwrap();
        assert_eq!(ladder[0]["action"], "interrupt");
        assert!(ladder[0].get("command").is_none(), "{}", ladder[0]);
    }

    #[test]
    fn a_console_kind_names_itself_and_where_to_reach_it() {
        let console = serde_json::to_value(console(&rust()).unwrap()).unwrap();
        assert_eq!(console["kind"], "rcon");
        assert_eq!(console["protocol"], "webrcon");
        assert_eq!(console["port"], "rcon");
        assert_eq!(console["secret"], "rcon");

        let stdin = serde_json::to_value(ConsoleKind::Stdin).unwrap();
        assert_eq!(stdin["kind"], "stdin");
    }

    #[test]
    fn a_game_with_no_presence_markers_reads_nothing_into_its_log() {
        assert_eq!(presence(&rust(), "Player joined"), Presence::default());
    }

    #[test]
    fn presence_markers_classify_the_lines_they_name() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g",
            "observe": { "players": "log-regex",
                         "presence": { "join": "has entered", "leave": "has left" } }
        }))
        .unwrap();
        assert!(presence(&d, "Notch has entered the game").joined);
        assert!(presence(&d, "Notch has left the game").left);
        assert_eq!(presence(&d, "Notch said hello"), Presence::default());
    }
}

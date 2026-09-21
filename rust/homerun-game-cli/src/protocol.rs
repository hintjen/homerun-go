//! The `supervise` protocol â€” newline-delimited JSON, commands in, events out.
//!
//! # This file is a contract, not a convenience
//!
//! It is `plans/multi-game-contracts.md` Â§ 2 in the `homerun` monorepo,
//! transcribed. Three codebases build against these strings at the same time
//! â€” this runner, the Electron client, and the API session that owns the
//! document â€” and a rename here compiles cleanly and fails at runtime on
//! somebody else's machine. **Change the document first, tell the other two
//! owners, and then change this.**
//!
//! The tests at the bottom pin every command and event against literal JSON
//! for that reason. They are not testing serde; they are testing that nobody
//! tidied a field name.
//!
//! # Why stdio and not a socket
//!
//! Electron spawns the runner as a child with piped stdio. There is no named
//! pipe and no socket, which closes the unauthenticated-pipe hole for new
//! games *by construction* rather than by getting authentication right:
//! there is nothing for anything else on the machine to connect to.
//!
//! **stdin EOF is a graceful shutdown**, identical to `shutdown`. If Electron
//! dies, the pipe closes, and the runner takes the server down with it rather
//! than leaving an orphan holding the world file.
//!
//! # Both sides may be newer than the other
//!
//! An unknown command is ignored with a note on stderr, and unknown fields
//! are ignored everywhere. That is what lets the desktop ship a version that
//! speaks a command this runner has never heard of, and the reverse.
//!
//! stderr is free-form diagnostics. Electron logs it and never parses it, so
//! nothing on stderr is part of this contract.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The protocol version this runner speaks.
pub const PROTOCOL: u32 = 1;

/// What Electron sends.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Command {
    Hello {
        #[serde(default)]
        protocol: u32,
    },
    #[serde(rename_all = "camelCase")]
    Fetch {
        server_id: String,
        descriptor: Value,
        runtime_root: String,
        /// Absent is `false`, and that is the one default in this file that
        /// must never be the permissive one. The runner never infers
        /// acceptance â€” see `homerun_core::engine::licence`.
        #[serde(default)]
        licence_accepted: bool,
    },
    #[serde(rename_all = "camelCase")]
    Start {
        server_id: String,
        descriptor: Value,
        server_dir: String,
        runtime_root: String,
        #[serde(default)]
        server_name: String,
        /// The API's strings. Coerced by `engine::settings`, which is where
        /// `""` stops meaning an empty value and starts meaning unset.
        #[serde(default)]
        settings: Map<String, Value>,
        /// Generated per server by the host, stored by it, never sent to the
        /// API. `engine.secrets` says which ones a descriptor needs.
        #[serde(default)]
        secrets: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        bind_address: Option<String>,
        #[serde(default)]
        licence_accepted: bool,
    },
    #[serde(rename_all = "camelCase")]
    StartTunnel {
        server_id: String,
        bin_path: String,
        conf_path: String,
    },
    #[serde(rename_all = "camelCase")]
    Console {
        server_id: String,
        command: String,
        #[serde(default)]
        req_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Stop {
        server_id: String,
    },
    Status,
    Shutdown,
    /// A command added after this runner shipped.
    ///
    /// Not an error: the desktop and the runner are versioned separately and
    /// either may be newer. The caller notes it on stderr and carries on.
    #[serde(other)]
    Unknown,
}

/// What the runner sends.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    #[serde(rename_all = "camelCase")]
    Ready {
        protocol: u32,
        version: String,
        /// The first twelve characters of this executable's own sha256 â€” the
        /// same build id its published manifest carries, so a desktop can say
        /// which runner it is talking to.
        build: String,
    },
    #[serde(rename_all = "camelCase")]
    FetchProgress {
        server_id: String,
        /// `steamcmd` | `download` | `verify` | `extract`.
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        received: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    FetchComplete {
        server_id: String,
        runtime_dir: String,
        build_id: String,
    },
    #[serde(rename_all = "camelCase")]
    ServerLog {
        server_id: String,
        line: String,
        /// `stdout` | `stderr` | `host`. `host` is the runner talking, and it
        /// is how a player sees "downloading" rather than a silent minute.
        stream: String,
    },
    #[serde(rename_all = "camelCase")]
    ServerPorts {
        server_id: String,
        /// Keyed by the descriptor's port *name*, valued with what was
        /// actually bound. Emitted once the process has bound and always
        /// before `server-started`.
        ports: std::collections::BTreeMap<String, u16>,
    },
    #[serde(rename_all = "camelCase")]
    ServerStarted {
        server_id: String,
    },
    #[serde(rename_all = "camelCase")]
    TunnelStarted {
        server_id: String,
    },
    #[serde(rename_all = "camelCase")]
    TunnelFailed {
        server_id: String,
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    Players {
        server_id: String,
        count: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
        players: Vec<Player>,
    },
    #[serde(rename_all = "camelCase")]
    Stats {
        server_id: String,
        /// Counters, never rates. `homerun_core::metrics` turns two of these
        /// into a percentage; doing it here would put that arithmetic back
        /// where this codebase spent a day removing it from.
        rss_kb: u64,
        cpu_seconds: f64,
    },
    #[serde(rename_all = "camelCase")]
    ConsoleResponse {
        server_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        req_id: Option<String>,
        response: String,
    },
    #[serde(rename_all = "camelCase")]
    ServerStopped {
        server_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
    #[serde(rename_all = "camelCase")]
    ServerCrashed {
        server_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        /// The last lines of console output, so a player is told what
        /// happened without being asked to find a log file.
        tail: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    Status {
        servers: Vec<ServerStatus>,
    },
    #[serde(rename_all = "camelCase")]
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        server_id: Option<String>,
        code: String,
        /// **A sentence a player can read.** Never a diagnostic: no `errno`,
        /// no `unwrap`, no `panicked at`. The `homerun-go` rule.
        message: String,
    },
    ShutdownComplete,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub server_id: String,
    /// `starting` | `running` | `stopping` | `stopped` | `crashed`.
    pub state: String,
}

/// Every code the contract names, spelled once.
///
/// Constants rather than an enum because they cross to JavaScript as strings
/// and a typo in one of them is a branch the desktop never takes.
pub mod codes {
    pub const LICENCE_NOT_ACCEPTED: &str = "licence_not_accepted";
    pub const DESCRIPTOR_INVALID: &str = "descriptor_invalid";
    pub const REQUIRES_UNMET: &str = "requires_unmet";
    pub const PORT_UNAVAILABLE: &str = "port_unavailable";
    pub const FETCH_FAILED: &str = "fetch_failed";
    pub const SPAWN_FAILED: &str = "spawn_failed";
    pub const READY_TIMEOUT: &str = "ready_timeout";
    pub const BUSY: &str = "busy";
    /// The server bound a port the descriptor said would stay on this
    /// computer to an address other computers can reach, and the runner
    /// stopped it.
    ///
    /// **This is protocol v1's ninth code and an addition to the contract.**
    /// It is not `descriptor_invalid`: the descriptor may be perfectly good
    /// and the game may simply have ignored the address it was given, and a
    /// desktop that reads `descriptor_invalid` as "this file is broken, do
    /// not retry" would say the wrong thing. It is not `spawn_failed`
    /// either — the server started, which is how the port came to be
    /// observed at all.
    ///
    /// A desktop built before this code existed shows the message, which is
    /// written for a player and says what happened.
    pub const PORT_EXPOSED: &str = "port_exposed";
    pub const PORT_INSPECTION_FAILED: &str = "port_inspection_failed";

    /// Every one of them, for the test that keeps this list and the
    /// document's in step.
    #[cfg(test)]
    pub const ALL: [&str; 10] = [
        LICENCE_NOT_ACCEPTED,
        DESCRIPTOR_INVALID,
        REQUIRES_UNMET,
        PORT_UNAVAILABLE,
        FETCH_FAILED,
        SPAWN_FAILED,
        READY_TIMEOUT,
        BUSY,
        PORT_EXPOSED,
        PORT_INSPECTION_FAILED,
    ];
}

impl Event {
    /// One line of NDJSON, newline included.
    ///
    /// Serialising an event cannot fail â€” every field is a plain type â€” but
    /// if it somehow did, a runner that printed nothing would hang a UI
    /// promise for ever, which is the worst failure in this protocol. So the
    /// fallback is an error envelope rather than a lost line.
    pub fn line(&self) -> String {
        match serde_json::to_string(self) {
            Ok(json) => format!("{json}\n"),
            Err(_) => String::from(
                "{\"event\":\"error\",\"code\":\"spawn_failed\",\"message\":\
                 \"Homerun could not describe what just happened.\"}\n",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Command {
        serde_json::from_str(line).expect("a documented command must parse")
    }

    // â”€â”€â”€ commands, exactly as the document writes them â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn every_documented_command_parses() {
        assert_eq!(
            parse(r#"{"cmd":"hello","protocol":1}"#),
            Command::Hello { protocol: 1 }
        );

        assert_eq!(
            parse(
                r#"{"cmd":"fetch","serverId":"s1","descriptor":{"id":"rust"},
                    "runtimeRoot":"C:\\rt","licenceAccepted":true}"#
            ),
            Command::Fetch {
                server_id: "s1".into(),
                descriptor: serde_json::json!({ "id": "rust" }),
                runtime_root: "C:\\rt".into(),
                licence_accepted: true,
            }
        );

        let start = parse(
            r#"{"cmd":"start","serverId":"s1","descriptor":{"id":"rust"},
                "serverDir":"C:\\servers\\s1","runtimeRoot":"C:\\rt",
                "serverName":"Keep","settings":{"maxPlayers":"10"},
                "secrets":{"rcon":"x"},"bindAddress":"127.0.0.1",
                "licenceAccepted":true}"#,
        );
        match start {
            Command::Start {
                server_id,
                server_dir,
                server_name,
                settings,
                secrets,
                bind_address,
                licence_accepted,
                ..
            } => {
                assert_eq!(server_id, "s1");
                assert_eq!(server_dir, "C:\\servers\\s1");
                assert_eq!(server_name, "Keep");
                assert_eq!(settings["maxPlayers"], "10");
                assert_eq!(secrets["rcon"], "x");
                assert_eq!(bind_address.as_deref(), Some("127.0.0.1"));
                assert!(licence_accepted);
            }
            other => panic!("{other:?}"),
        }

        assert_eq!(
            parse(
                r#"{"cmd":"start-tunnel","serverId":"s1","binPath":"w.exe","confPath":"w.conf"}"#
            ),
            Command::StartTunnel {
                server_id: "s1".into(),
                bin_path: "w.exe".into(),
                conf_path: "w.conf".into(),
            }
        );

        assert_eq!(
            parse(r#"{"cmd":"console","serverId":"s1","command":"status","reqId":"abc"}"#),
            Command::Console {
                server_id: "s1".into(),
                command: "status".into(),
                req_id: Some("abc".into()),
            }
        );

        assert_eq!(
            parse(r#"{"cmd":"stop","serverId":"s1"}"#),
            Command::Stop {
                server_id: "s1".into()
            }
        );
        assert_eq!(parse(r#"{"cmd":"status"}"#), Command::Status);
        assert_eq!(parse(r#"{"cmd":"shutdown"}"#), Command::Shutdown);
    }

    /// The default that must never be the permissive one.
    #[test]
    fn a_fetch_that_does_not_mention_the_licence_has_not_accepted_it() {
        let Command::Fetch {
            licence_accepted, ..
        } = parse(r#"{"cmd":"fetch","serverId":"s1","descriptor":{},"runtimeRoot":"/rt"}"#)
        else {
            panic!("expected a fetch");
        };
        assert!(!licence_accepted, "absent must mean not accepted");

        let Command::Start {
            licence_accepted, ..
        } = parse(
            r#"{"cmd":"start","serverId":"s1","descriptor":{},"serverDir":"/s",
                "runtimeRoot":"/rt"}"#,
        )
        else {
            panic!("expected a start");
        };
        assert!(!licence_accepted, "absent must mean not accepted");
    }

    /// Either side may be newer. Neither may fall over because of it.
    #[test]
    fn a_command_from_a_newer_desktop_is_unknown_rather_than_fatal() {
        assert_eq!(
            parse(r#"{"cmd":"teleport","serverId":"s1"}"#),
            Command::Unknown
        );
    }

    #[test]
    fn unknown_fields_are_ignored_everywhere() {
        assert_eq!(
            parse(r#"{"cmd":"stop","serverId":"s1","force":true,"why":"because"}"#),
            Command::Stop {
                server_id: "s1".into()
            }
        );
    }

    // â”€â”€â”€ events, exactly as the document writes them â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    fn rendered(event: Event) -> Value {
        let line = event.line();
        assert!(line.ends_with('\n'), "NDJSON: one object per line");
        assert_eq!(line.matches('\n').count(), 1, "no embedded newlines");
        serde_json::from_str(&line).expect("an event is always JSON")
    }

    #[test]
    fn every_documented_event_renders_with_the_names_the_desktop_reads() {
        let ready = rendered(Event::Ready {
            protocol: PROTOCOL,
            version: "0.1.0".into(),
            build: "abc123def456".into(),
        });
        assert_eq!(ready["event"], "ready");
        assert_eq!(ready["protocol"], 1);
        assert_eq!(ready["build"], "abc123def456");

        let progress = rendered(Event::FetchProgress {
            server_id: "s1".into(),
            phase: "download".into(),
            received: Some(123),
            total: Some(456),
            message: None,
        });
        assert_eq!(progress["event"], "fetch-progress");
        assert_eq!(progress["serverId"], "s1");
        assert_eq!(progress["received"], 123);
        assert!(progress.get("message").is_none(), "absent, not null");

        let complete = rendered(Event::FetchComplete {
            server_id: "s1".into(),
            runtime_dir: "C:\\rt\\rust".into(),
            build_id: "abc123".into(),
        });
        assert_eq!(complete["event"], "fetch-complete");
        assert_eq!(complete["runtimeDir"], "C:\\rt\\rust");
        assert_eq!(complete["buildId"], "abc123");

        let log = rendered(Event::ServerLog {
            server_id: "s1".into(),
            line: "hello".into(),
            stream: "stdout".into(),
        });
        assert_eq!(log["event"], "server-log");
        assert_eq!(log["stream"], "stdout");

        let ports = rendered(Event::ServerPorts {
            server_id: "s1".into(),
            ports: [("game".to_string(), 28015u16)].into_iter().collect(),
        });
        assert_eq!(ports["event"], "server-ports");
        assert_eq!(ports["ports"]["game"], 28015);

        assert_eq!(
            rendered(Event::ServerStarted {
                server_id: "s1".into()
            })["event"],
            "server-started"
        );
        assert_eq!(
            rendered(Event::TunnelStarted {
                server_id: "s1".into()
            })["event"],
            "tunnel-started"
        );
        assert_eq!(
            rendered(Event::TunnelFailed {
                server_id: "s1".into(),
                message: "no".into()
            })["event"],
            "tunnel-failed"
        );

        let players = rendered(Event::Players {
            server_id: "s1".into(),
            count: 1,
            max: Some(10),
            players: vec![Player {
                name: "Craig".into(),
                platform_id: Some("7656".into()),
                platform: Some("steam".into()),
            }],
        });
        assert_eq!(players["event"], "players");
        assert_eq!(players["players"][0]["platformId"], "7656");

        let stats = rendered(Event::Stats {
            server_id: "s1".into(),
            rss_kb: 123_456,
            cpu_seconds: 12.5,
        });
        assert_eq!(stats["event"], "stats");
        assert_eq!(stats["rssKb"], 123_456);
        assert_eq!(stats["cpuSeconds"], 12.5);

        let response = rendered(Event::ConsoleResponse {
            server_id: "s1".into(),
            req_id: Some("abc".into()),
            response: "ok".into(),
        });
        assert_eq!(response["event"], "console-response");
        assert_eq!(response["reqId"], "abc");

        assert_eq!(
            rendered(Event::ServerStopped {
                server_id: "s1".into(),
                code: Some(0)
            })["event"],
            "server-stopped"
        );

        let crashed = rendered(Event::ServerCrashed {
            server_id: "s1".into(),
            code: Some(1),
            tail: vec!["last".into(), "lines".into()],
        });
        assert_eq!(crashed["event"], "server-crashed");
        assert_eq!(crashed["tail"][1], "lines");

        let status = rendered(Event::Status {
            servers: vec![ServerStatus {
                server_id: "s1".into(),
                state: "running".into(),
            }],
        });
        assert_eq!(status["event"], "status");
        assert_eq!(status["servers"][0]["serverId"], "s1");

        let error = rendered(Event::Error {
            server_id: Some("s1".into()),
            code: codes::BUSY.into(),
            message: "This computer is already running a server.".into(),
        });
        assert_eq!(error["event"], "error");
        assert_eq!(error["code"], "busy");

        assert_eq!(
            rendered(Event::ShutdownComplete)["event"],
            "shutdown-complete"
        );
    }

    /// Every event about a server carries its id, so a desktop running one
    /// runner per server never has to infer which is which.
    #[test]
    fn every_event_about_a_server_names_it() {
        let about_a_server = [
            Event::FetchProgress {
                server_id: "s1".into(),
                phase: "verify".into(),
                received: None,
                total: None,
                message: None,
            },
            Event::ServerLog {
                server_id: "s1".into(),
                line: String::new(),
                stream: "host".into(),
            },
            Event::ServerStarted {
                server_id: "s1".into(),
            },
            Event::ServerStopped {
                server_id: "s1".into(),
                code: None,
            },
            Event::ServerCrashed {
                server_id: "s1".into(),
                code: None,
                tail: vec![],
            },
        ];
        for event in about_a_server {
            let json = rendered(event);
            assert_eq!(json["serverId"], "s1", "{json}");
        }
    }

    #[test]
    fn the_error_codes_are_the_ones_the_contract_names() {
        assert_eq!(
            codes::ALL.to_vec(),
            vec![
                "licence_not_accepted",
                "descriptor_invalid",
                "requires_unmet",
                "port_unavailable",
                "fetch_failed",
                "spawn_failed",
                "ready_timeout",
                "busy",
                "port_exposed",
                "port_inspection_failed",
            ]
        );
    }

    /// A log line containing a newline would be two NDJSON lines, and the
    /// second would be unparseable. serde escapes it; this is the test that
    /// says so out loud, because a server printing `\r\n` is routine.
    #[test]
    fn a_console_line_with_a_newline_in_it_stays_one_line() {
        let line = Event::ServerLog {
            server_id: "s1".into(),
            line: "first\nsecond\r\nthird".into(),
            stream: "stdout".into(),
        }
        .line();
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        let back: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["line"], "first\nsecond\r\nthird");
    }
}

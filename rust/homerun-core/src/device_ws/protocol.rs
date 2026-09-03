//! The frames the dashboard and a device exchange, and the order they are
//! allowed in.
//!
//! Reference: `deviceWebsocket/handlers.ts` in the `homerun` repo. Every shape
//! here is what the dashboard already parses, so a difference is a bug by
//! definition — the same standard [`crate::tunnel`] holds itself to.
//!
//! # Nothing here does any I/O
//!
//! This decides what a frame *means* and what carrying it out would require.
//! Reading the socket, verifying the token against the JWKS, asking the API who
//! the caller may touch, and writing the reply are all effects, and they live
//! in whatever is driving this.
//!
//! # Authentication is not authorisation
//!
//! The token proves *who*. Whether they may touch a given server is a question
//! only the API can answer, because membership lives there and nowhere else —
//! so a request carries a [`Scope`] and the driver resolves it against the
//! **caller's own** token. Two different questions, two different answerers,
//! and conflating them is how a device would start deciding who owns a world.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Close code for every authentication failure, matching the desktop.
///
/// A single code on purpose: the dashboard distinguishes "you may not be here"
/// from a transport failure, and does not need to know *which* way the client
/// got it wrong. The reasons that separate them are in the `error` frame sent
/// first.
pub const CLOSE_AUTH_FAILED: u16 = 4001;

/// How long a socket may stay silent before it has to have authenticated.
///
/// Short because there is nothing to think about: the client sends its token as
/// its first act or it has no business here. Anything longer is a socket held
/// open by whoever is scanning the gateway's port.
pub const AUTH_TIMEOUT_MS: u64 = 5_000;

/// Ping cadence. A peer that misses one round is terminated.
///
/// Clients reach a device through a tunnel, where a peer that vanishes without
/// a FIN — a laptop sleeping, a train entering a tunnel — leaves a socket that
/// stays open and buffers every send for ever. On a phone that is memory
/// nobody can reclaim, so this is not the optional politeness it looks like.
pub const HEARTBEAT_INTERVAL_MS: u64 = 30_000;

/// Terminate a peer whose write backlog reaches this.
///
/// The other half of the same problem: a peer that is still *connected* but has
/// stopped draining is indistinguishable from a slow one until you cap it. A
/// console can produce lines faster than a stalled socket accepts them.
pub const MAX_BUFFERED_BYTES: usize = 4 * 1024 * 1024;

/// A well-formed request from the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The first frame on every socket. Carries a Keycloak access token.
    Auth {
        token: String,
    },
    SubscribeLogs {
        server_id: String,
    },
    UnsubscribeLogs {
        server_id: String,
    },
    Rcon {
        server_id: String,
        command: String,
    },
    /// This device's own logs, for remote support.
    GetAppLogs,
}

/// What must be true before a request may be carried out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Nothing beyond being authenticated.
    None,
    /// The caller must be a member of this server.
    Server(String),
    /// The caller must be a member of this device.
    Device,
}

/// Why a frame will not be acted on.
///
/// These reach a dashboard developer's console, not a Minecraft player, so
/// they name the problem plainly rather than softening it — the opposite of
/// [`crate::minecraft::jvm::Refusal`], and worth the contrast being deliberate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not JSON at all.
    NotJson,
    /// A frame arrived before the socket authenticated. Fatal to the socket.
    Unauthenticated,
    /// Authenticated, but the type is not one we serve.
    UnknownType(String),
    /// The right type, missing something it cannot work without.
    Missing { frame: String, field: String },
}

impl Refusal {
    /// The sentence to put in an `error` frame.
    pub fn message(&self) -> String {
        match self {
            Refusal::NotJson => "Invalid JSON".to_string(),
            Refusal::Unauthenticated => "Not authenticated".to_string(),
            Refusal::UnknownType(kind) => format!("Unknown message type: {kind}"),
            Refusal::Missing { frame, field } => format!("{frame} requires {field}"),
        }
    }

    /// Whether the socket should close after the error is sent.
    ///
    /// Only the auth failures are fatal. A dashboard that sends one malformed
    /// frame — a new client against an old device, most likely — should be told
    /// and allowed to carry on, because dropping the socket takes the console
    /// stream with it.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Refusal::Unauthenticated)
    }
}

/// The authentication half of a connection's state.
///
/// Deliberately tiny and deliberately not holding the token: the driver keeps
/// that, because it needs it to ask the API questions and this module must not
/// be able to leak it into a log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    authenticated: bool,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Record that the driver verified the token.
    ///
    /// Separate from [`Session::read`] because verification is an effect —
    /// fetching a JWKS over the network — and this module does none.
    pub fn authenticated(&mut self) {
        self.authenticated = true;
    }

    /// Interpret one text frame in this session's current state.
    ///
    /// The auth-first rule is enforced here rather than by the driver: until
    /// the socket has authenticated, `auth` is the *only* acceptable frame and
    /// anything else closes the connection. A driver that got this wrong would
    /// serve a console to an unauthenticated peer, which is the one failure in
    /// this protocol worth being categorical about.
    pub fn read(&self, raw: &str) -> Result<Request, Refusal> {
        let value: Value = serde_json::from_str(raw).map_err(|_| Refusal::NotJson)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let text = |field: &str| -> Result<String, Refusal> {
            value
                .get(field)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| Refusal::Missing {
                    frame: kind.to_string(),
                    field: field.to_string(),
                })
        };

        if !self.authenticated {
            // Not `UnknownType`: an unauthenticated peer learns only that it
            // has not authenticated, whatever it asked for.
            if kind != "auth" {
                return Err(Refusal::Unauthenticated);
            }
            return Ok(Request::Auth {
                token: text("token")?,
            });
        }

        match kind {
            // Re-authenticating is not an error, and not a way to change who
            // you are either — the driver verified a token once and cached the
            // membership it implies. Accepting it keeps a reconnecting client
            // simple; acting on it would let a socket escalate itself.
            "auth" => Ok(Request::Auth {
                token: text("token")?,
            }),
            "subscribe-logs" => Ok(Request::SubscribeLogs {
                server_id: text("serverId")?,
            }),
            "unsubscribe-logs" => Ok(Request::UnsubscribeLogs {
                server_id: text("serverId")?,
            }),
            "rcon" => Ok(Request::Rcon {
                server_id: text("serverId")?,
                command: text("command")?,
            }),
            "get-app-logs" => Ok(Request::GetAppLogs),
            other => Err(Refusal::UnknownType(other.to_string())),
        }
    }
}

impl Request {
    /// What the driver must establish before carrying this out.
    pub fn scope(&self) -> Scope {
        match self {
            Request::Auth { .. } => Scope::None,
            Request::SubscribeLogs { server_id }
            | Request::UnsubscribeLogs { server_id }
            | Request::Rcon { server_id, .. } => Scope::Server(server_id.clone()),
            Request::GetAppLogs => Scope::Device,
        }
    }
}

/// The frames a device sends.
///
/// Free functions rather than a type, because there is nothing to hold: each
/// one is a shape the dashboard already parses, and the only thing worth
/// centralising is that the shape is written once.
pub mod outgoing {
    use super::*;
    use crate::reporting::{app_error::redact, scrub};

    pub fn auth_ok() -> Value {
        json!({ "type": "auth-ok" })
    }

    pub fn error(message: impl AsRef<str>) -> Value {
        json!({ "type": "error", "message": message.as_ref() })
    }

    /// The refusal a caller gets for a server they are not a member of.
    ///
    /// Identical to the one for a server that does not exist, and deliberately
    /// so: telling an outsider which ids are real is a membership oracle.
    pub fn not_authorized_for_server() -> Value {
        error("Not authorized for this server")
    }

    pub fn not_authorized_for_device() -> Value {
        error("Not authorized for this device")
    }

    /// The check itself failed — no signal, or the API rejected the token.
    ///
    /// Refused rather than allowed. Failing open here would serve a console to
    /// anyone whenever the API is unreachable, which is exactly when nobody is
    /// watching.
    pub fn authorization_unavailable() -> Value {
        error("Authorization check failed")
    }

    /// `timestamp` is the caller's: this crate has no clock, on purpose.
    ///
    /// The line is scrubbed *here*, as the frame is built, and not by whoever
    /// drains the console. A join line carries a player's address and a chat
    /// line whatever they typed, and this frame is on its way through the
    /// tunnel to a browser. The rule is the crash report's — addresses and
    /// chat go, names stay — made in [`crate::reporting::scrub`] and applied
    /// in the one place every driver has to pass through, so none of them can
    /// send a raw line by forgetting a step.
    pub fn log(server_id: &str, line: &str, timestamp: &str) -> Value {
        json!({
            "type": "log",
            "serverId": server_id,
            "line": scrub::console_line(line),
            "timestamp": timestamp,
        })
    }

    /// Everything the console holds, sent once on subscribe. Scrubbed line by
    /// line for the reason [`log`] gives.
    ///
    /// Without it a dashboard opened mid-session shows an empty console for a
    /// server that has been talking for an hour.
    pub fn log_history(server_id: &str, lines: &[String]) -> Value {
        json!({
            "type": "log-history",
            "serverId": server_id,
            "lines": scrub::console_lines(lines),
        })
    }

    pub fn rcon_response(server_id: &str, response: &str, success: bool) -> Value {
        json!({
            "type": "rcon-response",
            "serverId": server_id,
            "response": response,
            "success": success,
        })
    }

    pub fn server_status(server_id: &str, online: bool) -> Value {
        json!({ "type": "server-status", "serverId": server_id, "online": online })
    }

    /// This device's own log, for a support request.
    ///
    /// Redacted as it is built, with the same scanner an error report uses:
    /// the app's log quotes URLs, and through them OAuth codes; request
    /// headers, and through them bearer tokens; and whatever the page
    /// printed, which has included an email address. The crash report already
    /// runs this over the very same logcat text before uploading it
    /// ([`crate::reporting::crash`]); a support request reads the same text
    /// and used to get it raw. Names and UUIDs survive, as everywhere else.
    pub fn app_logs(main_log: &str, renderer_log: &str) -> Value {
        json!({
            "type": "app-logs",
            "mainLog": redact::text(main_log),
            "rendererLog": redact::text(renderer_log),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authed() -> Session {
        let mut session = Session::new();
        session.authenticated();
        session
    }

    #[test]
    fn nothing_but_auth_is_accepted_before_authenticating() {
        let session = Session::new();
        for frame in [
            r#"{"type":"subscribe-logs","serverId":"s1"}"#,
            r#"{"type":"rcon","serverId":"s1","command":"list"}"#,
            r#"{"type":"get-app-logs"}"#,
            r#"{"type":"whatever"}"#,
        ] {
            assert_eq!(
                session.read(frame),
                Err(Refusal::Unauthenticated),
                "an unauthenticated socket must be told only that, whatever it asked: {frame}"
            );
        }
    }

    #[test]
    fn only_an_auth_failure_closes_the_socket() {
        assert!(Refusal::Unauthenticated.is_fatal());
        // A malformed frame is most likely a newer dashboard against an older
        // device. Closing would take the console stream with it.
        assert!(!Refusal::NotJson.is_fatal());
        assert!(!Refusal::UnknownType("nope".into()).is_fatal());
        assert!(!Refusal::Missing {
            frame: "rcon".into(),
            field: "command".into()
        }
        .is_fatal());
    }

    #[test]
    fn an_auth_frame_with_no_token_is_refused_rather_than_accepted_blank() {
        let session = Session::new();
        assert_eq!(
            session.read(r#"{"type":"auth"}"#),
            Err(Refusal::Missing {
                frame: "auth".into(),
                field: "token".into()
            })
        );
        assert_eq!(
            session.read(r#"{"type":"auth","token":""}"#),
            Err(Refusal::Missing {
                frame: "auth".into(),
                field: "token".into()
            }),
            "a blank token must not reach the verifier as if it were one"
        );
    }

    #[test]
    fn every_server_frame_carries_the_id_it_must_be_authorized_for() {
        let session = authed();
        let cases = [
            (
                r#"{"type":"subscribe-logs","serverId":"s1"}"#,
                Request::SubscribeLogs {
                    server_id: "s1".into(),
                },
            ),
            (
                r#"{"type":"unsubscribe-logs","serverId":"s1"}"#,
                Request::UnsubscribeLogs {
                    server_id: "s1".into(),
                },
            ),
            (
                r#"{"type":"rcon","serverId":"s1","command":"list"}"#,
                Request::Rcon {
                    server_id: "s1".into(),
                    command: "list".into(),
                },
            ),
        ];
        for (raw, expected) in cases {
            let request = session.read(raw).expect(raw);
            assert_eq!(request, expected);
            assert_eq!(
                request.scope(),
                Scope::Server("s1".into()),
                "{raw} must be gated on membership of s1"
            );
        }
    }

    #[test]
    fn app_logs_are_scoped_to_the_device_not_to_a_server() {
        // The caller is a member of this *device*, which is a different
        // question from any server on it — the desktop asks GET /api/device/
        // for exactly this and nothing else.
        let request = authed().read(r#"{"type":"get-app-logs"}"#).unwrap();
        assert_eq!(request, Request::GetAppLogs);
        assert_eq!(request.scope(), Scope::Device);
    }

    #[test]
    fn rcon_without_a_command_is_refused() {
        assert_eq!(
            authed().read(r#"{"type":"rcon","serverId":"s1"}"#),
            Err(Refusal::Missing {
                frame: "rcon".into(),
                field: "command".into()
            })
        );
    }

    #[test]
    fn an_unknown_type_names_itself() {
        let refusal = authed()
            .read(r#"{"type":"restart-everything"}"#)
            .unwrap_err();
        assert_eq!(refusal, Refusal::UnknownType("restart-everything".into()));
        assert!(
            refusal.message().contains("restart-everything"),
            "a developer reading the console should see what was rejected"
        );
    }

    #[test]
    fn garbage_is_not_json() {
        assert_eq!(authed().read("not json at all"), Err(Refusal::NotJson));
        assert_eq!(Session::new().read("{"), Err(Refusal::NotJson));
    }

    /// The three frames that carry text off the device are scrubbed as they
    /// are built. A join line carries a player's address and a chat line
    /// whatever they typed; the rule is the crash report's, and it is made
    /// here so no driver can send a raw line by forgetting a step.
    #[test]
    fn console_frames_leave_without_addresses_or_chat() {
        let join = "[12:00:00] [Server thread/INFO]: Steve[/203.0.113.4:52341] logged in with entity id 42";
        let chat = "[12:00:00] [Server thread/INFO]: <Steve> my address is 10.0.0.7 come over";

        let frame = outgoing::log("s1", join, "t");
        let line = frame["line"].as_str().unwrap();
        assert!(!line.contains("203.0.113.4"), "{line}");
        assert!(line.contains("Steve"), "names survive by decision: {line}");

        let history = outgoing::log_history("s1", &[join.to_string(), chat.to_string()]);
        let lines = history["lines"].as_array().unwrap();
        assert!(!lines[0].as_str().unwrap().contains("203.0.113.4"));
        let chat_line = lines[1].as_str().unwrap();
        assert!(!chat_line.contains("come over"), "{chat_line}");
        assert!(!chat_line.contains("10.0.0.7"), "{chat_line}");
    }

    /// The app's own log is what support reads, and it quotes URLs, headers
    /// and whatever a page printed. Same redactor as an error report.
    #[test]
    fn app_logs_leave_without_tokens_addresses_or_emails() {
        let main = "08-12 14:13:37 1 2 D HomerunApi: GET /api/user/ Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhYmMifQ.abcdefghijklmnopqrstuvwxyz\n\
                    08-12 14:13:38 1 2 D HomerunTunnel: peer 198.51.100.7:51820 handshake\n";
        let renderer = "08-12 14:13:39 1 2 I HomerunWeb: signed in as someone@example.com\n";

        let frame = outgoing::app_logs(main, renderer);
        let main_log = frame["mainLog"].as_str().unwrap();
        let renderer_log = frame["rendererLog"].as_str().unwrap();
        assert!(!main_log.contains("eyJhbGci"), "{main_log}");
        assert!(!main_log.contains("198.51.100.7"), "{main_log}");
        assert!(
            main_log.contains("handshake"),
            "the line itself survives: {main_log}"
        );
        assert!(
            !renderer_log.contains("someone@example.com"),
            "{renderer_log}"
        );
    }

    /// The dashboard already parses these. A field renamed here is a console
    /// that silently stops updating, so they are asserted whole.
    #[test]
    fn outgoing_frames_match_what_the_dashboard_reads() {
        assert_eq!(outgoing::auth_ok(), json!({ "type": "auth-ok" }));
        assert_eq!(
            outgoing::error("nope"),
            json!({ "type": "error", "message": "nope" })
        );
        assert_eq!(
            outgoing::log("s1", "Done (2.3s)!", "2026-08-12T17:24:58.000Z"),
            json!({
                "type": "log",
                "serverId": "s1",
                "line": "Done (2.3s)!",
                "timestamp": "2026-08-12T17:24:58.000Z"
            })
        );
        assert_eq!(
            outgoing::log_history("s1", &["a".to_string(), "b".to_string()]),
            json!({ "type": "log-history", "serverId": "s1", "lines": ["a", "b"] })
        );
        assert_eq!(
            outgoing::rcon_response("s1", "There are 0 players", true),
            json!({
                "type": "rcon-response",
                "serverId": "s1",
                "response": "There are 0 players",
                "success": true
            })
        );
        assert_eq!(
            outgoing::server_status("s1", true),
            json!({ "type": "server-status", "serverId": "s1", "online": true })
        );
        assert_eq!(
            outgoing::app_logs("main", "renderer"),
            json!({ "type": "app-logs", "mainLog": "main", "rendererLog": "renderer" })
        );
    }

    /// Both refusals are the same sentence on purpose. A different one for
    /// "no such server" would tell an outsider which ids exist.
    #[test]
    fn a_forbidden_server_is_indistinguishable_from_an_absent_one() {
        assert_eq!(
            outgoing::not_authorized_for_server(),
            outgoing::error("Not authorized for this server")
        );
    }
}

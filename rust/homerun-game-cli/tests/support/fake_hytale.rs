//! A fake of Hytale's sign-in and session services, on loopback, for tests.
//!
//! Plays the parts of the Server Provider Authentication Guide the `hytale`
//! extension uses -- the device flow, refresh-token rotation, profiles, and
//! game sessions -- and records every request, so a test can say what was
//! asked and in what order. Shared by the runner's unit tests and the
//! lifecycle tests through `#[path]`, so there is one fake to keep honest.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

use serde_json::{json, Value};

/// What the fake does, and what it has seen.
#[derive(Debug)]
pub struct State {
    /// How many polls answer `authorization_pending` before approval.
    pub pending_polls: usize,
    /// Never approve: every poll is pending. For expiry and stop tests.
    pub never_approve: bool,
    pub expires_in: u64,
    /// The refresh token that currently works. Each refresh replaces it.
    pub live_refresh: Option<String>,
    pub issued: usize,
    pub profiles: Vec<(String, String)>,
    /// `Some(body)` refuses new sessions with 403 and that body.
    pub refuse_session: Option<String>,
    pub requests: Vec<Seen>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            pending_polls: 1,
            never_approve: false,
            expires_in: 900,
            live_refresh: None,
            issued: 0,
            profiles: vec![("uuid-1".into(), "Operator".into())],
            refuse_session: None,
            requests: vec![],
        }
    }
}

pub struct FakeHytale {
    pub port: u16,
    pub state: Arc<Mutex<State>>,
}

impl FakeHytale {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let state = shared.clone();
                thread::spawn(move || serve(stream, &state));
            }
        });
        Self { port, state }
    }

    /// The extension config pointing at this fake. Sign-in links are
    /// `https://accounts.example.com/...`, allowed by `signInHost`.
    pub fn config(&self) -> Value {
        let base = format!("http://127.0.0.1:{}", self.port);
        json!({
            "clientId": "hytale-server",
            "scope": "openid offline auth:server",
            "deviceUrl": format!("{base}/oauth2/device/auth"),
            "tokenUrl": format!("{base}/oauth2/token"),
            "profilesUrl": format!("{base}/my-account/get-profiles"),
            "sessionUrl": format!("{base}/game-session"),
            "signInHost": "example.com",
            "consoleSignIn": {
                "needed": "No server tokens configured",
                "command": "auth login device",
                "url": "accounts.example.com/device",
                "code": "Enter code: ",
                "done": "Authentication successful!"
            }
        })
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().requests.clone()
    }

    /// `METHOD /path` for each request, in order.
    pub fn calls(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|s| format!("{} {}", s.method, s.path))
            .collect()
    }
}

fn serve(stream: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    let mut authorization = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.trim().to_string();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap_or(0),
            "authorization" => authorization = Some(value),
            _ => {}
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let body = String::from_utf8_lossy(&body).into_owned();

    let (status, reply) = {
        let mut state = state.lock().unwrap();
        state.requests.push(Seen {
            method: method.clone(),
            path: path.clone(),
            authorization: authorization.clone(),
            body: body.clone(),
        });
        route(&mut state, &method, &path, authorization.as_deref(), &body)
    };
    // No body at all where there is nothing to say: a 204 must not have one.
    let text = if reply.is_null() {
        String::new()
    } else {
        reply.to_string()
    };
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Other",
    };
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

fn form(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.replace("%3A", ":").replace('+', " "))
    })
}

fn route(
    state: &mut State,
    method: &str,
    path: &str,
    authorization: Option<&str>,
    body: &str,
) -> (u16, Value) {
    match (method, path) {
        ("POST", "/oauth2/device/auth") => (
            200,
            json!({
                "device_code": "device-code-1",
                "user_code": "ABCD-1234",
                "verification_uri": "https://accounts.example.com/device",
                "verification_uri_complete": "https://accounts.example.com/device?user_code=ABCD-1234",
                "expires_in": state.expires_in,
                "interval": 1
            }),
        ),
        ("POST", "/oauth2/token") => match form(body, "grant_type").as_deref() {
            Some("urn:ietf:params:oauth:grant-type:device_code") => {
                if state.never_approve || state.pending_polls > 0 {
                    state.pending_polls = state.pending_polls.saturating_sub(1);
                    (400, json!({ "error": "authorization_pending" }))
                } else {
                    issue(state)
                }
            }
            Some("refresh_token") => {
                if form(body, "refresh_token") == state.live_refresh && state.live_refresh.is_some()
                {
                    issue(state)
                } else {
                    (400, json!({ "error": "invalid_grant" }))
                }
            }
            _ => (400, json!({ "error": "unsupported_grant_type" })),
        },
        ("GET", "/my-account/get-profiles") => {
            if authorization.is_some_and(|a| a.starts_with("Bearer access-")) {
                let profiles: Vec<Value> = state
                    .profiles
                    .iter()
                    .map(|(uuid, name)| json!({ "uuid": uuid, "username": name }))
                    .collect();
                (200, json!({ "owner": "owner-1", "profiles": profiles }))
            } else {
                (401, Value::Null)
            }
        }
        ("POST", "/game-session/new") => match &state.refuse_session {
            Some(body) => (403, Value::String(body.clone())),
            None => (
                200,
                json!({
                    "sessionToken": format!("session-token-{}", state.issued),
                    "identityToken": format!("identity-token-{}", state.issued),
                    "expiresAt": "2026-09-24T20:00:00Z"
                }),
            ),
        },
        ("DELETE", "/game-session") => (204, Value::Null),
        _ => (404, Value::Null),
    }
}

/// New tokens, and the refresh token that replaces the last one.
fn issue(state: &mut State) -> (u16, Value) {
    state.issued += 1;
    let refresh = format!("refresh-{}", state.issued);
    state.live_refresh = Some(refresh.clone());
    (
        200,
        json!({
            "access_token": format!("access-{}", state.issued),
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": refresh,
            "scope": "openid offline auth:server"
        }),
    )
}

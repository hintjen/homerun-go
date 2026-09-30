//! Plain HTTP to a game server's own admin API, on loopback, for a game's
//! extension.
//!
//! # Why this exists
//!
//! Some dedicated servers have no console on stdin at all, and the way their
//! vendor supports asking one to save and exit is a REST endpoint on the
//! server itself. Palworld is the first: `POST /v1/api/save`, then
//! `POST /v1/api/shutdown`, with basic authentication, on a port it can be
//! told to bind to loopback. Which requests, in what order, is that game's
//! extension's business; this is the one way any extension reaches such an
//! API, and it does not choose the rules:
//!
//!  - **Loopback, and nothing else.** A [`Target`] is a port number the
//!    runner resolved from a port *name* the descriptor declares private.
//!    There is no host in it, from the extension or anywhere else: every
//!    request goes to `127.0.0.1`. `engine::validate` holds the port to
//!    `expose: false` and TCP, and the runner's bind check stops a server
//!    that puts it on any other interface with `port_exposed`. So this is
//!    plain `http://` with no TLS, for the same reason `rcon.rs` has none:
//!    the request never leaves the machine.
//!  - **A plain path.** Absolute, bounded, printable, no `.` or `..`, and
//!    nothing that would end the request line ([`path_is_safe`]). The method
//!    is a closed set.
//!  - **No redirects.** A `3xx` is an answer like any other, and its
//!    `Location` is never followed.
//!  - **Bounded.** 15 seconds per request, a request body of at most 16 KiB
//!    and an answer of at most 1 MiB.
//!  - **Cancellable.** The answer is read in short steps that check the
//!    caller's cancellation, so a stop's grace is never stuck behind a
//!    server that accepted the connection and went quiet.
//!  - **Quiet.** Method, port and path go to stderr; the query string,
//!    bodies and the `Authorization` header never do.
//!
//! # std, not reqwest
//!
//! `reqwest` is in the tree for `vendor_http`, and it is the wrong tool here:
//! it follows redirects, resolves names and brings a runtime into a thread
//! that is walking a stop ladder. One HTTP/1.1 request on loopback is a
//! request line, a handful of headers and a status line, which is less code
//! than configuring a client out of everything it does by default.
//!
//! # The password
//!
//! A [`Request`] names its basic credentials as a user and a host secret's
//! *name*; the runner resolves the name into the [`Target`], whose `Debug`
//! prints `[redacted]`. The extension that wrote the request never sees the
//! value, and no failure message quotes a request or an answer.

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::Value;

pub use crate::vendor_http::{Failure, Response, MAX_BODY};

/// How long one request may take, start to finish. A save does real work
/// before it answers, so not a second; a wedged server must not hold a stop
/// for ever, so not unbounded. A caller's cancellation bounds it further.
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// The longest path, query included.
pub const MAX_PATH: usize = 256;

/// The largest request body, as serialised JSON.
pub const MAX_REQUEST_BODY: usize = 16 * 1024;

/// The longest status line and headers read before giving up on an answer.
const MAX_HEAD: usize = 16 * 1024;

/// How long one read waits before checking the cancellation again.
const STEP: Duration = Duration::from_millis(50);

/// The methods an extension may send. A closed set: nothing here deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
        }
    }
}

/// Basic credentials, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Basic {
    /// A literal user name: `admin` for Palworld. Not secret.
    pub user: String,
    /// The **name** of a host secret, never its value. The runner resolves
    /// it into the [`Target`].
    pub secret: String,
}

/// One request, as an extension writes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: Method,
    /// An absolute path on the server, query included: `/v1/api/save`.
    pub path: String,
    /// Sent as `application/json`. `None` sends no body.
    pub body: Option<Value>,
    pub basic: Option<Basic>,
}

impl Request {
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::Get, path)
    }
    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::Post, path)
    }
    pub fn put(path: impl Into<String>) -> Self {
        Self::new(Method::Put, path)
    }
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            body: None,
            basic: None,
        }
    }
    pub fn json(mut self, value: Value) -> Self {
        self.body = Some(value);
        self
    }
    /// Sign in as `user` with the host secret called `secret`.
    pub fn basic(mut self, user: impl Into<String>, secret: impl Into<String>) -> Self {
        self.basic = Some(Basic {
            user: user.into(),
            secret: secret.into(),
        });
        self
    }
}

/// Where a request goes, and the password its secret name resolved to.
/// Built by the runner, never by an extension.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    /// On `127.0.0.1`. What the server was told to bind.
    pub port: u16,
    password: Option<String>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("port", &self.port)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl Target {
    /// `password` is the value of the request's secret, when it names one.
    pub fn new(port: u16, password: Option<String>) -> Self {
        Self { port, password }
    }
}

/// Send one request to `127.0.0.1:<target.port>`, giving up once
/// `cancelled` is true.
///
/// Any status is `Ok`: whether it counts is the caller's decision. A request
/// the rules refuse is [`Failure::NotAllowed`] before anything is sent.
pub fn send(
    target: &Target,
    request: &Request,
    cancelled: &dyn Fn() -> bool,
) -> Result<Response, Failure> {
    let result = perform(target, request, cancelled);
    let status = match &result {
        Ok(response) => response.status.to_string(),
        Err(Failure::NotAllowed) => "refused".into(),
        Err(Failure::Cancelled) => "cancelled".into(),
        Err(Failure::Unreachable(_)) => "unreachable".into(),
    };
    // The route alone: a query is where a game would put what it should not
    // have. A refused path is not printed at all.
    let route = if path_is_safe(&request.path) {
        request.path.split('?').next().unwrap_or_default()
    } else {
        "(refused path)"
    };
    eprintln!(
        "extension local http: {} 127.0.0.1:{}{route} -> {status}",
        request.method.as_str(),
        target.port,
    );
    result
}

fn perform(
    target: &Target,
    request: &Request,
    cancelled: &dyn Fn() -> bool,
) -> Result<Response, Failure> {
    let body = request.body.as_ref().map(Value::to_string);
    let authorization = match (&request.basic, &target.password) {
        (None, _) => None,
        (Some(basic), Some(password)) if user_is_safe(&basic.user) => Some(format!(
            "Basic {}",
            base64(format!("{}:{password}", basic.user).as_bytes())
        )),
        // A user basic cannot carry, or a secret the runner did not resolve.
        (Some(_), _) => return Err(Failure::NotAllowed),
    };
    if target.port == 0
        || !path_is_safe(&request.path)
        || body.as_ref().is_some_and(|b| b.len() > MAX_REQUEST_BODY)
    {
        return Err(Failure::NotAllowed);
    }
    if cancelled() {
        return Err(Failure::Cancelled);
    }

    let begun = Instant::now();
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, target.port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|_| unreachable("Nothing answered on the server's admin port."))?;
    stream.set_nodelay(true).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let content = body.as_deref().unwrap_or_default();
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        request.method.as_str(),
        request.path,
        target.port,
    );
    if request.method != Method::Get {
        head.push_str(&format!("Content-Length: {}\r\n", content.len()));
    }
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    if let Some(authorization) = &authorization {
        head.push_str("Authorization: ");
        head.push_str(authorization);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(content.as_bytes()))
        .and_then(|_| stream.flush())
        .map_err(|_| unreachable("The server's admin port stopped answering mid-request."))?;

    // Read in short steps, checking the cancellation and the deadline
    // between them, until the answer is whole or the server closes.
    stream.set_read_timeout(Some(STEP)).ok();
    let mut received = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        if let Some(answer) = answer(&received, false) {
            return answer;
        }
        if cancelled() {
            return Err(Failure::Cancelled);
        }
        if begun.elapsed() >= TIMEOUT {
            return Err(unreachable("The server did not answer in time."));
        }
        match stream.read(&mut buffer) {
            Ok(0) => return answer(&received, true).unwrap_or_else(|| Err(not_http())),
            Ok(n) => {
                received.extend_from_slice(&buffer[..n]);
                if received.len() > MAX_HEAD + MAX_BODY {
                    return Err(too_much());
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(unreachable("The server's admin port stopped answering.")),
        }
    }
}

/// The answer in `received`, once it is whole: `None` while more is needed.
/// `closed` means the server has finished sending.
fn answer(received: &[u8], closed: bool) -> Option<Result<Response, Failure>> {
    let Some(end) = find(received, b"\r\n\r\n") else {
        return (closed || received.len() > MAX_HEAD).then(|| Err(not_http()));
    };
    let Some(head) = std::str::from_utf8(&received[..end]).ok() else {
        return Some(Err(not_http()));
    };
    let mut lines = head.split("\r\n");
    let Some(status) = lines.next().and_then(status) else {
        return Some(Err(not_http()));
    };
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse::<usize>().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    let rest = &received[end + 4..];
    let whole = |body: Vec<u8>| {
        Some(if body.len() > MAX_BODY {
            Err(too_much())
        } else {
            Ok(Response { status, body })
        })
    };
    // These carry no body, whatever the headers say.
    if (100..200).contains(&status) || status == 204 || status == 304 {
        return whole(vec![]);
    }
    if chunked {
        return match dechunk(rest) {
            Some(body) => whole(body),
            None => closed.then(|| Err(not_http())),
        };
    }
    match length {
        Some(length) if rest.len() >= length => whole(rest[..length].to_vec()),
        Some(_) => closed.then(|| Err(not_http())),
        // Delimited by the close, as `Connection: close` allows.
        None if closed => whole(rest.to_vec()),
        None => None,
    }
}

/// A chunked body, decoded: `None` while it is incomplete, or if it is not
/// chunked at all. Trailers are ignored.
fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = find(rest, b"\r\n")?;
        let size = std::str::from_utf8(&rest[..line]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line + 2..];
        if size == 0 {
            return Some(body);
        }
        if rest.len() < size.checked_add(2)? || body.len() + size > MAX_BODY {
            return None;
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `HTTP/1.1 200 OK` -> 200.
fn status(line: &str) -> Option<u16> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next()?;
    let code = parts.next()?;
    if !version.starts_with("HTTP/1.") || code.len() != 3 {
        return None;
    }
    code.parse().ok().filter(|c| (100..600).contains(c))
}

fn unreachable(message: &str) -> Failure {
    Failure::Unreachable(message.into())
}

fn not_http() -> Failure {
    unreachable("The server's answer was not HTTP.")
}

fn too_much() -> Failure {
    unreachable("The server answered with more than expected.")
}

/// Whether `path` may go on a request line.
///
/// Absolute, bounded, printable ASCII without the characters that would end
/// the request line or make it a different URL, and no `.` or `..` segment --
/// spelled out or percent-encoded -- so the path a reviewer reads in an
/// extension is the one the server routes.
pub fn path_is_safe(path: &str) -> bool {
    const REFUSED: &[u8] = b"\\#\"<>{}|^`";
    let route = path.split('?').next().unwrap_or_default();
    let lower = path.to_ascii_lowercase();
    path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= MAX_PATH
        && path
            .bytes()
            .all(|b| b.is_ascii_graphic() && !REFUSED.contains(&b))
        && !route
            .split('/')
            .any(|segment| segment == ".." || segment == ".")
        && !lower.contains("%2e")
        && !lower.contains("%2f")
}

/// Whether `user` is one basic authentication can carry.
fn user_is_safe(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 64
        && !user.contains(':')
        && !user.chars().any(char::is_control)
}

/// Standard base64, padded -- what basic authentication carries. Encode
/// only, and a dozen lines, which is why it is not a dependency.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// One connection, answered with `reply`; what arrived comes back.
    fn server(reply: &'static str) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 4096];
            // Headers, then as much body as Content-Length says.
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received.extend_from_slice(&buffer[..n]),
                }
                let text = String::from_utf8_lossy(&received);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length: usize = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|l| l.trim().parse().ok())
                        .unwrap_or(0);
                    if received.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(reply.as_bytes()).unwrap();
            tx.send(String::from_utf8(received).unwrap()).unwrap();
        });
        (port, rx)
    }

    fn never() -> bool {
        false
    }

    fn save() -> Request {
        Request::post("/v1/api/save")
            .json(json!({}))
            .basic("admin", "admin")
    }

    fn with_password(port: u16) -> Target {
        Target::new(port, Some("hunter2".into()))
    }

    #[test]
    fn a_request_carries_its_line_host_auth_and_body_and_returns_the_answer() {
        let (port, received) = server("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");
        let response = send(&with_password(port), &save(), &never).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.json(), Some(json!({ "ok": true })));
        let request = received.recv().unwrap();
        assert!(
            request.starts_with("POST /v1/api/save HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains(&format!("\r\nHost: 127.0.0.1:{port}\r\n")),
            "{request}"
        );
        // base64("admin:hunter2")
        assert!(
            request.contains("\r\nAuthorization: Basic YWRtaW46aHVudGVyMg==\r\n"),
            "{request}"
        );
        assert!(
            request.contains("\r\nContent-Type: application/json\r\n"),
            "{request}"
        );
        assert!(request.contains("\r\nContent-Length: 2\r\n"), "{request}");
        assert!(request.ends_with("\r\n\r\n{}"), "{request}");
    }

    /// Any status is an answer. A redirect in particular is not followed.
    #[test]
    fn a_refusal_or_a_redirect_is_an_answer_and_not_followed() {
        let (port, _) = server("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
        let response = send(&with_password(port), &save(), &never);
        assert_eq!(response.unwrap().status, 401);
        let (port, _) = server("HTTP/1.1 302 Found\r\nLocation: http://evil.example/\r\n\r\n");
        let response = send(&Target::new(port, None), &Request::post("/save"), &never);
        assert_eq!(response.unwrap().status, 302);
    }

    #[test]
    fn no_body_and_no_credentials_send_neither() {
        let (port, received) = server("HTTP/1.0 204 No Content\r\n\r\n");
        let response = send(&Target::new(port, None), &Request::put("/save"), &never);
        assert_eq!(response.unwrap().status, 204);
        let request = received.recv().unwrap();
        assert!(!request.contains("Content-Type"), "{request}");
        assert!(!request.contains("Authorization"), "{request}");
        assert!(request.contains("Content-Length: 0\r\n"), "{request}");
    }

    #[test]
    fn a_chunked_or_close_delimited_answer_is_read_whole() {
        let (port, _) = server(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n",
        );
        let response = send(&Target::new(port, None), &Request::get("/info"), &never);
        assert_eq!(response.unwrap().json(), Some(json!({ "a": 1 })));

        let (port, _) = server("HTTP/1.1 200 OK\r\n\r\nplain");
        let response = send(&Target::new(port, None), &Request::get("/info"), &never);
        assert_eq!(response.unwrap().text(), "plain");
    }

    #[test]
    fn nothing_listening_and_not_http_are_failures_in_words() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let Err(Failure::Unreachable(why)) = send(&with_password(port), &save(), &never) else {
            panic!("a closed port answered");
        };
        assert!(why.contains("Nothing answered"), "{why}");
        assert!(!why.contains("hunter2"));

        let (port, _) = server("SSH-2.0-OpenSSH\r\n");
        let Err(Failure::Unreachable(why)) =
            send(&Target::new(port, None), &Request::post("/save"), &never)
        else {
            panic!("not HTTP was read as HTTP");
        };
        assert!(why.contains("not HTTP"), "{why}");
    }

    /// A server that accepts and never answers must not hold a stop: the
    /// caller's cancellation ends the request well before the timeout.
    #[test]
    fn a_silent_server_is_given_up_on_when_the_caller_cancels() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(5));
            drop(stream);
        });
        let begun = Instant::now();
        let cancelled = move || begun.elapsed() > Duration::from_millis(200);
        assert_eq!(
            send(
                &Target::new(port, None),
                &Request::post("/save"),
                &cancelled
            ),
            Err(Failure::Cancelled)
        );
        assert!(
            begun.elapsed() < Duration::from_secs(2),
            "{:?}",
            begun.elapsed()
        );
    }

    #[test]
    fn the_password_never_appears_in_debug_output() {
        let printed = format!("{:?}", with_password(8212));
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("[redacted]"), "{printed}");
    }

    /// Refused before anything is sent, so nothing needs to be listening.
    #[test]
    fn a_request_the_rules_refuse_is_not_sent() {
        let target = with_password(1);
        for request in [
            Request::post("/v1/api/save HTTP/1.1\r\nX: y"),
            Request::post("http://evil.example/"),
            Request::post("/v1/api/save").basic("ad:min", "admin"),
            Request::post("/v1/api/save").basic("", "admin"),
            Request::post("/save").json(json!({ "m": "x".repeat(MAX_REQUEST_BODY) })),
        ] {
            assert_eq!(
                send(&target, &request, &never),
                Err(Failure::NotAllowed),
                "{request:?}"
            );
        }
        // Credentials whose secret the runner did not resolve.
        assert_eq!(
            send(&Target::new(1, None), &save(), &never),
            Err(Failure::NotAllowed)
        );
        assert_eq!(
            send(&Target::new(0, None), &Request::post("/save"), &never),
            Err(Failure::NotAllowed)
        );
    }

    /// The runner is only ever given a path. Anything that could make it a
    /// different URL, or end the request line, is refused.
    #[test]
    fn a_path_is_a_plain_absolute_path() {
        for bad in [
            "",
            "v1/api/save",
            "http://evil.example/v1/api/save",
            "//evil.example/v1/api/save",
            "/v1/../admin",
            "/v1/./api",
            "/v1/%2e%2e/admin",
            "/v1/api%2Fsave",
            "/v1/api/save HTTP/1.1\r\nHost: x",
            "/v1/api/save\n",
            "/v1/api/save#x",
            "/v1\\api",
            "/v1/ api",
            "/v1/api/sav\u{e9}",
        ] {
            assert!(!path_is_safe(bad), "{bad:?} was accepted");
        }
        assert!(!path_is_safe(&format!("/{}", "a".repeat(MAX_PATH))));
        for good in ["/v1/api/save", "/", "/api/stop?now=1&why=update", "/a..b/c"] {
            assert!(path_is_safe(good), "{good:?} was refused");
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn a_status_line_is_read_strictly() {
        assert_eq!(status("HTTP/1.1 200 OK"), Some(200));
        assert_eq!(status("HTTP/1.1 204"), Some(204));
        assert_eq!(status("HTTP/2 200"), None);
        assert_eq!(status("HTTP/1.1 20 OK"), None);
        assert_eq!(status("HTTP/1.1 999 What"), None);
        assert_eq!(status(""), None);
    }
}

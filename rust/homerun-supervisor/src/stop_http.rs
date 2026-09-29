//! Asking a server to stop through its own HTTP admin API.
//!
//! # Why this exists
//!
//! Some dedicated servers have no console on stdin at all, and the way their
//! vendor supports asking one to save and exit is a REST endpoint. Palworld
//! is the first: `POST /v1/api/save`, then `POST /v1/api/shutdown`, with
//! basic authentication, on a port it can be told to bind to loopback. Its
//! RCON is deprecated and binds every interface with no setting to stop it,
//! so it is not the way in. Without this, the only stop such a game gets on
//! Windows is a terminate -- a piped-stdio child has no console for a
//! control event -- and whatever was not saved is lost.
//!
//! # Loopback, and nothing else
//!
//! A [`Target`] is a port number. There is no host in it, from the
//! descriptor or anywhere else: every request goes to `127.0.0.1`. The port
//! is one the descriptor declares private, which `engine::validate` requires
//! and the runner's bind check enforces while the server runs -- a private
//! port seen on any other interface stops the server with `port_exposed`.
//! So this is plain `http://` with no TLS, for the same reason `rcon.rs` has
//! none: the request never leaves the machine.
//!
//! # std, not reqwest
//!
//! `reqwest` is in the tree behind `game-engine`, and it is the wrong tool
//! here: it follows redirects, resolves names and would bring a runtime into
//! a thread that walks a stop ladder. One HTTP/1.1 request on loopback is a
//! request line, five headers and a status line, which is less code than
//! configuring a client out of everything it does by default. Nothing here
//! follows a redirect: a `3xx` is an answer that is not `2xx`.
//!
//! # The password
//!
//! It is only ever inside [`Target`]'s `Authorization` value, whose `Debug`
//! prints `[redacted]`, and no failure message quotes a request or a reply.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use homerun_core::engine::descriptor::{HttpMethod, HttpRequest};
use homerun_core::engine::validate::{
    http_path_is_safe, MAX_HTTP_STOP_BODY, MAX_HTTP_STOP_REQUESTS,
};

/// The longest any one request may take. A save does real work before it
/// answers, so not a second; a wedged server must not hold the ladder for
/// ever, so not unbounded. The rung's own grace bounds it further.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The longest status line read before giving up on it as not HTTP.
const MAX_STATUS_LINE: u64 = 1024;

/// Where the requests go, and how they are let in.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    /// On `127.0.0.1`. What the server was told to bind, not what the
    /// descriptor preferred.
    pub port: u16,
    /// The whole `Authorization` header value, when there is one.
    authorization: Option<String>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("port", &self.port)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl Target {
    /// `basic` is the user name and the resolved secret.
    pub fn new(port: u16, basic: Option<(&str, &str)>) -> Self {
        Self {
            port,
            authorization: basic.map(|(user, password)| {
                format!("Basic {}", base64(format!("{user}:{password}").as_bytes()))
            }),
        }
    }
}

/// One request, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: &'static str,
    pub path: String,
    /// Serialised JSON, or nothing.
    pub body: Option<String>,
}

/// A whole HTTP stop: where, and what, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    pub target: Target,
    pub requests: Vec<Request>,
}

impl Stop {
    /// From the descriptor's requests, asked again whether each is one this
    /// may send. `engine::validate` has refused a bad one already; this is
    /// the check standing next to the socket, so a descriptor that reached
    /// here some other way still cannot put a second line in a request.
    pub fn new(target: Target, requests: &[HttpRequest]) -> Result<Self, String> {
        let refused =
            || "This game's descriptor asks for a stop Homerun will not send.".to_string();
        if requests.is_empty() || requests.len() > MAX_HTTP_STOP_REQUESTS {
            return Err(refused());
        }
        let requests = requests
            .iter()
            .map(|r| {
                let method = match r.method {
                    HttpMethod::Post => "POST",
                    HttpMethod::Put => "PUT",
                    HttpMethod::Unknown => return Err(refused()),
                };
                if !http_path_is_safe(&r.path) {
                    return Err(refused());
                }
                let body = r.body.as_ref().map(|b| b.to_string());
                if body.as_ref().is_some_and(|b| b.len() > MAX_HTTP_STOP_BODY) {
                    return Err(refused());
                }
                Ok(Request {
                    method,
                    path: r.path.clone(),
                    body,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { target, requests })
    }
}

/// Send one request and return the status the server answered with.
///
/// Any status is `Ok`: whether it counts is the caller's decision. `Err` is
/// a request that got no answer, in a sentence that quotes neither the
/// request nor the reply.
pub fn send(target: &Target, request: &Request, timeout: Duration) -> Result<u16, String> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, target.port));
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|_| "nothing answered on the server's admin port".to_string())?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    stream.set_nodelay(true).ok();

    let body = request.body.as_deref().unwrap_or_default();
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n",
        request.method,
        request.path,
        target.port,
        body.len()
    );
    if request.body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    if let Some(authorization) = &target.authorization {
        head.push_str("Authorization: ");
        head.push_str(authorization);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(body.as_bytes()))
        .and_then(|_| stream.flush())
        .map_err(|_| "the server's admin port stopped listening mid-request".to_string())?;

    // Only the status line matters. The rest -- headers, a body -- is left
    // unread; the connection closes when this returns.
    let mut line = Vec::new();
    BufReader::new(std::io::Read::take(&mut stream, MAX_STATUS_LINE))
        .read_until(b'\n', &mut line)
        .map_err(|_| "the server did not answer in time".to_string())?;
    status(&line).ok_or_else(|| "the server's answer was not HTTP".to_string())
}

/// `HTTP/1.1 200 OK` -> 200.
fn status(line: &[u8]) -> Option<u16> {
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.trim_end().splitn(3, ' ');
    let version = parts.next()?;
    let code = parts.next()?;
    if !version.starts_with("HTTP/1.") || code.len() != 3 {
        return None;
    }
    code.parse().ok().filter(|c| (100..600).contains(c))
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
    use std::io::Read;
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

    fn save() -> Request {
        Request {
            method: "POST",
            path: "/v1/api/save".into(),
            body: Some("{}".into()),
        }
    }

    #[test]
    fn a_request_carries_its_line_host_auth_and_body_and_returns_the_status() {
        let (port, received) = server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let target = Target::new(port, Some(("admin", "hunter2")));
        assert_eq!(send(&target, &save(), REQUEST_TIMEOUT), Ok(200));
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
        let (port, _) = server("HTTP/1.1 401 Unauthorized\r\n\r\n");
        assert_eq!(
            send(&Target::new(port, None), &save(), REQUEST_TIMEOUT),
            Ok(401)
        );
        let (port, _) = server("HTTP/1.1 302 Found\r\nLocation: http://evil.example/\r\n\r\n");
        assert_eq!(
            send(&Target::new(port, None), &save(), REQUEST_TIMEOUT),
            Ok(302)
        );
    }

    #[test]
    fn no_body_sends_no_content_type_and_a_zero_length() {
        let (port, received) = server("HTTP/1.0 204 No Content\r\n\r\n");
        let request = Request {
            body: None,
            ..save()
        };
        assert_eq!(
            send(&Target::new(port, None), &request, REQUEST_TIMEOUT),
            Ok(204)
        );
        let request = received.recv().unwrap();
        assert!(!request.contains("Content-Type"), "{request}");
        assert!(!request.contains("Authorization"), "{request}");
        assert!(request.contains("Content-Length: 0\r\n"), "{request}");
    }

    #[test]
    fn nothing_listening_and_not_http_are_failures_in_words() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = send(
            &Target::new(port, Some(("admin", "hunter2"))),
            &save(),
            REQUEST_TIMEOUT,
        )
        .unwrap_err();
        assert!(err.contains("nothing answered"), "{err}");
        assert!(!err.contains("hunter2"));

        let (port, _) = server("SSH-2.0-OpenSSH\r\n");
        let err = send(&Target::new(port, None), &save(), REQUEST_TIMEOUT).unwrap_err();
        assert!(err.contains("not HTTP"), "{err}");
    }

    #[test]
    fn the_password_never_appears_in_debug_output() {
        let stop = Stop::new(
            Target::new(8212, Some(("admin", "hunter2"))),
            &[HttpRequest {
                method: HttpMethod::Post,
                path: "/v1/api/save".into(),
                body: None,
            }],
        )
        .unwrap();
        let printed = format!("{stop:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(!printed.contains("YWRtaW46aHVudGVyMg"), "{printed}");
        assert!(printed.contains("[redacted]"), "{printed}");
    }

    /// The check beside the socket: validation should have refused these,
    /// and this refuses them again rather than send them.
    #[test]
    fn a_request_validation_would_refuse_is_refused_here_too() {
        let target = Target::new(8212, None);
        let request = |method, path: &str| HttpRequest {
            method,
            path: path.into(),
            body: None,
        };
        for bad in [
            vec![],
            vec![request(HttpMethod::Unknown, "/v1/api/save")],
            vec![request(HttpMethod::Post, "/v1/api/save HTTP/1.1\r\nX: y")],
            vec![request(HttpMethod::Post, "http://evil.example/")],
            vec![request(HttpMethod::Post, "/save"); MAX_HTTP_STOP_REQUESTS + 1],
        ] {
            assert!(Stop::new(target.clone(), &bad).is_err(), "{bad:?}");
        }
        let big = HttpRequest {
            body: Some(serde_json::json!({ "m": "x".repeat(MAX_HTTP_STOP_BODY) })),
            ..request(HttpMethod::Put, "/save")
        };
        assert!(Stop::new(target.clone(), &[big]).is_err());
        let ok = Stop::new(target, &[request(HttpMethod::Put, "/save")]).unwrap();
        assert_eq!(ok.requests[0].method, "PUT");
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
        assert_eq!(status(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(status(b"HTTP/1.1 204\r\n"), Some(204));
        assert_eq!(status(b"HTTP/2 200\r\n"), None);
        assert_eq!(status(b"HTTP/1.1 20 OK\r\n"), None);
        assert_eq!(status(b"HTTP/1.1 999 What\r\n"), None);
        assert_eq!(status(b""), None);
    }
}

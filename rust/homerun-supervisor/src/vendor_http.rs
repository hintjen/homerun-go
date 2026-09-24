//! HTTPS to a game vendor's own hosts, for a game's extension.
//!
//! An extension talks to its vendor -- a sign-in service, a session API --
//! through this and nothing else. It does not choose the rules:
//!
//!  - **Only the hosts its spec names.** Every URL, and every redirect hop,
//!    must pass `homerun_core::engine::extensions::url_allowed` against the
//!    hosts the spec derives from the descriptor's config. A redirect anywhere
//!    else ends the request instead of being followed.
//!  - **Bounded.** 30 seconds per request, and a response body of at most
//!    1 MiB. A vendor that streams forever cannot hold a start, or memory.
//!  - **Cancellable.** The request races the caller's cancellation, so a
//!    player's Stop is never stuck behind a silent peer.
//!  - **Quiet.** Host, path and status go to stderr (the runner's host log);
//!    query strings and bodies never do, because that is where tokens and
//!    device codes travel.
//!
//! `loopback_http` exists for the runner's own tests, which serve a fake
//! vendor on `http://127.0.0.1`. The runner sets it only when built with its
//! test-only feature.

use std::time::Duration;

use homerun_core::engine::extensions::url_allowed;
use serde_json::Value;

/// How long one request may take, start to finish.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The largest response body read. Past it, the request fails.
pub const MAX_BODY: usize = 1024 * 1024;

/// Redirect hops followed, all of them on allowed hosts.
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// `application/x-www-form-urlencoded`, as OAuth token endpoints want.
    Form(Vec<(String, String)>),
    Json(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Self {
        Self::new(Method::Get, url)
    }
    pub fn post(url: impl Into<String>) -> Self {
        Self::new(Method::Post, url)
    }
    pub fn delete(url: impl Into<String>) -> Self {
        Self::new(Method::Delete, url)
    }
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: vec![],
            body: None,
        }
    }
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    pub fn form(mut self, fields: &[(&str, &str)]) -> Self {
        self.body = Some(Body::Form(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ));
        self
    }
    pub fn json(mut self, value: Value) -> Self {
        self.body = Some(Body::Json(value));
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Where requests may go.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// From the spec: `ExtensionSpec::hosts(config)`.
    pub hosts: Vec<String>,
    /// Also allow `http://127.0.0.1:<port>`. Test builds only.
    pub loopback_http: bool,
}

impl Policy {
    pub fn allows(&self, url: &str) -> bool {
        url_allowed(url, &self.hosts) || (self.loopback_http && is_loopback_http(url))
    }
}

/// Why a request did not produce a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The URL, or a redirect, pointed somewhere the policy does not allow.
    NotAllowed,
    Cancelled,
    /// Could not connect, timed out, or answered with too much.
    Unreachable(String),
}

/// Send one request under `policy`, giving up if `cancelled` becomes true.
pub fn send(
    request: &Request,
    policy: &Policy,
    cancelled: &dyn Fn() -> bool,
) -> Result<Response, Failure> {
    if !policy.allows(&request.url) {
        return Err(Failure::NotAllowed);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Failure::Unreachable("Homerun could not start a network request.".into()))?;
    let result = runtime.block_on(async {
        tokio::select! {
            result = tokio::time::timeout(TIMEOUT, perform(request, policy)) => match result {
                Ok(result) => result,
                Err(_) => Err(Failure::Unreachable(format!(
                    "{} took too long to answer.",
                    host_of(&request.url)
                ))),
            },
            _ = async {
                while !cancelled() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => Err(Failure::Cancelled),
        }
    });
    let status = match &result {
        Ok(response) => response.status.to_string(),
        Err(Failure::NotAllowed) => "refused".into(),
        Err(Failure::Cancelled) => "cancelled".into(),
        Err(Failure::Unreachable(_)) => "unreachable".into(),
    };
    eprintln!(
        "extension http: {:?} {} -> {status}",
        request.method,
        without_query(&request.url)
    );
    result
}

async fn perform(request: &Request, policy: &Policy) -> Result<Response, Failure> {
    let allowed = policy.clone();
    let redirects = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if allowed.allows(attempt.url().as_str()) {
            attempt.follow()
        } else {
            attempt.error("redirected off the vendor's site")
        }
    });
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(redirects)
        .build()
        .map_err(|_| Failure::Unreachable("Homerun could not start a network request.".into()))?;
    let method = match request.method {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Delete => reqwest::Method::DELETE,
    };
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    builder = match &request.body {
        None => builder,
        Some(Body::Form(fields)) => builder.form(fields),
        Some(Body::Json(value)) => builder.json(value),
    };
    let mut response = builder.send().await.map_err(|err| {
        if err.is_redirect() {
            Failure::NotAllowed
        } else {
            Failure::Unreachable(format!(
                "Homerun could not reach {}.",
                host_of(&request.url)
            ))
        }
    })?;
    // The policy above refused any other hop; this is the check that does
    // not depend on how a redirect was followed.
    if !policy.allows(response.url().as_str()) {
        return Err(Failure::NotAllowed);
    }
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        Failure::Unreachable(format!("{} stopped answering.", host_of(&request.url)))
    })? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(Failure::Unreachable(format!(
                "{} sent more than Homerun expected.",
                host_of(&request.url)
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Response { status, body })
}

/// `http://127.0.0.1:<port>/...` and nothing that only looks like it.
fn is_loopback_http(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let port: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let after = &rest[port.len()..];
    !port.is_empty()
        && port.parse::<u16>().is_ok()
        && (after.is_empty() || after.starts_with(['/', '?', '#']))
}

fn without_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("the game's service")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
        time::Instant,
    };

    /// Answer each connection with the next canned response, in order.
    fn serve(responses: Vec<String>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        port
    }

    fn reply(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn loopback() -> Policy {
        Policy {
            hosts: vec!["vendor.example".into()],
            loopback_http: true,
        }
    }

    #[test]
    fn a_request_to_an_allowed_host_returns_status_and_body() {
        let port = serve(vec![reply("201 Created", "{\"ok\":true}")]);
        let response = send(
            &Request::post(format!("http://127.0.0.1:{port}/token")).form(&[("a", "b")]),
            &loopback(),
            &|| false,
        )
        .unwrap();
        assert_eq!(response.status, 201);
        assert!(response.ok());
        assert_eq!(response.json().unwrap()["ok"], true);
    }

    #[test]
    fn anywhere_else_is_refused_before_connecting() {
        let policy = Policy {
            hosts: vec!["vendor.example".into()],
            loopback_http: false,
        };
        for url in [
            "http://127.0.0.1:1/x",
            "https://evil.example/x",
            "http://vendor.example/x",
            "https://vendor.example.evil.net/x",
        ] {
            assert_eq!(
                send(&Request::get(url), &policy, &|| false),
                Err(Failure::NotAllowed),
                "{url}"
            );
        }
    }

    #[test]
    fn loopback_http_means_exactly_127_0_0_1_with_a_port() {
        assert!(is_loopback_http("http://127.0.0.1:8080/x"));
        assert!(is_loopback_http("http://127.0.0.1:8080"));
        for url in [
            "http://127.0.0.1/x",
            "http://127.0.0.1:99999/x",
            "http://127.0.0.1:80@evil.net/x",
            "http://127.0.0.1:80.evil.net/x",
            "https://127.0.0.1:80/x",
            "http://localhost:80/x",
        ] {
            assert!(!is_loopback_http(url), "{url}");
        }
    }

    #[test]
    fn a_redirect_off_the_allowed_hosts_is_not_followed() {
        let port = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: https://evil.example/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into(),
        ]);
        assert_eq!(
            send(
                &Request::get(format!("http://127.0.0.1:{port}/start")),
                &loopback(),
                &|| false
            ),
            Err(Failure::NotAllowed)
        );
    }

    #[test]
    fn a_body_past_the_limit_fails_the_request() {
        let big = "x".repeat(MAX_BODY + 1);
        let port = serve(vec![reply("200 OK", &big)]);
        assert!(matches!(
            send(
                &Request::get(format!("http://127.0.0.1:{port}/big")),
                &loopback(),
                &|| false
            ),
            Err(Failure::Unreachable(_))
        ));
    }

    /// A peer that accepts and never answers must not hold a Stop.
    #[test]
    fn cancellation_ends_a_request_to_a_silent_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _held = thread::spawn(move || {
            let held = listener.accept();
            thread::sleep(Duration::from_secs(5));
            drop(held);
        });
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            s.store(true, Ordering::SeqCst);
        });
        let begun = Instant::now();
        let result = send(
            &Request::get(format!("http://127.0.0.1:{port}/silent")),
            &loopback(),
            &|| stop.load(Ordering::SeqCst),
        );
        assert_eq!(result, Err(Failure::Cancelled));
        assert!(
            begun.elapsed() < Duration::from_secs(3),
            "{:?}",
            begun.elapsed()
        );
    }

    #[test]
    fn nothing_after_the_path_reaches_the_log() {
        assert_eq!(
            without_query("https://v.example/token?device_code=secret#x"),
            "https://v.example/token"
        );
    }
}

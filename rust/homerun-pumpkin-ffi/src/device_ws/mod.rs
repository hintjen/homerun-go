//! The websocket the dashboard connects to, and the effects behind it.
//!
//! What a frame *means* is decided in [`homerun_core::device_ws::protocol`],
//! which is pure and has no idea a socket exists. This is the other half:
//! accepting connections, verifying a token against Keycloak's JWKS, asking the
//! API what the caller may touch, and moving console lines and RCON between the
//! socket and the supervisor.
//!
//! Reference: `deviceWebsocket/handlers.ts` in the `homerun` repo.
//!
//! # Why the console needs no round trip to the host
//!
//! The supervisor already owns the console — every line a server writes and
//! every note the host adds goes into [`crate::server::ServerHost`]'s buffer,
//! which pages by cursor. So `subscribe-logs` reads the same buffer the host's
//! own log pump reads, in-process, and `rcon` goes straight to
//! [`crate::server::ServerHost::command`]. Nothing crosses the FFI in either
//! direction while a console is streaming.
//!
//! # Fail closed, always
//!
//! Every authorisation question is asked of the API with the **caller's** token
//! and denied if the answer does not arrive. Failing open would serve a console
//! to anyone whenever the API is unreachable, which is exactly when nobody is
//! watching.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use homerun_core::device_ws::protocol::{
    outgoing, Refusal, Request, Scope, Session, CLOSE_AUTH_FAILED, HEARTBEAT_INTERVAL_MS,
};
use homerun_core::minecraft;
use homerun_core::reporting;

pub mod app_logs;
pub mod tls;

/// Everything the socket needs that only the host can know.
#[derive(Debug, Clone)]
pub struct Config {
    /// The loopback port the tunnel forwards the gateway's `:443` to.
    pub port: u16,
    /// e.g. `https://api.gethomerun.app`.
    pub api_url: String,
    /// Keycloak's JWKS endpoint.
    pub jwks_url: String,
    /// This device's API id, for device-scoped requests.
    pub device_id: String,
    /// This device's public hostname. Absent means no certificate can be
    /// obtained, and the socket serves plaintext — reachable through the
    /// tunnel, not reachable by a browser.
    pub fqdn: Option<String>,
    /// Where the ACME account and the certificate live. Absent has the same
    /// effect as no `fqdn`.
    pub storage_dir: Option<String>,
    /// The loopback port the tunnel forwards the gateway's `:80` to, for the
    /// ACME challenge. Absent means no order can be validated.
    pub challenge_port: Option<u16>,
    /// True on the legacy plane, where nginx writes a PROXY v1 header ahead of
    /// the ClientHello. The v2 gateway writes none, and stripping one that is
    /// not there eats the handshake — see
    /// [`homerun_core::device_ws::proxy_protocol`].
    pub expect_proxy_protocol: bool,
    /// Use Let's Encrypt's staging directory. Its certificates are not trusted
    /// by browsers, which is the point: staging has generous rate limits and
    /// production allows five certificates per hostname per week.
    pub acme_staging: bool,
}

/// The `azp` claim every Homerun token carries.
///
/// Checked because a token minted for another client of the same realm is a
/// valid signature over the wrong audience — it proves who, for something else.
/// Mirrors `FractalJWTAuthentication` in the API.
const EXPECTED_AZP: &str = "homerun";

/// How many frames may be queued for a peer before it is dropped.
///
/// The bounded equivalent of the desktop's `bufferedAmount` cap: a peer that is
/// still connected but has stopped draining is indistinguishable from a slow
/// one until you cap it, and a console during world generation produces lines
/// far faster than a stalled socket accepts them.
const OUTGOING_QUEUE: usize = 512;

/// How often a subscribed console is drained.
///
/// The supervisor's buffer is polled rather than pushed, exactly as the hosts'
/// own log pumps poll it — one cursor per subscriber, so two dashboards
/// watching the same server cannot consume each other's lines.
const CONSOLE_POLL: Duration = Duration::from_millis(250);

struct Running {
    shutdown: tokio::sync::oneshot::Sender<()>,
    runtime: tokio::runtime::Runtime,
}

static RUNNING: OnceLock<Mutex<Option<Running>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Running>> {
    RUNNING.get_or_init(|| Mutex::new(None))
}

/// Bind the listener and serve until [`stop`] is called.
///
/// Returns the port actually bound, which is [`Config::port`] unless that was
/// 0. Binds `127.0.0.1` only: the sole route in is the tunnel, and a listener
/// on all interfaces would also be reachable from the phone's LAN, which is not
/// something anyone asked for.
pub fn start(config: Config) -> Result<Bound, String> {
    let mut current = slot()
        .lock()
        .map_err(|_| "the device websocket lock is poisoned")?;
    if current.is_some() {
        return Err("the device websocket is already running".to_string());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("the device websocket could not start: {e}"))?;

    // Two listeners, both on loopback, both speaking the same protocol.
    //
    // The plaintext one is what the app's own UI connects to: the shared UI
    // asks `get-device-ws-port` and dials `ws://localhost:<port>` for the
    // device it is running on, reserving `wss://<fqdn>` for other people's
    // devices. Terminating TLS on the single port would break that — the
    // certificate is for the public hostname and a loopback client has no
    // reason to present that SNI.
    //
    // Reaching the plaintext port still requires a Keycloak token and an API
    // membership check, so another app on the device gains nothing by finding
    // it. This is the shape desktop has had all along, and the reason its
    // cert-manager sits in front of a separate plaintext server rather than
    // replacing it.
    let listener = runtime
        .block_on(async { TcpListener::bind(("127.0.0.1", config.port)).await })
        .map_err(|e| format!("the device websocket could not bind: {e}"))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("the device websocket has no address: {e}"))?
        .port();

    // The TLS one is what the tunnel forwards the gateway's `:443` to. Bound
    // now, before the certificate exists, so the host can render the tunnel
    // config immediately — an order takes seconds and the forward has to be
    // right from the start.
    let tls_listener = runtime
        .block_on(async { TcpListener::bind(("127.0.0.1", 0)).await })
        .map_err(|e| format!("the TLS listener could not bind: {e}"))?;
    let tls_bound = tls_listener
        .local_addr()
        .map_err(|e| format!("the TLS listener has no address: {e}"))?
        .port();

    let (shutdown, mut stopping) = tokio::sync::oneshot::channel();
    let expect_proxy = config.expect_proxy_protocol;
    let tls_inputs = config
        .fqdn
        .clone()
        .zip(config.storage_dir.clone())
        .zip(config.challenge_port)
        .map(|((fqdn, dir), port)| (fqdn, dir, port));
    let staging = config.acme_staging;
    let state = Arc::new(Shared::new(config));

    // The plaintext loop: the app's own UI, and nothing else.
    {
        let state = Arc::clone(&state);
        runtime.spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            let _ = stream.set_nodelay(true);
                            if let Err(err) = serve(stream, state).await {
                                log::warn(&format!("local connection refused: {err}"));
                            }
                        });
                    }
                    Err(err) => {
                        log::warn(&format!("accept failed: {err}"));
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }
        });
    }

    runtime.spawn(async move {
        // The certificate is obtained *after* both listeners are bound, so a
        // slow or failing order never delays the ports coming up. Until it
        // arrives the TLS listener has nothing to hand a connection, and says
        // so rather than answering plaintext on a port the gateway will send a
        // ClientHello to.
        let acceptor = match tls_inputs {
            None => {
                log::warn("no hostname or storage — serving plaintext, which no browser will use");
                None
            }
            Some((fqdn, dir, challenge_port)) => {
                let store = tls::CertStore::new(dir);
                match tls::ensure_certificate(&store, &fqdn, challenge_port, staging).await {
                    Ok(certificate) => match tls::acceptor(&certificate) {
                        Ok(acceptor) => {
                            log::info(&format!("serving TLS for {fqdn}"));
                            Some(acceptor)
                        }
                        Err(err) => {
                            log::warn(&format!("the certificate could not be loaded: {err}"));
                            None
                        }
                    },
                    Err(err) => {
                        log::warn(&format!("no certificate: {err}"));
                        None
                    }
                }
            }
        };

        loop {
            tokio::select! {
                _ = &mut stopping => break,
                accepted = tls_listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let Some(acceptor) = acceptor.clone() else {
                            // Nothing to hand it. Dropping beats answering
                            // plaintext to a peer that opened with a
                            // ClientHello, which would look like a protocol
                            // error rather than a missing certificate.
                            log::warn("a connection arrived before there was a certificate");
                            continue;
                        };
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            // Warn, not debug. A connection that fails before
                            // it becomes a websocket is either a broken
                            // handshake or a misread PROXY header, and both are
                            // invisible from the dashboard's side — a silent
                            // one here is how a device looks unreachable with
                            // nothing anywhere saying why.
                            if let Err(err) = accept(stream, state, acceptor, expect_proxy).await {
                                log::warn(&format!("connection refused: {err}"));
                            }
                        });
                    }
                    // A listener that cannot accept is usually out of file
                    // descriptors. Backing off beats spinning on it.
                    Err(err) => {
                        log::warn(&format!("accept failed: {err}"));
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                },
            }
        }
    });

    *current = Some(Running { shutdown, runtime });
    Ok(Bound {
        plaintext: bound,
        tls: tls_bound,
    })
}

/// The two ports a host has to know about.
pub struct Bound {
    /// What `get-device-ws-port` answers, and what the app's own UI dials.
    pub plaintext: u16,
    /// What the tunnel forwards the gateway's `:443` to.
    pub tls: u16,
}

/// Stop serving and release the port.
pub fn stop() {
    let taken = slot().lock().ok().and_then(|mut slot| slot.take());
    if let Some(running) = taken {
        let _ = running.shutdown.send(());
        // Do not wait on in-flight connections: a dashboard holding a console
        // open would otherwise keep the port bound past a logout.
        running.runtime.shutdown_timeout(Duration::from_secs(1));
    }
}

pub fn is_running() -> bool {
    slot().lock().map(|slot| slot.is_some()).unwrap_or(false)
}

/// Take one connection from the gateway through to a websocket.
///
/// Three things happen in order and each has to be exact: strip the PROXY
/// header if the plane sends one, complete the TLS handshake if there is a
/// certificate, then upgrade. Getting the first wrong breaks the second with a
/// message about neither.
async fn accept(
    stream: TcpStream,
    shared: Arc<Shared>,
    acceptor: tokio_rustls::TlsAcceptor,
    expect_proxy: bool,
) -> Result<(), String> {
    // A peer that dies without a FIN leaves a socket that never errors. The
    // app-level ping is the real defence; this is the backstop under it.
    let _ = stream.set_nodelay(true);

    let stream = if expect_proxy {
        strip_proxy_header(stream).await?
    } else {
        Prefixed::new(stream, Vec::new())
    };

    let tls = acceptor
        .accept(stream)
        .await
        .map_err(|e| format!("the TLS handshake failed: {e}"))?;
    serve(tls, shared).await
}

/// Read the PROXY v1 header off the front, or establish there is none.
///
/// Reads a byte at a time until the parser can answer. That is slower than one
/// large read and it is the only way to guarantee nothing is consumed when the
/// answer is "no header" — which is every connection on the v2 plane. Forty-odd
/// syscalls once per connection is not a cost worth trading correctness for.
async fn strip_proxy_header(mut stream: TcpStream) -> Result<Prefixed<TcpStream>, String> {
    use homerun_core::device_ws::proxy_protocol::{self, Preface};

    let mut seen = Vec::with_capacity(proxy_protocol::MAX_HEADER);
    loop {
        match proxy_protocol::read(&seen) {
            Preface::Header { consumed, line } => {
                log::debug(&format!("stripped {line:?}"));
                // Anything read past the header is the client's first bytes and
                // must be handed on, not dropped.
                return Ok(Prefixed::new(stream, seen[consumed..].to_vec()));
            }
            // Not a header after all: everything read so far belongs to the
            // client, so it is replayed rather than consumed.
            Preface::Absent => return Ok(Prefixed::new(stream, seen)),
            Preface::Incomplete => {
                let mut byte = [0u8; 1];
                match stream.read_exact(&mut byte).await {
                    Ok(_) => seen.push(byte[0]),
                    Err(err) => return Err(format!("the peer closed mid-header: {err}")),
                }
            }
        }
    }
}

/// A stream that yields some already-read bytes before the socket's own.
///
/// Needed because deciding whether a PROXY header is present means reading
/// bytes that may turn out to belong to the TLS handshake. Without this they
/// would be lost, and the failure would be a handshake error naming nothing.
struct Prefixed<S> {
    inner: S,
    prefix: Vec<u8>,
    offset: usize,
}

impl<S> Prefixed<S> {
    fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self {
            inner,
            prefix,
            offset: 0,
        }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        if self.offset < self.prefix.len() {
            let remaining = &self.prefix[self.offset..];
            let take = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..take]);
            self.offset += take;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// State every connection shares.
struct Shared {
    config: Config,
    http: reqwest::Client,
    /// Keycloak's signing keys, fetched once and reused.
    ///
    /// Not refreshed on a miss yet: a realm that rotates keys mid-session makes
    /// every later token fail to verify until the process restarts. Worth
    /// fixing before this ships to anyone, and cheap — refetch once per unknown
    /// `kid`, rate-limited.
    jwks: tokio::sync::Mutex<Option<HashMap<String, jsonwebtoken::DecodingKey>>>,
}

impl Shared {
    fn new(config: Config) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            jwks: tokio::sync::Mutex::new(None),
        }
    }
}

/// One connection, from the upgrade to the close.
///
/// Generic over the stream because by here it may be a plain socket or a TLS
/// session, and nothing below this point cares which.
async fn serve<S>(stream: S, shared: Arc<Shared>) -> Result<(), String>
where
    // `Send + 'static` because the writer half is moved into its own task:
    // the read loop and the write loop have to make progress independently, or
    // a slow console blocks the frame the dashboard just sent.
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let socket = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| format!("upgrade failed: {e}"))?;
    let (mut sink, mut incoming) = socket.split();

    let (out, mut queued) = mpsc::channel::<Message>(OUTGOING_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut connection = Connection {
        shared,
        session: Session::new(),
        token: None,
        allowed_servers: None,
        allowed_device: None,
        subscriptions: HashSet::new(),
        cursors: HashMap::new(),
        out: out.clone(),
    };

    // Auth has a deadline, and it is the socket's whole lifetime until it is
    // met: a peer that opens a connection and says nothing is either broken or
    // scanning the gateway's port.
    let deadline = tokio::time::sleep(Duration::from_millis(
        homerun_core::device_ws::protocol::AUTH_TIMEOUT_MS,
    ));
    tokio::pin!(deadline);

    let mut heartbeat = tokio::time::interval(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
    heartbeat.tick().await; // the first tick is immediate
    let mut alive = true;

    let mut console = tokio::time::interval(CONSOLE_POLL);

    loop {
        tokio::select! {
            _ = &mut deadline, if !connection.session.is_authenticated() => {
                connection.send(outgoing::error("Authentication timed out"));
                connection.close_unauthenticated("auth timeout");
                break;
            }
            _ = heartbeat.tick() => {
                if !alive {
                    log::info("terminating a peer that missed a ping round");
                    break;
                }
                alive = false;
                if connection.out.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
            _ = console.tick(), if !connection.subscriptions.is_empty() => {
                connection.drain_consoles();
            }
            frame = incoming.next() => match frame {
                None => break,
                Some(Err(err)) => {
                    log::debug(&format!("read failed: {err}"));
                    break;
                }
                Some(Ok(Message::Pong(_))) => alive = true,
                Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Text(raw))) => {
                    if !connection.handle(&raw).await {
                        break;
                    }
                }
                // Binary and the rest are not part of this protocol.
                Some(Ok(_)) => {}
            },
        }
    }

    drop(connection);
    drop(out);
    let _ = writer.await;
    Ok(())
}

/// Everything one connection knows.
struct Connection {
    shared: Arc<Shared>,
    session: Session,
    /// The caller's own token, kept to ask the API questions **as them**.
    /// Never logged: it is a bearer credential for the whole account.
    token: Option<String>,
    /// Resolved once per connection. `None` until asked; a membership change
    /// therefore takes effect on reconnect, which is the desktop's tradeoff.
    allowed_servers: Option<HashSet<String>>,
    allowed_device: Option<bool>,
    /// serverId -> the console cursor this connection has consumed to.
    subscriptions: HashSet<String>,
    cursors: HashMap<String, u64>,
    out: mpsc::Sender<Message>,
}

impl Connection {
    /// Queue a frame, or drop the peer if it has stopped draining.
    fn send(&self, frame: Value) {
        // `try_send` rather than `send`: a full queue is the signal that this
        // peer is not reading, and blocking on it would stall the console for
        // everyone else watching the same server.
        if self.out.try_send(Message::Text(frame.to_string())).is_err() {
            log::info("dropping a peer whose queue is full");
        }
    }

    /// Close with the code the dashboard distinguishes.
    ///
    /// One code for every authentication failure, matching the desktop: the
    /// client needs to tell "you may not be here" from a transport failure, and
    /// does not need to know *which* way it got it wrong. The `error` frame
    /// sent just before carries that.
    fn close_unauthenticated(&self, reason: &'static str) {
        let _ = self.out.try_send(Message::Close(Some(
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: CLOSE_AUTH_FAILED.into(),
                reason: reason.into(),
            },
        )));
    }

    /// Returns false when the socket should close.
    async fn handle(&mut self, raw: &str) -> bool {
        let request = match self.session.read(raw) {
            Ok(request) => request,
            Err(refusal) => {
                self.send(outgoing::error(refusal.message()));
                if refusal == Refusal::Unauthenticated {
                    self.close_unauthenticated("not authenticated");
                }
                return !refusal.is_fatal();
            }
        };

        // Authorisation before anything is carried out, and denied when the
        // question cannot be answered.
        match request.scope() {
            Scope::None => {}
            Scope::Server(ref id) => match self.may_touch_server(id).await {
                Some(true) => {}
                Some(false) => {
                    self.send(outgoing::not_authorized_for_server());
                    return true;
                }
                None => {
                    self.send(outgoing::authorization_unavailable());
                    return true;
                }
            },
            Scope::Device => match self.may_touch_device().await {
                Some(true) => {}
                Some(false) => {
                    self.send(outgoing::not_authorized_for_device());
                    return true;
                }
                None => {
                    self.send(outgoing::authorization_unavailable());
                    return true;
                }
            },
        }

        match request {
            Request::Auth { token } => {
                match self.shared.verify(&token).await {
                    Ok(subject) => {
                        log::info(&format!("authenticated sub={subject}"));
                        self.session.authenticated();
                        self.token = Some(token);
                        self.send(outgoing::auth_ok());
                    }
                    Err(err) => {
                        log::info(&format!("rejected a token: {err}"));
                        self.send(outgoing::error("Invalid token"));
                        self.close_unauthenticated("invalid token");
                        return false;
                    }
                }
                true
            }
            Request::SubscribeLogs { server_id } => {
                self.subscribe(&server_id);
                true
            }
            Request::UnsubscribeLogs { server_id } => {
                self.subscriptions.remove(&server_id);
                self.cursors.remove(&server_id);
                true
            }
            Request::Rcon { server_id, command } => {
                let outcome = crate::server::host().command(&command);
                let (text, ok) = match outcome {
                    Ok(()) => ("".to_string(), true),
                    Err(err) => (err, false),
                };
                self.send(outgoing::rcon_response(&server_id, &text, ok));
                // The dashboard's console is a console: an `op` typed here has
                // to reach the server's settings for the same reason it does
                // when typed in the app, or the next launch rewrites ops.json
                // from the API and takes it back. The app's own path does this
                // in `Reporting.consoleCommand`; this is the other half.
                if ok {
                    self.sync_ops(&server_id, &command).await;
                }
                true
            }
            Request::GetAppLogs => {
                // logcat, filtered to this process by logd — see
                // [`app_logs`]. Read on the caller's task rather than
                // spawned: it is one `logcat -d` that returns in milliseconds,
                // and a support request is not a hot path.
                let (main_log, renderer_log) = app_logs::collect();
                self.send(outgoing::app_logs(&main_log, &renderer_log));
                true
            }
        }
    }

    /// Mirror an `op`/`deop`/`ban`/`pardon` into the server's settings.
    ///
    /// `ops.json` and `banned-players.json` are rewritten from the API's
    /// environment variables at every launch, so an operator granted only
    /// through a console silently loses it on the next start unless this runs.
    ///
    /// Signed as **the person who typed it**, which is the whole reason this
    /// lives here rather than being handed to the host: the caller's token is
    /// what authenticated this socket, and it is the only credential that
    /// identifies them. The device token would be accepted and the change
    /// stripped.
    ///
    /// Silent and best-effort by design — the command has already run on the
    /// server, and a settings write that fails must not turn a working console
    /// into an error. It says so in the log, and on the console the caller is
    /// already reading.
    async fn sync_ops(&self, server_id: &str, command: &str) {
        let Some(parsed) = minecraft::ops::parse(command) else {
            return;
        };
        let Some(token) = self.token.clone() else {
            return;
        };

        let path = format!("/api/server/{server_id}/");
        let Some(body) = self.shared.read_as(&token, &path).await else {
            log::warn(&format!(
                "could not read {server_id} to change its operators"
            ));
            return;
        };

        // None means the settings already say this — a `/op` for somebody who
        // is already an operator is not a change to save.
        let Some(change) = minecraft::ops::sync(&parsed, &body, server_id) else {
            return;
        };

        if self.shared.perform_as(&token, &change.request).await {
            // Onto the console, so the person who typed the command sees that
            // it was kept — and only once it has been, because telling them it
            // was saved and then losing it is worse than saying nothing.
            crate::server::host().push_log(change.line);
        }
    }

    /// Send the console so far, then stream it.
    fn subscribe(&mut self, server_id: &str) {
        let host = crate::server::host();
        let (_, running, _, _) = host.snapshot();
        let online = running.as_deref() == Some(server_id);

        // History first, and only for the server this device is actually
        // running: the supervisor holds one console, and answering another
        // server's id with it would show a dashboard the wrong world's output.
        let slice = if online {
            host.logs_since(0)
        } else {
            crate::log_buffer::LogSlice {
                lines: Vec::new(),
                cursor: 0,
                dropped: false,
            }
        };
        self.send(outgoing::log_history(server_id, &slice.lines));
        self.send(outgoing::server_status(server_id, online));

        self.cursors.insert(server_id.to_string(), slice.cursor);
        self.subscriptions.insert(server_id.to_string());
    }

    /// Push whatever the console has produced since this connection's cursor.
    fn drain_consoles(&mut self) {
        let host = crate::server::host();
        let (_, running, _, _) = host.snapshot();
        let running = match running {
            Some(id) => id,
            None => return,
        };
        if !self.subscriptions.contains(&running) {
            return;
        }

        let cursor = self.cursors.get(&running).copied().unwrap_or(0);
        let slice = host.logs_since(cursor);
        if slice.lines.is_empty() {
            return;
        }
        // The timestamp is the host's, because homerun-core has no clock. The
        // dashboard renders it, so drift matters less than it being present.
        let now = timestamp();
        for line in &slice.lines {
            self.send(outgoing::log(&running, line, &now));
        }
        self.cursors.insert(running, slice.cursor);
    }

    /// Whether the caller is a member of this server, or `None` if the API
    /// could not be asked. Cached for the connection's lifetime.
    async fn may_touch_server(&mut self, server_id: &str) -> Option<bool> {
        if self.allowed_servers.is_none() {
            let token = self.token.clone()?;
            self.allowed_servers = self.shared.membership(&token, "/api/server/").await;
        }
        self.allowed_servers
            .as_ref()
            .map(|ids| ids.contains(server_id))
    }

    async fn may_touch_device(&mut self) -> Option<bool> {
        if self.allowed_device.is_none() {
            let token = self.token.clone()?;
            let ids = self.shared.membership(&token, "/api/device/").await?;
            self.allowed_device = Some(ids.contains(&self.shared.config.device_id));
        }
        self.allowed_device
    }
}

impl Shared {
    /// Verify a Keycloak access token, returning its subject.
    async fn verify(&self, token: &str) -> Result<String, String> {
        let header = jsonwebtoken::decode_header(token).map_err(|e| e.to_string())?;
        let kid = header.kid.ok_or("the token names no signing key")?;

        let keys = self.signing_keys().await?;
        let key = keys
            .get(&kid)
            .ok_or("the token was signed by a key this device does not know")?;

        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        // The API's own authentication does not check `aud` either, and
        // Keycloak's default access tokens carry an `aud` that would fail here.
        // `azp` below is the check that matters.
        validation.validate_aud = false;
        let decoded = jsonwebtoken::decode::<Value>(token, key, &validation)
            .map_err(|e| format!("the token did not verify: {e}"))?;

        let azp = decoded.claims.get("azp").and_then(Value::as_str);
        if azp != Some(EXPECTED_AZP) {
            return Err(format!(
                "the token was minted for {} rather than {EXPECTED_AZP}",
                azp.unwrap_or("(nothing)")
            ));
        }

        Ok(decoded
            .claims
            .get("sub")
            .and_then(Value::as_str)
            .unwrap_or("(unknown)")
            .to_string())
    }

    async fn signing_keys(&self) -> Result<HashMap<String, jsonwebtoken::DecodingKey>, String> {
        let mut cached = self.jwks.lock().await;
        if let Some(keys) = cached.as_ref() {
            return Ok(keys.clone());
        }

        let body: Value = self
            .http
            .get(&self.config.jwks_url)
            .send()
            .await
            .map_err(|e| format!("the signing keys could not be fetched: {e}"))?
            .json()
            .await
            .map_err(|e| format!("the signing keys were not JSON: {e}"))?;

        let mut keys = HashMap::new();
        for jwk in body
            .get("keys")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            let (Some(kid), Some(n), Some(e)) = (
                jwk.get("kid").and_then(Value::as_str),
                jwk.get("n").and_then(Value::as_str),
                jwk.get("e").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let Ok(key) = jsonwebtoken::DecodingKey::from_rsa_components(n, e) {
                keys.insert(kid.to_string(), key);
            }
        }

        if keys.is_empty() {
            return Err("the signing key set was empty".to_string());
        }
        *cached = Some(keys.clone());
        Ok(keys)
    }

    /// A record read as the caller, or `None` if it could not be read.
    ///
    /// For decisions that degrade rather than fail — every caller treats a
    /// missing answer as "change nothing".
    async fn read_as(&self, token: &str, path: &str) -> Option<Value> {
        let url = format!("{}{path}", self.config.api_url.trim_end_matches('/'));
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| log::warn(&format!("could not read {path}: {e}")))
            .ok()?;
        if !response.status().is_success() {
            log::warn(&format!("reading {path} returned {}", response.status()));
            return None;
        }
        response.json().await.ok()
    }

    /// Carry out a request `homerun-core` decided on, as the caller.
    ///
    /// The **caller's** token, never the device's. The API judges a settings
    /// change against the person who asked for it, and strips one made by
    /// somebody who could not have made it in the UI rather than refusing it —
    /// so signing this with the device token would read as success and change
    /// nothing.
    async fn perform_as(&self, token: &str, request: &reporting::Request) -> bool {
        let url = format!(
            "{}{}",
            self.config.api_url.trim_end_matches('/'),
            request.path
        );
        let builder = match request.method {
            reporting::Method::Patch => self.http.patch(url),
            reporting::Method::Post => self.http.post(url),
        };
        match builder.bearer_auth(token).json(&request.body).send().await {
            Ok(response) if response.status().is_success() => true,
            Ok(response) => {
                log::warn(&format!("{} returned {}", request.path, response.status()));
                false
            }
            Err(error) => {
                log::warn(&format!("{} did not go through: {error}", request.path));
                false
            }
        }
    }

    /// The ids at `path` the caller is a member of, or `None` if the API could
    /// not be asked. `None` is a denial everywhere it is used.
    async fn membership(&self, token: &str, path: &str) -> Option<HashSet<String>> {
        let url = format!("{}{path}", self.config.api_url.trim_end_matches('/'));
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| log::warn(&format!("membership check failed: {e}")))
            .ok()?;
        if !response.status().is_success() {
            log::warn(&format!("membership check returned {}", response.status()));
            return None;
        }
        let body: Value = response.json().await.ok()?;
        // The API answers either a bare array or a paginated object.
        let list = body
            .as_array()
            .or_else(|| body.get("results").and_then(Value::as_array))?;
        Some(
            list.iter()
                .filter_map(|item| item.get("id"))
                .map(|id| match id.as_str() {
                    Some(text) => text.to_string(),
                    None => id.to_string(),
                })
                .collect(),
        )
    }
}

/// An ISO-8601 timestamp, which the dashboard renders.
///
/// Hand-rolled rather than pulling a date crate in for one line: this is the
/// only place in the crate that needs wall-clock formatting, and the format is
/// fixed by what the desktop already sends.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();

    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Civil-from-days, Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Logging that actually arrives somewhere.
///
/// Through the `log` facade, never `eprintln!`. Two reasons, and the first cost
/// a debugging round: **Android does not capture a process's stdout or stderr
/// at all**, so an `eprintln!` here is written to nothing and a device with no
/// certificate has no explanation anywhere. And after a server launches stdout
/// *is* the pipe feeding the player-visible console — the same trap `HostLog`
/// documents on iOS.
///
/// `nativeInitLogging` wires the facade to logcat under `HomerunNative`.
mod log {
    pub fn info(message: &str) {
        ::log::info!("[DeviceWs] {message}");
    }
    pub fn warn(message: &str) {
        ::log::warn!("[DeviceWs] {message}");
    }
    pub fn debug(message: &str) {
        ::log::debug!("[DeviceWs] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dashboard parses these, so the shape is not ours to drift.
    #[test]
    fn timestamps_are_iso_8601_with_milliseconds() {
        let stamped = timestamp();
        assert_eq!(stamped.len(), 24, "{stamped}");
        assert_eq!(&stamped[4..5], "-");
        assert_eq!(&stamped[10..11], "T");
        assert!(stamped.ends_with('Z'));
        // Sanity: this decade, not 1970 — a wrong epoch would still be
        // well-formed and would be invisible in the format alone.
        let year: i32 = stamped[..4].parse().unwrap();
        assert!(year >= 2026, "{stamped}");
    }

    #[test]
    fn stopping_when_nothing_runs_is_not_an_error() {
        stop();
        assert!(!is_running());
    }
}

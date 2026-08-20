//! The certificate a device serves `wss://` with, and how it gets one.
//!
//! Reference: `deviceWebsocket/tls/` in the `homerun` repo, which does this in
//! ~1,300 lines of TypeScript including a hand-rolled ACME client. This is the
//! same flow against a maintained one.
//!
//! # Why a device does this at all
//!
//! The gateway is SNI `:443` passthrough — it routes by hostname and forwards
//! the bytes untouched, so the TLS session terminates *here*. That is the whole
//! reason this file exists; a gateway that terminated would have removed it.
//! See `plans/device-websocket.md`.
//!
//! # HTTP-01
//!
//! The gateway forwards its `:80` to a second local port, and Let's Encrypt
//! fetches `http://<fqdn>/.well-known/acme-challenge/<token>` over it. That
//! listener answers exactly one path and exists only while an order is in
//! flight — nothing else is ever served on `:80`, which is why the forward is
//! omitted when there is no order to run.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use super::log;
use homerun_core::reporting::app_error;

/// instant-acme's HTTP, carried by the client this crate already has.
///
/// Its own client is hyper-rustls over `rustls-platform-verifier`, which on
/// Android has to be handed a `JavaVM` and a `Context` over JNI before it can
/// read the system trust store. Skip that and it does not report an error — the
/// order simply never completes, which is exactly how this presented: a device
/// that logged "ordering one" and then nothing at all.
///
/// reqwest is already here, already reaches the API and Keycloak from a phone,
/// and carries its own roots. One HTTP stack, no platform dependency.
struct AcmeHttp(reqwest::Client);

impl instant_acme::HttpClient for AcmeHttp {
    fn request(
        &self,
        request: http::Request<instant_acme::BodyWrapper<bytes::Bytes>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<instant_acme::BytesResponse, instant_acme::Error>,
                > + Send,
        >,
    > {
        let client = self.0.clone();
        Box::pin(async move {
            use http_body_util::BodyExt as _;

            let (parts, body) = request.into_parts();
            let bytes = body
                .collect()
                .await
                .map(|collected| collected.to_bytes())
                .map_err(|e| instant_acme::Error::Other(Box::new(e)))?;

            let response = client
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(bytes)
                .send()
                .await
                .map_err(|e| instant_acme::Error::Other(Box::new(e)))?;

            Ok(instant_acme::BytesResponse::from(http::Response::from(
                response,
            )))
        })
    }
}

/// Renew once a certificate is this old.
///
/// Let's Encrypt issues for 90 days. Thirty days of margin is generous on a
/// desktop and barely enough on a phone, which may not be opened for a
/// fortnight — see the renewal note in `plans/device-websocket.md`.
const RENEW_AFTER: Duration = Duration::from_secs(60 * 24 * 60 * 60);

/// How long a whole order may take before it is called a failure.
///
/// Generous: an HTTP-01 validation involves Let's Encrypt reaching this device
/// from the internet, and a phone on a slow link is not a broken phone. It is
/// here to bound *silence*, not to be a tight timeout -- five minutes is far
/// longer than a healthy order and far shorter than for ever.
const ORDER_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Where the account and the certificate live.
///
/// One directory, app-private. It holds a private key, so on Android it must
/// stay out of auto-backup (`allowBackup="false"`, already set) and on iOS out
/// of iCloud.
pub struct CertStore {
    dir: PathBuf,
}

/// A certificate and the key that goes with it, ready to serve.
pub struct Certificate {
    pub chain_pem: String,
    pub key_pem: String,
    /// When this was issued, as seconds since the epoch. Stored beside the
    /// certificate rather than parsed back out of it: the only question ever
    /// asked is "is it time to renew", and answering it from a number we wrote
    /// costs nothing where an X.509 parser is a dependency and a decode path.
    pub issued_at: u64,
}

impl Certificate {
    fn stale(&self) -> bool {
        let age = now().saturating_sub(self.issued_at);
        Duration::from_secs(age) >= RENEW_AFTER
    }
}

impl CertStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The certificate on disk, if there is a usable one.
    ///
    /// A half-written store — a chain with no key, a key with no timestamp —
    /// reads as nothing rather than as an error. The answer either way is to
    /// order a new one, and a device that refused to start over a corrupt cache
    /// would be unreachable until someone cleared it by hand.
    pub fn load(&self) -> Option<Certificate> {
        let chain_pem = std::fs::read_to_string(self.path("chain.pem")).ok()?;
        let key_pem = std::fs::read_to_string(self.path("key.pem")).ok()?;
        let issued_at = std::fs::read_to_string(self.path("issued-at"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        if chain_pem.is_empty() || key_pem.is_empty() {
            return None;
        }
        Some(Certificate {
            chain_pem,
            key_pem,
            issued_at,
        })
    }

    fn save(&self, certificate: &Certificate) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_private(&self.path("key.pem"), &certificate.key_pem)?;
        std::fs::write(self.path("chain.pem"), &certificate.chain_pem)?;
        std::fs::write(self.path("issued-at"), certificate.issued_at.to_string())?;
        Ok(())
    }

    fn load_account(&self) -> Option<AccountCredentials> {
        let raw = std::fs::read_to_string(self.path("account.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn save_account(&self, credentials: &AccountCredentials) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let raw = serde_json::to_string(credentials)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private(&self.path("account.json"), &raw)
    }
}

/// Write a file only this app can read.
///
/// The account key and the certificate key are both credentials. App-private
/// storage already keeps other apps out; this keeps the file mode honest as
/// well, which costs one syscall and closes the case where the directory is
/// later made group-readable by something else.
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// A certificate for `fqdn`, from the store or from Let's Encrypt.
///
/// Never fails the caller into having nothing: an ACME failure with a usable
/// certificate already on disk returns that one, expired or not. A device that
/// refused to serve because renewal failed would be unreachable for the days it
/// still had left on the old certificate, which is the wrong way round.
/// A way to report a degraded outcome without this file knowing who we are.
///
/// Same shape as `pumpkin_settings::load_config`'s `warn`, and for the same
/// reason: the decision about *where* a report goes belongs to the caller, and
/// threading an `app_error::Context` down here would put host identity in a
/// file whose entire job is certificates.
/// `Sync` because this is held across an await inside a task tokio spawns:
/// a future is only `Send` when everything it holds is, and a bare
/// `&dyn Fn` is not.
pub type Note<'a> = &'a (dyn Fn(&str, app_error::Severity, String) + Sync);

pub async fn ensure_certificate(
    store: &CertStore,
    fqdn: &str,
    challenge_port: u16,
    staging: bool,
    note: Note<'_>,
) -> Result<Certificate, String> {
    let existing = store.load();
    match &existing {
        Some(certificate) if !certificate.stale() => {
            log::info("the stored certificate is still fresh");
            return Ok(store.load().expect("just loaded"));
        }
        Some(_) => log::info("the stored certificate is due for renewal"),
        None => log::info("no certificate stored — ordering one"),
    }

    // Bounded, because the failure this file was written to fix produced no
    // error at all. `instant-acme`'s poll uses a retry policy that can wait a
    // very long time, and a validation that never completes leaves the whole
    // order hanging -- which is exactly how the platform-verifier bug in the
    // header presented: "ordering one", and then nothing, for ever. Silence is
    // the one failure nothing downstream can see, so it is turned into an
    // error here rather than left to be nothing.
    let ordered = match tokio::time::timeout(
        ORDER_DEADLINE,
        order_certificate(store, fqdn, challenge_port, staging),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "the order did not finish within {}s",
            ORDER_DEADLINE.as_secs()
        )),
    };

    match ordered {
        Ok(fresh) => {
            if let Err(err) = store.save(&fresh) {
                // Worth saying loudly. The certificate works for this run, and
                // every later launch will order another one — Let's Encrypt
                // rate-limits that at five per week per hostname.
                log::warn(&format!("the new certificate was not stored: {err}"));
                // Which is why this reports. The symptom arrives days later
                // and nowhere near the cause: a device that quietly re-orders
                // on every launch works fine until the sixth one in a week,
                // and then stops getting certificates for reasons nothing on
                // the device explains.
                note(
                    "cert-not-stored",
                    app_error::Severity::Error,
                    format!("the new certificate was not stored: {err}"),
                );
            }
            Ok(fresh)
        }
        Err(err) => match existing {
            Some(certificate) => {
                log::warn(&format!(
                    "renewal failed ({err}); serving the certificate already held"
                ));
                // Serving, so not fatal — but the held certificate is already
                // past [`RENEW_AFTER`] and will expire. This is the window in
                // which it is still fixable, and it closes silently.
                note(
                    "cert-renewal-failed",
                    app_error::Severity::Error,
                    format!("renewal failed ({err}); serving the certificate already held"),
                );
                Ok(certificate)
            }
            None => Err(err),
        },
    }
}

/// A boxed HTTP client for instant-acme. See [`AcmeHttp`].
fn http_client() -> Box<dyn instant_acme::HttpClient> {
    Box::new(AcmeHttp(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default(),
    ))
}

async fn order_certificate(
    store: &CertStore,
    fqdn: &str,
    challenge_port: u16,
    staging: bool,
) -> Result<Certificate, String> {
    let directory = if staging {
        LetsEncrypt::Staging.url()
    } else {
        LetsEncrypt::Production.url()
    };

    // Reuse the account across renewals. A new account per order is not an
    // error, but it burns a rate limit that exists precisely to stop it.
    let account = match store.load_account() {
        Some(credentials) => Account::builder_with_http(http_client())
            .from_credentials(credentials)
            .await
            .map_err(|e| format!("the stored ACME account was rejected: {e}"))?,
        None => {
            let (account, credentials) = Account::builder_with_http(http_client())
                .create(
                    &NewAccount {
                        contact: &[],
                        terms_of_service_agreed: true,
                        only_return_existing: false,
                    },
                    directory.to_owned(),
                    None,
                )
                .await
                .map_err(|e| format!("no ACME account could be created: {e}"))?;
            if let Err(err) = store.save_account(&credentials) {
                log::warn(&format!("the ACME account was not stored: {err}"));
            }
            account
        }
    };

    let identifiers = [Identifier::Dns(fqdn.to_string())];
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .map_err(|e| format!("the certificate order was refused: {e}"))?;

    // Collected before anything is served: the listener needs every token it
    // might be asked for, and the borrow on `order` has to end before the
    // order can be polled.
    let mut responses: Vec<(String, String)> = Vec::new();
    {
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authorization = result.map_err(|e| e.to_string())?;
            if authorization.status == instant_acme::AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = authorization
                .challenge(ChallengeType::Http01)
                .ok_or("Let's Encrypt offered no HTTP-01 challenge for this name")?;
            responses.push((
                challenge.token.clone(),
                challenge.key_authorization().as_str().to_string(),
            ));
            challenge
                .set_ready()
                .await
                .map_err(|e| format!("the challenge could not be marked ready: {e}"))?;
        }
    }

    // The listener runs only while the order does. `:80` carries nothing else,
    // so leaving it up would be a surface with no purpose.
    let challenge_server = serve_challenges(challenge_port, responses).await?;

    let status = order
        .poll_ready(&RetryPolicy::default())
        .await
        .map_err(|e| format!("the order never became ready: {e}"));
    challenge_server.abort();
    if status? != OrderStatus::Ready {
        return Err("Let's Encrypt did not validate this device's hostname".to_string());
    }

    let key_pem = order
        .finalize()
        .await
        .map_err(|e| format!("the order could not be finalised: {e}"))?;
    let chain_pem = order
        .poll_certificate(&RetryPolicy::default())
        .await
        .map_err(|e| format!("the certificate never arrived: {e}"))?;

    log::info("a certificate was issued");
    Ok(Certificate {
        chain_pem,
        key_pem,
        issued_at: now(),
    })
}

/// Answer HTTP-01 challenges, and nothing else, until aborted.
///
/// Hand-rolled rather than an HTTP framework: it serves one path shape, it
/// lives for the length of one order, and the request it answers is a bare
/// `GET` from Let's Encrypt with no features worth supporting.
async fn serve_challenges(
    port: u16,
    responses: Vec<(String, String)>,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("the challenge listener could not bind :{port}: {e}"))?;

    Ok(tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let responses = responses.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                let Ok(read) = stream.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();

                let answer = path
                    .rsplit_once('/')
                    .map(|(_, token)| token)
                    .and_then(|token| {
                        responses
                            .iter()
                            .find(|(t, _)| t == token)
                            .map(|(_, key)| key.clone())
                    });

                let response = match answer {
                    Some(body) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                    // Anything else on this port is not part of the exchange.
                    None => {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                };
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    }))
}

/// Build the acceptor the listener wraps connections in.
pub fn acceptor(certificate: &Certificate) -> Result<TlsAcceptor, String> {
    let chain: Vec<CertificateDer<'static>> =
        rustls_pemfile_certs(&certificate.chain_pem).map_err(|e| e.to_string())?;
    if chain.is_empty() {
        return Err("the stored certificate chain has no certificates in it".to_string());
    }
    let key = rustls_pemfile_key(&certificate.key_pem)?;

    // rustls 0.23 needs a process-wide crypto provider, and it refuses to pick
    // one when more than one is compiled in — this build has both `ring` and
    // `aws-lc-rs`, pulled by different dependencies. Without this, building a
    // ServerConfig **panics**, and a panic inside a tokio task is caught by the
    // runtime and printed to stderr, which Android discards. The symptom is a
    // certificate that is issued and stored and then never served, with nothing
    // logged either way. Installing it is idempotent and the error means
    // somebody else already did.
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| format!("the certificate and key do not go together: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// PEM parsing, without a crate for it.
///
/// Two block types, both base64 between fixed delimiters. `rustls-pemfile`
/// would do this and is one more dependency on a build that is already the
/// heaviest thing in this crate.
fn rustls_pemfile_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    pem_blocks(pem, "CERTIFICATE")
        .into_iter()
        .map(|der| Ok(CertificateDer::from(der)))
        .collect()
}

fn rustls_pemfile_key(pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    // instant-acme finalises with a PKCS#8 key, which is what "PRIVATE KEY"
    // means. The EC and RSA spellings are accepted too rather than failing on a
    // store written by some future version.
    for label in ["PRIVATE KEY", "EC PRIVATE KEY", "RSA PRIVATE KEY"] {
        if let Some(der) = pem_blocks(pem, label).into_iter().next() {
            return PrivateKeyDer::try_from(der)
                .map_err(|e| format!("the stored private key could not be read: {e}"));
        }
    }
    Err("the stored key file holds no private key".to_string())
}

fn pem_blocks(pem: &str, label: &str) -> Vec<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut blocks = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let Some(finish) = after.find(&end) else {
            break;
        };
        let body: String = after[..finish]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if let Some(der) = base64_decode(&body) {
            blocks.push(der);
        }
        rest = &after[finish + end.len()..];
    }
    blocks
}

/// Standard base64, decode only.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits = 0;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = TABLE.iter().position(|c| *c == byte)? as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_a_known_vector() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
    }

    #[test]
    fn pem_blocks_are_found_in_order_and_ignore_the_rest() {
        let pem = "noise\n-----BEGIN CERTIFICATE-----\naGVsbG8=\n-----END CERTIFICATE-----\n\
                   -----BEGIN CERTIFICATE-----\nYQ==\n-----END CERTIFICATE-----\n";
        let blocks = pem_blocks(pem, "CERTIFICATE");
        assert_eq!(blocks.len(), 2, "a chain is more than one certificate");
        assert_eq!(blocks[0], b"hello");
        assert_eq!(blocks[1], b"a");
    }

    #[test]
    fn a_key_labelled_any_of_the_three_ways_is_found() {
        for label in ["PRIVATE KEY", "EC PRIVATE KEY", "RSA PRIVATE KEY"] {
            let pem = format!("-----BEGIN {label}-----\naGVsbG8=\n-----END {label}-----\n");
            assert_eq!(pem_blocks(&pem, label).len(), 1, "{label}");
        }
    }

    /// A half-written store must read as "order a new one", not as an error
    /// that leaves the device unreachable until someone clears it by hand.
    #[test]
    fn a_store_missing_any_piece_reads_as_empty() {
        let dir = std::env::temp_dir().join(format!("homerun-certstore-{}", now()));
        let store = CertStore::new(&dir);
        assert!(store.load().is_none(), "nothing written yet");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chain.pem"), "x").unwrap();
        assert!(store.load().is_none(), "a chain with no key is not usable");

        std::fs::write(dir.join("key.pem"), "y").unwrap();
        assert!(
            store.load().is_none(),
            "no issue time means no renewal clock"
        );

        std::fs::write(dir.join("issued-at"), now().to_string()).unwrap();
        assert!(store.load().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_certificate_becomes_stale_at_sixty_days() {
        let fresh = Certificate {
            chain_pem: String::new(),
            key_pem: String::new(),
            issued_at: now(),
        };
        assert!(!fresh.stale());

        let old = Certificate {
            issued_at: now() - RENEW_AFTER.as_secs() - 1,
            ..fresh
        };
        assert!(
            old.stale(),
            "Let's Encrypt issues for 90 days; renewing at 60 leaves margin for a phone \
             that is not opened for a fortnight"
        );
    }
}

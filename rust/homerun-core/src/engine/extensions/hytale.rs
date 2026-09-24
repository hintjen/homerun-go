//! Hytale's server sign-in — the pure half.
//!
//! A Hytale server admits nobody until it is authenticated to the hosting
//! person's Hytale account. Hytale's Server Provider Authentication Guide
//! describes the way to do that without the person ever typing into a server
//! console, and this extension follows it:
//!
//! 1. **Sign in once per machine** with the OAuth 2.0 device flow (RFC 8628)
//!    against client `hytale-server`: the person approves a code in their own
//!    browser; the refresh token that comes back is kept sealed on this
//!    computer and never leaves it.
//! 2. **At every start**, refresh (the refresh token rotates: every refresh
//!    returns a new one and invalidates the old), pick the account's game
//!    profile, and create a game session.
//! 3. **Pass the session to the server** through `HYTALE_SERVER_SESSION_TOKEN`
//!    and `HYTALE_SERVER_IDENTITY_TOKEN`, plus `--owner-uuid`, so it starts
//!    already signed in. The server refreshes that session itself while it
//!    runs.
//! 4. **At stop**, end the game session, so they do not pile up towards the
//!    account's limit of 500.
//!
//! If the server nonetheless reports it is not signed in -- the session could
//! not be refreshed while it ran -- the extension falls back to Hytale's own
//! console flow (`auth login device`), the one closed PR #60 drove.
//!
//! Everything here is a decision about what Hytale's service or server said;
//! the requests themselves are the runner's (`homerun-game-cli`
//! `extensions/hytale.rs`).

use serde_json::{json, Value};

use super::{ExtensionSpec, Supply};
use crate::engine::descriptor::GameDescriptor;
use crate::engine::fetch::{sign_in_host, vendor_scheme_allowed};
use crate::engine::validate::Report;

pub const SPEC: ExtensionSpec = ExtensionSpec {
    name: "hytale",
    supplies: &[
        Supply {
            key: "sessionToken",
            secret: true,
        },
        Supply {
            key: "identityToken",
            secret: true,
        },
        Supply {
            key: "ownerUuid",
            secret: false,
        },
    ],
    validate,
    hosts,
    config_schema,
};

/// The config keys that are addresses of Hytale's services.
const URLS: [&str; 4] = ["deviceUrl", "tokenUrl", "profilesUrl", "sessionUrl"];

/// The device grant's `grant_type`, from RFC 8628.
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The version of the document kept in the machine store.
pub const STORE_VERSION: u64 = 1;

// ─── the descriptor's config ───────────────────────────────────────────────

fn validate(config: &Value, _descriptor: &GameDescriptor, report: &mut Report) {
    for key in ["clientId", "scope"] {
        if text(config, key).is_empty() {
            report
                .problems
                .push(format!("the Hytale extension needs its \"{key}\"."));
        }
    }
    for key in URLS {
        let url = text(config, key);
        if url.is_empty() {
            report
                .problems
                .push(format!("the Hytale extension needs its \"{key}\"."));
        } else if !vendor_scheme_allowed(url)
            || (url.starts_with("https://") && url_host(url).is_none())
        {
            report.problems.push(format!(
                "the Hytale extension's \"{key}\" has to be an https:// address with a plain \
                 host name, and \"{url}\" is not."
            ));
        }
    }
    let extra = text(config, "signInHost");
    if !extra.is_empty() && sign_in_host(extra).as_deref() != Some(extra) {
        report.problems.push(format!(
            "the Hytale extension's \"signInHost\" has to be a plain host name, and \
             \"{extra}\" is not."
        ));
    }
    if let Some(console) = config.get("consoleSignIn").filter(|v| !v.is_null()) {
        for key in ["needed", "command", "url", "done"] {
            if console
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .is_empty()
            {
                report.problems.push(format!(
                    "the Hytale extension's console sign-in needs its \"{key}\"."
                ));
            }
        }
        let command = console
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if command.contains(['\n', '\r', '{']) {
            report.problems.push(
                "the Hytale extension's console sign-in command has to be one fixed line.".into(),
            );
        }
        let marker = console
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !marker.is_empty() && sign_in_host(marker).is_none() {
            report.problems.push(format!(
                "the Hytale extension's console sign-in marker \"{marker}\" has to start with \
                 the sign-in site's host name."
            ));
        }
    }
}

/// Every host the extension may reach, and send a person to.
fn hosts(config: &Value) -> Vec<String> {
    let mut hosts: Vec<String> = URLS
        .iter()
        .filter_map(|key| url_host(text(config, key)))
        .collect();
    if let Some(marker) = config
        .get("consoleSignIn")
        .and_then(|c| c.get("url"))
        .and_then(Value::as_str)
    {
        hosts.extend(sign_in_host(marker));
    }
    // Where the device flow sends a person may be a parent of the endpoints'
    // hosts (Hytale's guide shows accounts.hytale.com/device beside
    // oauth.accounts.hytale.com); `signInHost` names it.
    hosts.extend(sign_in_host(text(config, "signInHost")));
    hosts.sort();
    hosts.dedup();
    hosts
}

fn config_schema() -> Value {
    let url = json!({ "type": "string", "pattern": "^https://" });
    json!({
        "type": "object",
        "required": ["clientId", "scope", "deviceUrl", "tokenUrl", "profilesUrl", "sessionUrl"],
        "properties": {
            "clientId": { "type": "string", "description": "The OAuth client, hytale-server." },
            "scope": { "type": "string", "description": "openid offline auth:server" },
            "deviceUrl": url.clone(),
            "tokenUrl": url.clone(),
            "profilesUrl": url.clone(),
            "signInHost": {
                "type": "string",
                "description": "A host whose subdomains may be shown as sign-in links, e.g. hytale.com."
            },
            "sessionUrl": {
                "type": "string", "pattern": "^https://",
                "description": "The game-session resource: /new is appended to create one; DELETE ends it."
            },
            "consoleSignIn": {
                "type": ["object", "null"],
                "description":
                    "The server's own console sign-in, used only if it reports it is not \
                     signed in. Substrings of its console lines.",
                "properties": {
                    "needed": { "type": "string" },
                    "command": { "type": "string" },
                    "url": { "type": "string" },
                    "code": { "type": "string" },
                    "done": { "type": "string" }
                }
            }
        }
    })
}

/// A config string, or `""`.
pub fn text<'a>(config: &'a Value, key: &str) -> &'a str {
    config.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The host of an `https://` address: no userinfo, no port, a dotted name.
fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    sign_in_host(authority).filter(|host| host == &authority.to_ascii_lowercase())
}

// ─── what Hytale's services answer ─────────────────────────────────────────

/// The device authorization endpoint's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    /// The address to show: the one with the code in it when there is one.
    pub url: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// Read a device authorization answer. `None` if it is not one.
pub fn device_code(body: &Value) -> Option<DeviceCode> {
    let field = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    Some(DeviceCode {
        device_code: field("device_code")?.into(),
        user_code: field("user_code")?.into(),
        url: field("verification_uri_complete")
            .or_else(|| field("verification_uri"))?
            .into(),
        // RFC 8628's defaults, and a floor: a server that says 0 is not
        // asking to be polled in a tight loop.
        expires_in: body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900),
        interval: body
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1),
    })
}

/// What the token endpoint said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenReply {
    /// New tokens. The refresh token replaces the stored one at once: the
    /// old one no longer works.
    Tokens { access: String, refresh: String },
    /// The person has not approved the code yet: ask again after `interval`.
    Pending,
    /// Asked too often: ask less often (RFC 8628 adds five seconds).
    SlowDown,
    /// The code ran out before anyone approved it.
    Expired,
    /// The person declined.
    Denied,
    /// The refresh token is no good any more -- used, revoked, or 30 days
    /// unused. Forget it and sign in again.
    InvalidGrant,
    /// Anything else: the service is having trouble.
    Trouble,
}

/// Read a token endpoint answer, from its status and body.
pub fn token_reply(status: u16, body: &Value) -> TokenReply {
    if (200..300).contains(&status) {
        let field = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        return match (field("access_token"), field("refresh_token")) {
            (Some(access), Some(refresh)) => TokenReply::Tokens {
                access: access.into(),
                refresh: refresh.into(),
            },
            _ => TokenReply::Trouble,
        };
    }
    match body.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => TokenReply::Pending,
        Some("slow_down") => TokenReply::SlowDown,
        Some("expired_token") => TokenReply::Expired,
        Some("access_denied") => TokenReply::Denied,
        Some("invalid_grant") => TokenReply::InvalidGrant,
        _ => TokenReply::Trouble,
    }
}

/// One of the account's game profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub uuid: String,
    pub username: String,
}

/// Read the profiles endpoint's answer. `None` if it is not one.
pub fn profiles(body: &Value) -> Option<Vec<Profile>> {
    body.get("profiles")?
        .as_array()?
        .iter()
        .map(|p| {
            Some(Profile {
                uuid: p.get("uuid")?.as_str()?.to_string(),
                username: p
                    .get("username")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

/// Which profile hosts this server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileChoice {
    Use(String),
    /// More than one, and none remembered for this server: ask the person.
    Ask(Vec<Profile>),
    /// The account has no game profile at all.
    NoProfile,
}

/// Decide the profile: the one remembered for this server if the account
/// still has it, else the only one, else ask.
pub fn choose_profile(profiles: &[Profile], remembered: Option<&str>) -> ProfileChoice {
    if let Some(uuid) = remembered.filter(|u| profiles.iter().any(|p| p.uuid == *u)) {
        return ProfileChoice::Use(uuid.to_string());
    }
    match profiles {
        [] => ProfileChoice::NoProfile,
        [only] => ProfileChoice::Use(only.uuid.clone()),
        many => ProfileChoice::Ask(many.to_vec()),
    }
}

/// A game session: what the server is started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub session_token: String,
    pub identity_token: String,
}

/// Read a new game session. `None` if it is not one.
pub fn session(body: &Value) -> Option<Session> {
    let field = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    Some(Session {
        session_token: field("sessionToken")?.into(),
        identity_token: field("identityToken")?.into(),
    })
}

/// Why Hytale refused to open a game session, for a player. `403` is either
/// an account that does not own Hytale or one at its 500-session limit; the
/// body is read for which, and the message covers both when it cannot tell.
pub fn session_refusal(status: u16, body: &str) -> Option<&'static str> {
    if status != 403 {
        return None;
    }
    let body = body.to_ascii_lowercase();
    Some(if body.contains("limit") || body.contains("too many") {
        "This Hytale account already has as many servers running as it is allowed. Stop one, \
         then try again."
    } else if body.contains("entitle") || body.contains("own") {
        "This Hytale account does not own Hytale, so it cannot host a server."
    } else {
        "This Hytale account cannot host a server right now: it may not own Hytale, or may \
         already have as many servers running as it is allowed."
    })
}

// ─── what is kept between runs ─────────────────────────────────────────────

/// The machine store's document: the one refresh token, and nothing else.
pub fn stored(refresh: &str) -> Value {
    json!({ "version": STORE_VERSION, "refreshToken": refresh })
}

/// The refresh token a machine store document holds, if it is one this build
/// can read. An unknown version is treated as nothing kept: the person signs
/// in again rather than the start failing.
pub fn stored_refresh(document: Option<&Value>) -> Option<String> {
    let document = document?;
    if document.get("version").and_then(Value::as_u64) != Some(STORE_VERSION) {
        return None;
    }
    document
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Value {
        json!({
            "clientId": "hytale-server",
            "scope": "openid offline auth:server",
            "deviceUrl": "https://oauth.accounts.hytale.com/oauth2/device/auth",
            "tokenUrl": "https://oauth.accounts.hytale.com/oauth2/token",
            "profilesUrl": "https://account-data.hytale.com/my-account/get-profiles",
            "sessionUrl": "https://sessions.hytale.com/game-session",
            "signInHost": "hytale.com",
            "consoleSignIn": {
                "needed": "No server tokens configured",
                "command": "auth login device",
                "url": "oauth.accounts.hytale.com/oauth2/device/verify",
                "code": "Enter code: ",
                "done": "Authentication successful!"
            }
        })
    }

    fn problems(config: Value) -> Vec<String> {
        let mut report = Report::default();
        validate(&config, &GameDescriptor::default(), &mut report);
        report.problems
    }

    #[test]
    fn the_guides_endpoints_are_a_valid_config() {
        assert_eq!(problems(config()), Vec::<String>::new());
    }

    #[test]
    fn every_endpoint_is_needed_and_has_to_be_https_on_a_plain_host() {
        for key in URLS {
            let mut c = config();
            c[key] = json!("");
            assert!(problems(c).iter().any(|p| p.contains(key)), "{key}");
            for bad in [
                "http://oauth.accounts.hytale.com/x",
                "https://user@oauth.accounts.hytale.com/x",
                "https://oauth.accounts.hytale.com:8443/x",
                "ftp://x.example/y",
            ] {
                let mut c = config();
                c[key] = json!(bad);
                assert!(
                    problems(c).iter().any(|p| p.contains("https:// address")),
                    "{key} = {bad}"
                );
            }
        }
    }

    #[test]
    fn the_console_fallback_is_optional_but_whole_when_present() {
        let mut c = config();
        c["consoleSignIn"] = Value::Null;
        assert_eq!(problems(c), Vec::<String>::new());
        let mut c = config();
        c["consoleSignIn"]["done"] = json!("");
        assert!(problems(c).iter().any(|p| p.contains("\"done\"")));
        let mut c = config();
        c["consoleSignIn"]["command"] = json!("auth login device\nstop");
        assert!(problems(c).iter().any(|p| p.contains("one fixed line")));
        let mut c = config();
        c["consoleSignIn"]["url"] = json!("oauth2/device/verify");
        assert!(problems(c).iter().any(|p| p.contains("host name")));
    }

    #[test]
    fn the_hosts_are_the_endpoints_and_the_console_markers() {
        assert_eq!(
            hosts(&config()),
            vec![
                "account-data.hytale.com".to_string(),
                "hytale.com".to_string(),
                "oauth.accounts.hytale.com".to_string(),
                "sessions.hytale.com".to_string(),
            ]
        );
        let mut c = config();
        c["signInHost"] = json!("https://hytale.com");
        assert!(problems(c).iter().any(|p| p.contains("signInHost")));
    }

    #[test]
    fn a_device_code_prefers_the_address_with_the_code_in_it() {
        let code = device_code(&json!({
            "device_code": "dc", "user_code": "ABCD-1234",
            "verification_uri": "https://accounts.hytale.com/device",
            "verification_uri_complete": "https://accounts.hytale.com/device?user_code=ABCD-1234",
            "expires_in": 900, "interval": 5
        }))
        .unwrap();
        assert_eq!(
            code.url,
            "https://accounts.hytale.com/device?user_code=ABCD-1234"
        );
        assert_eq!((code.expires_in, code.interval), (900, 5));

        let bare = device_code(&json!({
            "device_code": "dc", "user_code": "U",
            "verification_uri": "https://accounts.hytale.com/device", "interval": 0
        }))
        .unwrap();
        assert_eq!(bare.url, "https://accounts.hytale.com/device");
        assert_eq!(
            (bare.expires_in, bare.interval),
            (900, 1),
            "defaults, and a floor"
        );
        assert_eq!(device_code(&json!({ "user_code": "U" })), None);
    }

    #[test]
    fn a_token_reply_is_read_by_rfc_8628s_error_codes() {
        assert_eq!(
            token_reply(200, &json!({ "access_token": "a", "refresh_token": "r" })),
            TokenReply::Tokens {
                access: "a".into(),
                refresh: "r".into()
            }
        );
        assert_eq!(
            token_reply(200, &json!({ "access_token": "a" })),
            TokenReply::Trouble
        );
        for (error, reply) in [
            ("authorization_pending", TokenReply::Pending),
            ("slow_down", TokenReply::SlowDown),
            ("expired_token", TokenReply::Expired),
            ("access_denied", TokenReply::Denied),
            ("invalid_grant", TokenReply::InvalidGrant),
            ("server_error", TokenReply::Trouble),
        ] {
            assert_eq!(
                token_reply(400, &json!({ "error": error })),
                reply,
                "{error}"
            );
        }
        assert_eq!(token_reply(502, &Value::Null), TokenReply::Trouble);
    }

    #[test]
    fn a_profile_is_remembered_used_alone_or_asked_for() {
        let p = |uuid: &str| Profile {
            uuid: uuid.into(),
            username: format!("user-{uuid}"),
        };
        assert_eq!(
            choose_profile(&[p("a")], None),
            ProfileChoice::Use("a".into())
        );
        assert_eq!(
            choose_profile(&[p("a"), p("b")], Some("b")),
            ProfileChoice::Use("b".into())
        );
        assert_eq!(
            choose_profile(&[p("a"), p("b")], Some("gone")),
            ProfileChoice::Ask(vec![p("a"), p("b")]),
            "a remembered profile the account no longer has is asked again"
        );
        assert_eq!(choose_profile(&[], None), ProfileChoice::NoProfile);
        assert_eq!(
            profiles(&json!({ "owner": "o", "profiles": [{ "uuid": "a", "username": "Op" }] })),
            Some(vec![Profile {
                uuid: "a".into(),
                username: "Op".into()
            }])
        );
        assert_eq!(
            profiles(&json!({ "profiles": [{ "username": "no uuid" }] })),
            None
        );
    }

    #[test]
    fn a_session_needs_both_tokens() {
        assert_eq!(
            session(&json!({ "sessionToken": "s", "identityToken": "i", "expiresAt": "x" })),
            Some(Session {
                session_token: "s".into(),
                identity_token: "i".into()
            })
        );
        assert_eq!(session(&json!({ "sessionToken": "s" })), None);
    }

    #[test]
    fn a_refused_session_is_explained_to_a_player() {
        assert_eq!(session_refusal(500, ""), None);
        assert!(session_refusal(403, "session limit reached")
            .unwrap()
            .contains("as many servers"));
        assert!(session_refusal(403, "missing entitlement")
            .unwrap()
            .contains("does not own"));
        assert!(session_refusal(403, "").unwrap().contains("may not own"));
    }

    #[test]
    fn only_a_document_this_build_wrote_gives_a_refresh_token() {
        assert_eq!(stored_refresh(Some(&stored("r1"))).as_deref(), Some("r1"));
        assert_eq!(stored_refresh(None), None);
        assert_eq!(
            stored_refresh(Some(&json!({ "version": 2, "refreshToken": "r" }))),
            None
        );
        assert_eq!(
            stored_refresh(Some(&json!({ "version": 1, "refreshToken": "" }))),
            None
        );
    }
}

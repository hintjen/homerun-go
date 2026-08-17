//! Signing in to a Microsoft account, and turning that into a Minecraft identity.
//!
//! # Why a phone needs this at all
//!
//! Minigame stats are keyed on a Minecraft uuid. Every stats read takes one as
//! input, and the only way to learn one is a Microsoft sign-in — which no
//! mobile host could perform, so a phone's Minigames Hub was structurally stuck
//! at zero stats. The API can now report an account linked from the desktop
//! (`GET /api/minecraft-account/`), which covers the common case without any of
//! this; what follows is for the person whose only Homerun device is a phone.
//!
//! # The chain, and why it is in the core
//!
//! Six steps, five of them network calls whose request bodies and response
//! shapes are fiddly in specific ways that are wrong-by-default:
//!
//! 1. Microsoft OAuth — get an MSA access token
//! 2. Xbox Live — `user.auth.xboxlive.com/user/authenticate`
//! 3. XSTS — `xsts.auth.xboxlive.com/xsts/authorize`, which is where `xuid`
//!    and every account-restriction error come from
//! 4. Minecraft services — `login_with_xbox`
//! 5. Profile — the uuid and the name
//!
//! The desktop derived all of it from Modrinth's `minecraft_auth.rs` and its
//! header lists the traps: the `d=` prefix on the RPS ticket, the
//! `rp://api.minecraftservices.com/` relying party, the `XBL3.0 x=<uhs>;<token>`
//! identity token, `login.live.com` rather than `login.microsoftonline.com`.
//! Every one of those is a silent failure if a second implementation gets it
//! wrong, and there would have been two more of them — Android and iOS. So the
//! shapes live here and the hosts move bytes, exactly as [`super::mods`] does.
//!
//! # Step 1 is deliberately replaceable
//!
//! The other five are fixed by Microsoft. How you get the *first* token is not,
//! and Homerun has two options with different trade-offs:
//!
//! - **Device code** ([`device_code_request`], [`poll_request`]) — what ships.
//!   It works with the public Xbox client below, needs no app registration, no
//!   redirect URI and no embedded web view. The user approves a short code in
//!   their real browser.
//! - **Authorization code + redirect** ([`redeem_request`]) — one tap and no
//!   code to read, but it needs an app registration that Microsoft has approved
//!   for the Minecraft API. A plain registration is not enough: `login_with_xbox`
//!   refuses it.
//!
//! Both produce the same MSA token and everything downstream is untouched,
//! which is why they are two small functions rather than two code paths.
//!
//! # Nothing here is stored, logged, or returned to JavaScript
//!
//! This module hands back tokens because the host has to send them upstream.
//! Where they are kept is the host's problem and the answer is platform
//! storage — Keystore, Keychain. See [`Session::redacted`] for what may cross
//! into a web view, and why it is not the tokens.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{Error, Result};

/// The Xbox app's own client id. Public, secretless, and already approved for
/// the Minecraft API — which is the property that matters and the reason the
/// desktop uses it rather than a Homerun registration.
pub const CLIENT_ID: &str = "00000000402b5328";

/// Consumer sign-in, not the AAD endpoint. `login.microsoftonline.com` looks
/// interchangeable and is not: this client is unknown there.
const AUTH_HOST: &str = "https://login.live.com";

/// What Xbox Live needs, plus a refresh token so a session outlives its hour.
const SCOPE: &str = "XboxLive.signin offline_access";

/// The redirect the desktop registers. Only used by [`redeem_request`], and
/// only meaningful for a host that can observe the navigation.
pub const DESKTOP_REDIRECT_URI: &str = "https://login.live.com/oauth20_desktop.srf";

/// One HTTP call, described. The host performs it; nothing here can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    /// Header name/value pairs, in order.
    pub headers: Vec<(String, String)>,
    /// Already encoded — form or JSON, per the `Content-Type` in `headers`.
    pub body: Option<String>,
}

impl HttpRequest {
    fn form(url: &str, fields: &[(&str, &str)]) -> Self {
        let body = fields
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        Self {
            method: "POST".into(),
            url: url.into(),
            headers: vec![
                (
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                ),
                ("Accept".into(), "application/json".into()),
            ],
            body: Some(body),
        }
    }

    fn json(url: &str, body: Value, extra: &[(&str, &str)]) -> Self {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "application/json".into()),
        ];
        headers.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        Self {
            method: "POST".into(),
            url: url.into(),
            headers,
            body: Some(body.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Step 1a — device code
// ---------------------------------------------------------------------------

/// A pending device-code sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCode {
    /// Shown to the user, and pre-filled into [`DeviceCode::approval_url`].
    pub user_code: String,
    /// Sent back when polling. Not for display — it is the secret half.
    pub device_code: String,
    /// Where Microsoft says to approve. Kept as returned rather than assumed.
    pub verification_uri: String,
    /// Seconds between polls. Microsoft asks for five and means it.
    pub interval_secs: u64,
    /// Seconds until the code stops working. Fifteen minutes, in practice.
    pub expires_in_secs: u64,
}

impl DeviceCode {
    /// The page to open, with the code already filled in.
    ///
    /// The user still has to confirm, but not to read eight characters off one
    /// screen and type them into another — which on a phone means switching
    /// apps twice with the code held in their head.
    pub fn approval_url(&self) -> String {
        format!(
            "{}?otc={}",
            self.verification_uri,
            urlencode(&self.user_code)
        )
    }
}

/// Ask Microsoft to start a device-code sign-in.
pub fn device_code_request() -> HttpRequest {
    HttpRequest::form(
        &format!("{AUTH_HOST}/oauth20_connect.srf"),
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("response_type", "device_code"),
        ],
    )
}

/// Read the device code out of that response.
pub fn device_code_from(body: &Value) -> Result<DeviceCode> {
    Ok(DeviceCode {
        user_code: text(body, "user_code")?,
        device_code: text(body, "device_code")?,
        verification_uri: body
            .get("verification_uri")
            .and_then(Value::as_str)
            .unwrap_or("https://www.microsoft.com/link")
            .to_string(),
        // Microsoft sends these as numbers; a missing one is not worth failing
        // a sign-in over, so both fall back to the documented defaults.
        interval_secs: body.get("interval").and_then(Value::as_u64).unwrap_or(5),
        expires_in_secs: body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900),
    })
}

/// Ask whether the user has approved yet.
pub fn poll_request(device_code: &str) -> HttpRequest {
    HttpRequest::form(
        &format!("{AUTH_HOST}/oauth20_token.srf"),
        &[
            ("client_id", CLIENT_ID),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
        ],
    )
}

/// What one poll meant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Poll {
    /// Nobody has approved it yet. Keep waiting.
    Pending,
    /// Polling too fast. Add a second, per RFC 8628, and keep waiting.
    SlowDown,
    /// The user said no.
    Declined,
    /// The code timed out. A new sign-in has to start from the beginning.
    Expired,
    /// Approved.
    Approved(MsaTokens),
}

/// Microsoft's answer to the sign-in itself, before any Xbox call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MsaTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds. Turned into an absolute time by the host, which has a clock.
    pub expires_in_secs: u64,
}

/// Interpret a poll response.
///
/// The pending states arrive as HTTP 400 with an `error` field, so a host that
/// treats non-2xx as failure would report a sign-in as broken every five
/// seconds while it was working perfectly. Pass the body either way.
pub fn poll_outcome(body: &Value) -> Result<Poll> {
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        return Ok(match error {
            "authorization_pending" => Poll::Pending,
            "slow_down" => Poll::SlowDown,
            "authorization_declined" | "access_denied" => Poll::Declined,
            "expired_token" | "code_expired" => Poll::Expired,
            other => {
                return Err(Error::Malformed(format!(
                    "Microsoft refused the sign-in ({other})."
                )))
            }
        });
    }
    Ok(Poll::Approved(msa_tokens_from(body)?))
}

// ---------------------------------------------------------------------------
// Step 1b — authorization code, for a host that can take a redirect
// ---------------------------------------------------------------------------

/// The URL to send a browser to, for the redirect flow.
///
/// Unused by the shipping device-code path. Kept beside it because the two are
/// alternatives for the same step and the second is a live option the moment an
/// approved app registration exists — see this module's header.
pub fn authorize_url(client_id: &str, redirect_uri: &str) -> String {
    format!(
        "{AUTH_HOST}/oauth20_authorize.srf?client_id={}&response_type=code&redirect_uri={}&scope={}&prompt=select_account",
        urlencode(client_id),
        urlencode(redirect_uri),
        urlencode(SCOPE),
    )
}

/// Exchange an authorization code for tokens.
pub fn redeem_request(client_id: &str, redirect_uri: &str, code: &str) -> HttpRequest {
    HttpRequest::form(
        &format!("{AUTH_HOST}/oauth20_token.srf"),
        &[
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", SCOPE),
            ("grant_type", "authorization_code"),
            ("code", code),
        ],
    )
}

/// Trade a refresh token for a fresh access token.
pub fn refresh_request(refresh_token: &str) -> HttpRequest {
    HttpRequest::form(
        &format!("{AUTH_HOST}/oauth20_token.srf"),
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )
}

/// Read an MSA token response.
pub fn msa_tokens_from(body: &Value) -> Result<MsaTokens> {
    Ok(MsaTokens {
        access_token: text(body, "access_token")?,
        refresh_token: text(body, "refresh_token")?,
        expires_in_secs: body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(3600),
    })
}

// ---------------------------------------------------------------------------
// Steps 2–5 — Xbox Live, XSTS, Minecraft
// ---------------------------------------------------------------------------

/// An Xbox token and the user hash that has to travel with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XboxToken {
    pub token: String,
    /// `uhs`, from the display claims. Half of the Minecraft identity token.
    pub user_hash: String,
}

/// Authenticate the MSA token with Xbox Live.
pub fn xbl_request(msa_access_token: &str) -> HttpRequest {
    HttpRequest::json(
        "https://user.auth.xboxlive.com/user/authenticate",
        json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                // The `d=` prefix is required and its absence fails in a way
                // that does not mention it.
                "RpsTicket": format!("d={msa_access_token}"),
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        }),
        &[("x-xbl-contract-version", "1")],
    )
}

/// Authorize that Xbox token for Minecraft specifically.
pub fn xsts_request(xbl_token: &str) -> HttpRequest {
    HttpRequest::json(
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            // Minecraft's relying party. `http://xboxlive.com` yields a token
            // that authenticates fine and that Minecraft will not accept.
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT",
        }),
        &[("x-xbl-contract-version", "1")],
    )
}

/// Read an Xbox Live or XSTS response.
pub fn xbox_token_from(body: &Value) -> Result<XboxToken> {
    let user_hash = body
        .get("DisplayClaims")
        .and_then(|c| c.get("xui"))
        .and_then(Value::as_array)
        .and_then(|xui| xui.first())
        .and_then(|claim| claim.get("uhs"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Malformed("Xbox did not return a user hash.".into()))?;

    Ok(XboxToken {
        token: text(body, "Token")?,
        user_hash: user_hash.to_string(),
    })
}

/// Turn an XSTS refusal into something a player can act on.
///
/// XSTS answers `401` with an `XErr` for every account-shaped problem — no Xbox
/// profile, a child account, a region that needs age verification. These are
/// the most common way a real sign-in fails and none of them is a bug, so each
/// gets a sentence naming what to go and do. Anything else is reported with its
/// code rather than swallowed.
pub fn xsts_refusal(body: &Value) -> String {
    let code = body.get("XErr").and_then(Value::as_u64);
    match code {
        Some(2148916227) => "This Microsoft account has been suspended.".into(),
        Some(2148916229) => "This Microsoft account is not permitted to play online.".into(),
        Some(2148916233) => {
            "This Microsoft account has no Xbox profile. Create one at xbox.com and try again."
                .into()
        }
        Some(2148916234) => "You need to accept the Xbox Terms of Service first.".into(),
        Some(2148916235) => "Xbox Live is not available in this account's region.".into(),
        Some(2148916238) => {
            "This is a child account. Add it to a family group at xbox.com to sign in.".into()
        }
        Some(2148916222) => {
            "This account's region requires age verification. Complete it at account.xbox.com."
                .into()
        }
        Some(2148916223) => "This is a child account without parental approval for Xbox.".into(),
        Some(2148916236) | Some(2148916237) => {
            "This account requires adult verification before it can sign in.".into()
        }
        Some(other) => format!("Xbox refused the sign-in (error {other})."),
        None => body
            .get("Message")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .unwrap_or("Xbox refused the sign-in.")
            .to_string(),
    }
}

/// Exchange the XSTS token for a Minecraft one.
pub fn minecraft_login_request(xsts: &XboxToken) -> HttpRequest {
    HttpRequest::json(
        "https://api.minecraftservices.com/authentication/login_with_xbox",
        json!({
            "identityToken": format!("XBL3.0 x={};{}", xsts.user_hash, xsts.token),
        }),
        &[],
    )
}

/// Read the Minecraft access token.
pub fn minecraft_token_from(body: &Value) -> Result<String> {
    text(body, "access_token")
}

/// Fetch the profile the uuid comes from.
pub fn profile_request(minecraft_token: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".into(),
        url: "https://api.minecraftservices.com/minecraft/profile".into(),
        headers: vec![
            ("Accept".into(), "application/json".into()),
            ("Authorization".into(), format!("Bearer {minecraft_token}")),
        ],
        body: None,
    }
}

// ---------------------------------------------------------------------------
// The result
// ---------------------------------------------------------------------------

/// A signed-in Minecraft account, as the host stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub username: String,
    /// Dashed, because that is what the stats API is keyed on.
    pub uuid: String,
    pub xuid: Option<String>,
    /// The Minecraft token. Never leaves native storage — see [`Session::redacted`].
    pub access_token: String,
    /// The MSA refresh token. Likewise, and this one is the long-lived secret.
    pub refresh_token: String,
    /// Epoch milliseconds.
    pub expires_at: i64,
}

/// Placeholder standing in for a token the web view has no use for.
const REDACTED: &str = "0";

impl Session {
    /// What may cross into JavaScript.
    ///
    /// The bridge type has `accessToken`/`refreshToken` fields because the
    /// desktop's client launcher needs them to actually start a game. No mobile
    /// surface reads any of the three — the phone uses `username`, `uuid` and
    /// `xuid`, and nothing else — so handing a live Minecraft token to a web
    /// view would be exposure bought for nothing. The fields stay present so
    /// the shape still satisfies the contract, holding `"0"`, which is the same
    /// placeholder the desktop already uses for an offline-mode account.
    ///
    /// `expiresAt` is real: it is not a secret, and a consumer that wants to
    /// show "signed in" has a legitimate use for it.
    pub fn redacted(&self) -> Value {
        json!({
            "username": self.username,
            "uuid": self.uuid,
            "xuid": self.xuid,
            "accessToken": REDACTED,
            "refreshToken": REDACTED,
            "expiresAt": self.expires_at,
        })
    }
}

/// Assemble the session from the profile and the tokens that produced it.
///
/// `now_ms` is passed in because this crate has no clock, which is what keeps
/// it deterministic and testable.
pub fn session_from(
    profile: &Value,
    minecraft_token: &str,
    msa: &MsaTokens,
    now_ms: i64,
) -> Result<Session> {
    let raw = profile.get("id").and_then(Value::as_str).ok_or_else(|| {
        // The specific case worth naming: the sign-in worked, the account is
        // real, and it simply does not own Minecraft. Telling somebody their
        // login failed would send them to fix the wrong thing.
        Error::Unsupported("That Microsoft account does not own Minecraft: Java Edition.".into())
    })?;

    Ok(Session {
        username: profile
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        uuid: super::settings::dash_uuid(raw)?,
        xuid: xuid_from_token(minecraft_token),
        access_token: minecraft_token.to_string(),
        refresh_token: msa.refresh_token.clone(),
        expires_at: now_ms + (msa.expires_in_secs as i64) * 1000,
    })
}

/// The Xbox user id, decoded from the Minecraft token's own payload.
///
/// It is in there already, so reading it costs nothing — no extra round trip
/// and no waiting for a refresh. `None` for a token that is not a JWT or does
/// not carry one; the xuid is optional everywhere it is used.
pub fn xuid_from_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url(payload)?;
    let json: Value = serde_json::from_slice(&decoded).ok()?;
    match json.get("xuid")? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Whether a stored session should be refreshed before it is trusted.
///
/// Early by a minute, because the alternative is a token that passes this check
/// and expires in flight — a failure that looks like a broken sign-in rather
/// than an expiry.
pub fn needs_refresh(expires_at: i64, now_ms: i64) -> bool {
    now_ms >= expires_at - 60_000
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn text(body: &Value, key: &str) -> Result<String> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Malformed(format!("the response had no \"{key}\"")))
}

/// Percent-encoding for a form value. Deliberately conservative: everything
/// outside the unreserved set is escaped.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Decode unpadded base64url. Only ever fed a JWT payload.
fn base64url(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in input.bytes() {
        // Padding is legal and carries nothing; JWTs omit it.
        if byte == b'=' {
            break;
        }
        let value = TABLE.iter().position(|c| *c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(request: &HttpRequest, key: &str) -> Option<String> {
        request.body.as_ref()?.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.to_string())
        })
    }

    #[test]
    fn the_device_code_request_is_the_one_microsoft_answered() {
        let request = device_code_request();
        assert_eq!(request.url, "https://login.live.com/oauth20_connect.srf");
        assert_eq!(field(&request, "client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(
            field(&request, "response_type").as_deref(),
            Some("device_code")
        );
        // The scope, percent-encoded — the space is the part that has to survive.
        assert_eq!(
            field(&request, "scope").as_deref(),
            Some("XboxLive.signin%20offline_access"),
        );
    }

    /// Pinned against a real response body, captured from the live endpoint.
    #[test]
    fn a_real_device_code_response_is_read_and_the_code_is_pre_filled() {
        let body = serde_json::json!({
            "user_code": "MWNYUL2R",
            "device_code": "-DkVH0sRqTYN*SRRP0TH2qlZ",
            "verification_uri": "https://www.microsoft.com/link",
            "interval": 5,
            "expires_in": 900,
        });

        let code = device_code_from(&body).unwrap();
        assert_eq!(code.user_code, "MWNYUL2R");
        assert_eq!(code.interval_secs, 5);
        assert_eq!(code.expires_in_secs, 900);
        assert_eq!(
            code.approval_url(),
            "https://www.microsoft.com/link?otc=MWNYUL2R",
        );
    }

    /// The states that arrive as HTTP 400. A host that read the status instead
    /// of the body would call a working sign-in broken every five seconds.
    #[test]
    fn waiting_is_not_failing() {
        let of = |error: &str| poll_outcome(&serde_json::json!({ "error": error })).unwrap();

        assert_eq!(of("authorization_pending"), Poll::Pending);
        assert_eq!(of("slow_down"), Poll::SlowDown);
        assert_eq!(of("authorization_declined"), Poll::Declined);
        assert_eq!(of("expired_token"), Poll::Expired);
    }

    #[test]
    fn an_approved_poll_carries_the_tokens() {
        let outcome = poll_outcome(&serde_json::json!({
            "access_token": "ms-access",
            "refresh_token": "ms-refresh",
            "expires_in": 3600,
        }))
        .unwrap();

        assert_eq!(
            outcome,
            Poll::Approved(MsaTokens {
                access_token: "ms-access".into(),
                refresh_token: "ms-refresh".into(),
                expires_in_secs: 3600,
            })
        );
    }

    #[test]
    fn an_unknown_error_is_reported_rather_than_treated_as_pending() {
        assert!(poll_outcome(&serde_json::json!({ "error": "invalid_client" })).is_err());
    }

    /// Every one of these is a documented trap that fails quietly if wrong.
    #[test]
    fn the_xbox_requests_carry_the_shapes_that_are_easy_to_get_wrong() {
        let xbl = xbl_request("ms-token");
        let body: Value = serde_json::from_str(xbl.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["Properties"]["RpsTicket"], "d=ms-token");
        assert_eq!(body["RelyingParty"], "http://auth.xboxlive.com");
        assert!(xbl
            .headers
            .contains(&("x-xbl-contract-version".into(), "1".into())));

        let xsts = xsts_request("xbl-token");
        let body: Value = serde_json::from_str(xsts.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["RelyingParty"], "rp://api.minecraftservices.com/");
        assert_eq!(body["Properties"]["SandboxId"], "RETAIL");

        let login = minecraft_login_request(&XboxToken {
            token: "xsts-token".into(),
            user_hash: "uhs-value".into(),
        });
        let body: Value = serde_json::from_str(login.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["identityToken"], "XBL3.0 x=uhs-value;xsts-token");
    }

    #[test]
    fn the_user_hash_is_read_out_of_the_display_claims() {
        let token = xbox_token_from(&serde_json::json!({
            "Token": "t",
            "DisplayClaims": { "xui": [ { "uhs": "hash" } ] },
        }))
        .unwrap();
        assert_eq!(token.user_hash, "hash");

        assert!(xbox_token_from(&serde_json::json!({ "Token": "t" })).is_err());
    }

    /// These are the ordinary ways a real sign-in fails, and each one needs to
    /// send the player somewhere different.
    #[test]
    fn every_xbox_refusal_says_what_to_do_about_it() {
        let of = |code: u64| xsts_refusal(&serde_json::json!({ "XErr": code }));

        assert!(of(2148916233).contains("xbox.com"));
        assert!(of(2148916238).contains("family group"));
        assert!(of(2148916235).contains("region"));
        assert!(of(2148916234).contains("Terms of Service"));
        // An unrecognised code still names itself rather than vanishing.
        assert!(of(1234).contains("1234"));
        // No XErr at all: fall back to whatever Microsoft said.
        assert_eq!(
            xsts_refusal(&serde_json::json!({ "Message": "nope" })),
            "nope",
        );
    }

    #[test]
    fn the_profile_request_is_bearer_authorized() {
        let request = profile_request("mc-token");
        assert_eq!(request.method, "GET");
        assert!(request
            .headers
            .contains(&("Authorization".into(), "Bearer mc-token".into())));
        assert!(request.body.is_none());
    }

    /// A JWT payload carrying `{"xuid":"2535428394"}`, base64url and unpadded.
    #[test]
    fn the_xuid_comes_out_of_the_token_that_is_already_here() {
        let token = "header.eyJ4dWlkIjoiMjUzNTQyODM5NCJ9.signature";
        assert_eq!(xuid_from_token(token).as_deref(), Some("2535428394"));

        assert_eq!(xuid_from_token("not-a-jwt"), None);
        assert_eq!(xuid_from_token("a.!!!!.c"), None);
    }

    #[test]
    fn a_session_dashes_the_uuid_the_stats_api_is_keyed_on() {
        let session = session_from(
            &serde_json::json!({ "id": "069a79f444e94726a5befca90e38aaf5", "name": "Notch" }),
            "header.eyJ4dWlkIjoiMjUzNTQyODM5NCJ9.sig",
            &MsaTokens {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_in_secs: 3600,
            },
            1_700_000_000_000,
        )
        .unwrap();

        assert_eq!(session.uuid, "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        assert_eq!(session.username, "Notch");
        assert_eq!(session.xuid.as_deref(), Some("2535428394"));
        assert_eq!(session.expires_at, 1_700_000_000_000 + 3_600_000);
    }

    /// Signing in with an account that has no Minecraft is a real thing people
    /// do, and "login failed" would send them to fix the wrong problem.
    #[test]
    fn an_account_without_minecraft_is_told_so() {
        let error = session_from(
            &serde_json::json!({ "path": "/minecraft/profile", "error": "NOT_FOUND" }),
            "t",
            &MsaTokens {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_in_secs: 1,
            },
            0,
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not own Minecraft"));
    }

    /// The rule this whole type exists to enforce.
    #[test]
    fn no_token_ever_reaches_the_web_view() {
        let session = Session {
            username: "Notch".into(),
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".into(),
            xuid: Some("2535428394".into()),
            access_token: "a-real-minecraft-token".into(),
            refresh_token: "a-real-refresh-token".into(),
            expires_at: 1_700_000_000_000,
        };

        let view = session.redacted();
        let rendered = view.to_string();

        assert!(!rendered.contains("a-real-minecraft-token"));
        assert!(!rendered.contains("a-real-refresh-token"));
        // Present, so the shape still satisfies the bridge contract.
        assert_eq!(view["accessToken"], "0");
        assert_eq!(view["refreshToken"], "0");
        // The parts a phone actually reads survive intact.
        assert_eq!(view["uuid"], "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        assert_eq!(view["username"], "Notch");
        assert_eq!(view["xuid"], "2535428394");
        assert_eq!(view["expiresAt"], 1_700_000_000_000i64);
    }

    #[test]
    fn a_session_is_refreshed_before_it_expires_rather_than_after() {
        let expires_at = 1_000_000i64;
        assert!(!needs_refresh(expires_at, expires_at - 120_000));
        // Inside the last minute: refresh now rather than mid-request.
        assert!(needs_refresh(expires_at, expires_at - 30_000));
        assert!(needs_refresh(expires_at, expires_at + 1));
    }

    /// The redirect flow is not what ships, but it is one function away and a
    /// broken URL would only be found the day somebody switched to it.
    #[test]
    fn the_redirect_flow_is_ready_for_an_approved_app_registration() {
        let url = authorize_url("some-app-id", "homerun://auth/minecraft");
        assert!(url.starts_with("https://login.live.com/oauth20_authorize.srf?"));
        assert!(url.contains("client_id=some-app-id"));
        assert!(url.contains("redirect_uri=homerun%3A%2F%2Fauth%2Fminecraft"));
        assert!(url.contains("response_type=code"));

        let request = redeem_request("some-app-id", "homerun://auth/minecraft", "the-code");
        assert_eq!(
            field(&request, "grant_type").as_deref(),
            Some("authorization_code")
        );
        assert_eq!(field(&request, "code").as_deref(), Some("the-code"));
    }

    #[test]
    fn a_refresh_asks_for_the_same_scope_it_was_granted() {
        let request = refresh_request("stored-refresh");
        assert_eq!(
            field(&request, "grant_type").as_deref(),
            Some("refresh_token")
        );
        assert_eq!(
            field(&request, "refresh_token").as_deref(),
            Some("stored-refresh")
        );
        assert_eq!(field(&request, "client_id").as_deref(), Some(CLIENT_ID));
    }
}

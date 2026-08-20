//! What a host tells the API about a failure it did not expect.
//!
//! # Why this is here rather than in each host
//!
//! Same argument as the rest of [`crate::reporting`], one step further. A
//! crash report describes a Minecraft server; this describes *the app*, and
//! the app is four languages — a React page in a WebView, a Kotlin host, a
//! Swift host, and this crate — that fail in four different ways and, until
//! now, reported none of them. A render throw was a blank screen. A Kotlin
//! crash was a logcat tombstone nobody would ever read. A panic here was
//! written to a file nothing opens.
//!
//! Every decision about such a failure is the same on every platform: whether
//! two of them are the same bug, how often one is worth sending, what must be
//! removed before it leaves the device, and what the payload looks like. Only
//! the request differs, and that is the one thing a phone cannot share.
//!
//! Doing it once also settles the question the other way round: a redaction
//! rule one platform forgot is the same leak, and the leak is silent.
//!
//! ```text
//!   host: a throw, a rejection, a 500, a panic  →  observe(&mut ledger, …)
//!   core:  same bug?  worth sending?  safe to send?
//!                                              →  Request { … } | Hold
//!   host: sign it, send it, forget it
//! ```
//!
//! # Nothing here retries. Dropped reports are counted, not queued.
//!
//! This is the property the whole design rests on, and it is worth stating
//! before the types.
//!
//! A React component that throws on every commit produces roughly 3,600
//! events a minute, on one phone. A queue would faithfully deliver all of
//! them; a retry would deliver them twice. Instead, the first sighting is
//! sent and the rest increment a counter, so a half-hour of that arrives as
//! three reports — at 0, 5 and 15 minutes — the last carrying
//! `occurrences: 36000`.
//!
//! The result is not merely smaller, it is *better*: one row saying eighteen
//! thousand is a legible fact, and eighteen thousand rows is an outage in the
//! error reporter. The same reasoning is why the API failure path exists at
//! all — during a real incident every device is failing against the same API
//! this report is posted to, and a reporter without a ceiling would finish
//! the job the outage started.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::Request;
use crate::reporting::truncate;

mod fingerprint;
mod redact;

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// How long one fingerprint waits before it is worth sending again.
///
/// Doubling from here to [`MAX_COOLDOWN_MS`]. A loop reports at 0, 5, 15, 35
/// and 75 minutes: often enough that a live incident is visible within
/// minutes, sparse enough that a session cannot spend its budget on one bug.
const COOLDOWN_MS: i64 = 5 * 60 * 1_000;
const MAX_COOLDOWN_MS: i64 = 60 * 60 * 1_000;

/// The burst window, and how many *distinct* fingerprints may go out inside
/// one. Cooldown does nothing against a cascade where twenty components each
/// throw differently; this does.
const BURST_WINDOW_MS: i64 = 60 * 1_000;
const BURST_MAX: u32 = 5;

/// The hard ceiling for one process, and the slightly higher one that a
/// never-before-seen fatal may reach.
///
/// The exemption exists because the reports most worth having are the ones
/// that arrive after something has already gone badly wrong for a while, and
/// a flat cap spends its budget on whatever failed first.
const SESSION_MAX: u32 = 20;
const SESSION_HARD_MAX: u32 = 30;

/// How many fingerprints are remembered. Least-recently-seen is evicted.
///
/// An evicted fingerprint coming back reads as new and sends once more. That
/// is acceptable: the burst and session caps both still apply, so the worst
/// case is bounded by them rather than by this.
const MAX_TRACKED: usize = 32;

/// Per-field byte caps. See [`crate::reporting::truncate`] for why `stack`
/// keeps its head while a console log keeps its tail.
const MAX_MESSAGE: usize = 1024;
const MAX_STACK: usize = 8 * 1024;
const MAX_HTTP_BODY: usize = 2 * 1024;
const MAX_LOCATION: usize = 256;
const MAX_KIND: usize = 120;
const MAX_EXTRA: usize = 2 * 1024;

/// The ceiling for the assembled body.
///
/// Asserted rather than assumed: the per-field caps sum to well under this,
/// but they are six numbers that will be edited separately over time and a
/// body that outgrows the endpoint fails at the far end where nobody is
/// looking.
const MAX_BODY: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// What the host saw
// ---------------------------------------------------------------------------

/// Where a failure came from.
///
/// Part of the fingerprint, so the same message from the page and from the
/// host stay separate — they are different bugs with the same words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// The React page: a render throw, a rejected promise, a boot failure.
    #[default]
    Ui,
    /// An API response the client could not use.
    Api,
    /// The Kotlin or Swift host.
    Host,
    /// This crate — a panic.
    Native,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Source::Ui => "ui",
            Source::Api => "api",
            Source::Host => "host",
            Source::Native => "native",
        }
    }
}

/// How bad it was.
///
/// `Fatal` means the surface it happened on stopped working — a render throw
/// caught by an error boundary, an uncaught exception, a panic. `Error` is
/// everything the app carried on after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Fatal,
    #[default]
    Error,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Fatal => "fatal",
            Severity::Error => "error",
        }
    }
}

/// An API response that could not be used.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Http {
    pub method: String,
    /// The URL as the caller had it. The core reduces it to a path shape —
    /// the page is not trusted to template its own routes, and doing it here
    /// is what makes three platforms group the same way.
    pub url: String,
    pub status: u16,
    pub body: Option<String>,
}

/// One thing that went wrong, as the platform saw it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Occurrence {
    pub source: Source,
    pub severity: Severity,
    /// `TypeError`, `kotlin.IllegalStateException`, `panic`, `http`.
    pub kind: String,
    pub message: String,
    pub stack: Option<String>,
    /// A route pattern or a symbol — `/server/[id]`, not a URL with a query.
    pub location: Option<String>,
    pub http: Option<Http>,
    pub extra: Map<String, Value>,
    /// Wall clock, milliseconds. This crate has no clock and will not grow
    /// one: a pure decision function that reads the time is a decision nobody
    /// can write a test for.
    pub at_ms: i64,
}

/// What this install is. Gathered once per session by the host.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Context {
    pub device_id: String,
    /// One id per process, so "this person hit forty errors in one sitting"
    /// is answerable.
    pub session: String,
    pub platform: String,
    pub app_version: String,
    /// The over-the-air UI bundle, or `shipped`. Absent on hosts that do not
    /// replace their UI.
    pub bundle: Option<String>,
    pub ui_version: Option<String>,
    pub host_revision: Option<u32>,
    /// Whichever API this install talks to. [`deployment`] reads it; nothing
    /// else does, and it never enters the body verbatim.
    pub api_url: String,
    pub server_id: Option<String>,
}

/// Which deployment an install is pointed at, from the API it talks to.
///
/// One rule for three platforms, and unfakeable from the page — the host
/// knows its own API URL and passes it in. Deriving it from a second,
/// independent signal is exactly what once sent staging journeys events to
/// production: a staging mobile build is a *production* Next build pointed at
/// a staging API, so anything reading `NODE_ENV` gets the wrong answer.
///
/// Follows `journeysUrlForApi` in the shared UI: the host must start `api.`,
/// and anything else is `unknown` rather than a guess.
pub fn deployment(api_url: &str) -> &'static str {
    let Some(rest) = api_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .or(Some(api_url))
    else {
        return "unknown";
    };
    let host = rest.split(['/', ':']).next().unwrap_or("");

    if !host.starts_with("api.") {
        return "unknown";
    }
    if host == "api.gethomerun.app" {
        "production"
    } else {
        "staging"
    }
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

/// One fingerprint's history within this session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    fingerprint: String,
    first_ms: i64,
    last_ms: i64,
    /// Sightings, ever.
    count: u32,
    /// `count` as of the last send, so the next send reports the difference
    /// rather than the total — the API sums them and would otherwise
    /// double-count every report after the first.
    count_at_send: u32,
    /// `None` until the first send, which is why the first sighting always
    /// goes. Deliberately not a sentinel `0`: that is a real instant, and a
    /// phone whose clock has not been set yet reports exactly that one.
    last_sent_ms: Option<i64>,
    cooldown_ms: i64,
}

/// What has been seen and sent this session.
///
/// # Not persisted, deliberately
///
/// A ledger written to disk would silence a first-launch crash loop on the
/// second launch — precisely the moment the report is worth most. It would
/// also mean file I/O on a path a panic handler can reach, which is a
/// different kind of bad day.
///
/// # One instance per process, not one per caller
///
/// This is the reason the ledger is a parameter rather than something each
/// caller owns. Four producers reach it on different threads: the JVM crash
/// handler, the WebView bridge, the panic hook, and the host's reporting
/// coroutine. Give each its own copy and every cap silently multiplies by
/// four. The FFI crate holds exactly one behind a mutex; this type stays a
/// plain value so the decisions over it are testable without a device.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Ledger {
    /// Most-recently-seen first. Capped at [`MAX_TRACKED`].
    entries: Vec<Entry>,
    sent: u32,
    /// Session total of everything held back, across all fingerprints. Rides
    /// out on every report so the API can see what it is not being told.
    suppressed: u32,
    window_start_ms: i64,
    window_sent: u32,
}

impl Ledger {
    /// Reports sent this session.
    pub fn sent(&self) -> u32 {
        self.sent
    }

    /// Sightings held back this session.
    pub fn suppressed(&self) -> u32 {
        self.suppressed
    }

    /// The entry for `fingerprint`, moved to the front, created if new.
    fn touch(&mut self, fingerprint: &str, now: i64) -> usize {
        if let Some(at) = self
            .entries
            .iter()
            .position(|entry| entry.fingerprint == fingerprint)
        {
            let entry = self.entries.remove(at);
            self.entries.insert(0, entry);
        } else {
            self.entries.insert(
                0,
                Entry {
                    fingerprint: fingerprint.to_string(),
                    first_ms: now,
                    last_ms: now,
                    count: 0,
                    count_at_send: 0,
                    last_sent_ms: None,
                    cooldown_ms: COOLDOWN_MS,
                },
            );
            self.entries.truncate(MAX_TRACKED);
        }
        0
    }
}

/// Why a sighting is not being sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Hold {
    /// This fingerprint went recently.
    Cooldown,
    /// Too many distinct fingerprints in the last minute.
    Burst,
    /// This process has sent its share.
    SessionCap,
    /// Nothing usable in it — no message, no stack, no response.
    Empty,
}

impl Hold {
    pub fn as_str(self) -> &'static str {
        match self {
            Hold::Cooldown => "cooldown",
            Hold::Burst => "burst",
            Hold::SessionCap => "sessionCap",
            Hold::Empty => "empty",
        }
    }
}

/// What to do about one sighting.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Send {
        request: Request,
        fingerprint: String,
    },
    Hold {
        fingerprint: String,
        reason: Hold,
    },
}

// ---------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------

/// Record one sighting and decide whether it is worth sending.
///
/// The only entry point. Mutates the ledger and is otherwise pure: no clock,
/// no I/O, no allocation beyond the body it may build. Every branch below is
/// reachable from a unit test by choosing `seen.at_ms`.
pub fn observe(ledger: &mut Ledger, ctx: &Context, seen: &Occurrence) -> Verdict {
    // Nothing to group on and nothing to read. Held before it can take a slot
    // in the ledger — an empty report would otherwise be one fingerprint that
    // every unrelated empty report joins.
    if seen.message.trim().is_empty() && seen.stack.is_none() && seen.http.is_none() {
        return Verdict::Hold {
            fingerprint: String::new(),
            reason: Hold::Empty,
        };
    }

    let signature = fingerprint::signature(seen);
    let fingerprint = fingerprint::hash(seen.source, &seen.kind, &signature);
    let now = seen.at_ms;

    // Roll the burst window. `now < window_start_ms` covers a clock that went
    // backwards, which a phone's does — NTP, a timezone change, a user
    // setting the date. Treating it as a fresh window is the safe reading:
    // the alternative is a window that never expires.
    if now.saturating_sub(ledger.window_start_ms) >= BURST_WINDOW_MS || now < ledger.window_start_ms
    {
        ledger.window_start_ms = now;
        ledger.window_sent = 0;
    }

    let at = ledger.touch(&fingerprint, now);
    let first_sighting = ledger.entries[at].count == 0;
    ledger.entries[at].count = ledger.entries[at].count.saturating_add(1);
    ledger.entries[at].last_ms = now;

    let hold = |ledger: &mut Ledger, reason: Hold| {
        ledger.suppressed = ledger.suppressed.saturating_add(1);
        Verdict::Hold {
            fingerprint: fingerprint.clone(),
            reason,
        }
    };

    let entry = &ledger.entries[at];
    let due = match entry.last_sent_ms {
        None => true,
        // `now < sent` is a clock that moved backwards; treat it as due
        // rather than let the cooldown run until the clock catches up.
        Some(sent) => now.saturating_sub(sent) >= entry.cooldown_ms || now < sent,
    };
    if !due {
        return hold(ledger, Hold::Cooldown);
    }

    if ledger.window_sent >= BURST_MAX {
        return hold(ledger, Hold::Burst);
    }

    // The exemption: a fatal nobody has seen before still gets out past the
    // ordinary cap, up to the hard one. A session that has already spent
    // twenty reports on a noisy warning would otherwise never mention the
    // crash that followed it.
    let spared = first_sighting && seen.severity == Severity::Fatal;
    let ceiling = if spared {
        SESSION_HARD_MAX
    } else {
        SESSION_MAX
    };
    if ledger.sent >= ceiling {
        return hold(ledger, Hold::SessionCap);
    }

    let entry = &mut ledger.entries[at];
    let occurrences = entry.count - entry.count_at_send;
    let first_seen_ms = entry.first_ms;
    entry.count_at_send = entry.count;
    // Double only when a cooldown was actually served. The first sighting
    // waits for nothing, so doubling after it would charge the second report
    // twice the interval and put the schedule a step ahead of itself.
    if entry.last_sent_ms.is_some() {
        entry.cooldown_ms = (entry.cooldown_ms.saturating_mul(2)).min(MAX_COOLDOWN_MS);
    }
    entry.last_sent_ms = Some(now);

    ledger.sent = ledger.sent.saturating_add(1);
    ledger.window_sent = ledger.window_sent.saturating_add(1);

    let request = build(
        ctx,
        seen,
        &fingerprint,
        &signature,
        occurrences,
        ledger.suppressed,
        first_seen_ms,
    );

    Verdict::Send {
        request,
        fingerprint,
    }
}

/// Assemble the report. Every free-text field is redacted, then truncated.
///
/// That order matters: redaction replaces a long secret with a short marker,
/// so truncating first could cut a token in half and leave the half that is
/// still a token.
#[allow(clippy::too_many_arguments)]
fn build(
    ctx: &Context,
    seen: &Occurrence,
    fingerprint: &str,
    signature: &str,
    occurrences: u32,
    suppressed: u32,
    first_seen_ms: i64,
) -> Request {
    let message = clean(&seen.message, MAX_MESSAGE);
    let stack = seen.stack.as_deref().map(|s| clean(s, MAX_STACK));
    let location = seen.location.as_deref().map(|s| clean(s, MAX_LOCATION));
    let kind = truncate::head(&seen.kind, MAX_KIND);

    let http = seen.http.as_ref().map(|http| {
        json!({
            "method": truncate::head(&http.method, 16),
            // The shape, not the URL: it is what the fingerprint grouped on,
            // and a reviewer comparing a row to its group needs to see the
            // same thing the grouping saw.
            "path": fingerprint::path_shape(&http.url),
            "status": http.status,
            "body": http.body.as_deref().map(|b| clean(b, MAX_HTTP_BODY)),
        })
    });

    let mut body = json!({
        "device": ctx.device_id,
        "session": ctx.session,
        "fingerprint": fingerprint,
        "signature": signature,
        "source": seen.source.as_str(),
        "severity": seen.severity.as_str(),
        "kind": kind,
        "message": message,
        "stack": stack,
        "location": location,
        "occurrences": occurrences,
        "suppressed": suppressed,
        "firstSeenMs": first_seen_ms,
        "lastSeenMs": seen.at_ms,
        "http": http,
        "platform": ctx.platform,
        "appVersion": ctx.app_version,
        "bundle": ctx.bundle,
        "uiVersion": ctx.ui_version,
        "hostRevision": ctx.host_revision,
        "deployment": deployment(&ctx.api_url),
        "server": ctx.server_id,
        "extra": extra(&seen.extra),
    });

    fit(&mut body);
    Request::post("/api/app-error/", body)
}

/// Redact, then cap. Both, always — a field that skips either is the one that
/// leaks or the one that blows the body budget.
fn clean(text: &str, max: usize) -> String {
    truncate::head(&redact::text(text), max)
}

/// The caller's extra fields, or nothing.
///
/// All-or-nothing on the size check, unlike every other field. A truncated
/// JSON object is not a smaller object, it is a broken one, and half a map
/// read as a whole map is a lie a reviewer cannot see through.
fn extra(extra: &Map<String, Value>) -> Value {
    if extra.is_empty() {
        return json!({});
    }

    let redacted: Map<String, Value> = extra
        .iter()
        .map(|(key, value)| {
            let value = match value.as_str() {
                Some(text) => Value::String(redact::text(text)),
                None => value.clone(),
            };
            (key.clone(), value)
        })
        .collect();

    match serde_json::to_string(&redacted) {
        Ok(encoded) if encoded.len() <= MAX_EXTRA => Value::Object(redacted),
        _ => json!({ "_dropped": true }),
    }
}

/// Bring an assembled body under [`MAX_BODY`], if the per-field caps somehow
/// did not.
///
/// Shedding rather than failing: a report with no stack is worth much more
/// than no report, and the endpoint would reject the oversized one at the far
/// end where nobody is watching. The stack goes first because it is by far
/// the largest field, and it says so in the body it leaves behind.
fn fit(body: &mut Value) {
    let too_big = |body: &Value| {
        serde_json::to_string(body)
            .map(|encoded| encoded.len() > MAX_BODY)
            .unwrap_or(false)
    };

    if !too_big(body) {
        return;
    }
    if let Some(object) = body.as_object_mut() {
        object.insert("stack".into(), json!("[dropped: report too large]"));
    }
    if !too_big(body) {
        return;
    }
    if let Some(object) = body.as_object_mut() {
        object.insert("message".into(), json!("[dropped: report too large]"));
        object.insert("extra".into(), json!({ "_dropped": true }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::{Auth, Method};

    fn ctx() -> Context {
        Context {
            device_id: "9f2a1c0b-7d3e-4f55-a1b2-c3d4e5f60718".into(),
            session: "session-1".into(),
            platform: "android".into(),
            app_version: "0.4.2".into(),
            bundle: Some("2026-08-14.3".into()),
            ui_version: Some("1.0.0".into()),
            host_revision: Some(12),
            api_url: "https://api.gethomerun.app".into(),
            server_id: None,
        }
    }

    fn ui_error(at_ms: i64) -> Occurrence {
        Occurrence {
            source: Source::Ui,
            severity: Severity::Fatal,
            kind: "TypeError".into(),
            message: "cannot read properties of undefined (reading 'players')".into(),
            stack: Some("    at ServerCard (https://h/_next/static/chunks/a.js:1:2)".into()),
            location: Some("/server/[id]".into()),
            at_ms,
            ..Occurrence::default()
        }
    }

    fn body_of(verdict: &Verdict) -> &Value {
        match verdict {
            Verdict::Send { request, .. } => &request.body,
            Verdict::Hold { reason, .. } => panic!("expected a send, got {reason:?}"),
        }
    }

    fn sent(verdict: &Verdict) -> bool {
        matches!(verdict, Verdict::Send { .. })
    }

    // -- deployment ---------------------------------------------------------

    #[test]
    fn deployment_reads_the_api_host() {
        assert_eq!(deployment("https://api.gethomerun.app"), "production");
        assert_eq!(deployment("https://api.gethomerun.app/api/"), "production");
        assert_eq!(deployment("https://api.fractalnetworks.co"), "staging");
        assert_eq!(deployment("https://api.staging.example.com"), "staging");
    }

    #[test]
    fn a_host_that_is_not_an_api_host_is_unknown_rather_than_a_guess() {
        assert_eq!(deployment("http://localhost:8000"), "unknown");
        assert_eq!(deployment("https://gethomerun.app"), "unknown");
        assert_eq!(deployment("https://staging-api.gethomerun.app"), "unknown");
        assert_eq!(deployment(""), "unknown");
        assert_eq!(deployment("not a url"), "unknown");
    }

    // -- the happy path -----------------------------------------------------

    #[test]
    fn the_first_sighting_is_always_sent() {
        let mut ledger = Ledger::default();
        let verdict = observe(&mut ledger, &ctx(), &ui_error(0));

        assert!(sent(&verdict));
        let body = body_of(&verdict);
        assert_eq!(body["occurrences"], 1);
        assert_eq!(body["source"], "ui");
        assert_eq!(body["severity"], "fatal");
        assert_eq!(body["deployment"], "production");
        assert_eq!(body["bundle"], "2026-08-14.3");
        assert_eq!(body["location"], "/server/[id]");
    }

    #[test]
    fn the_request_is_a_device_signed_post_to_the_endpoint() {
        let mut ledger = Ledger::default();
        let Verdict::Send { request, .. } = observe(&mut ledger, &ctx(), &ui_error(0)) else {
            panic!("expected a send");
        };
        assert_eq!(request.path, "/api/app-error/");
        assert_eq!(request.auth, Auth::Device);
        assert_eq!(request.method, Method::Post);
    }

    // -- dedup and counting -------------------------------------------------

    #[test]
    fn a_repeat_within_the_cooldown_is_held_and_counted() {
        let mut ledger = Ledger::default();
        assert!(sent(&observe(&mut ledger, &ctx(), &ui_error(0))));

        for at in 1..=100 {
            let verdict = observe(&mut ledger, &ctx(), &ui_error(at));
            assert!(
                matches!(
                    verdict,
                    Verdict::Hold {
                        reason: Hold::Cooldown,
                        ..
                    }
                ),
                "sighting {at} should have been held"
            );
        }
        assert_eq!(ledger.sent(), 1);
        assert_eq!(ledger.suppressed(), 100);
    }

    #[test]
    fn the_next_send_reports_the_sightings_since_the_last_one() {
        let mut ledger = Ledger::default();
        observe(&mut ledger, &ctx(), &ui_error(0));
        for at in 1..500 {
            observe(&mut ledger, &ctx(), &ui_error(at));
        }

        let verdict = observe(&mut ledger, &ctx(), &ui_error(COOLDOWN_MS));
        // 499 held plus this one — not the running total, which the API sums.
        assert_eq!(body_of(&verdict)["occurrences"], 500);
        assert_eq!(body_of(&verdict)["suppressed"], 499);
    }

    #[test]
    fn the_cooldown_doubles_so_a_slow_loop_gets_quieter() {
        let mut ledger = Ledger::default();
        let mut now = 0;
        let mut sends = 1;
        assert!(sent(&observe(&mut ledger, &ctx(), &ui_error(now))));

        // Half an hour of a component throwing on every commit at 60 Hz.
        let mut expected_wait = COOLDOWN_MS;
        for _ in 0..5 {
            now += expected_wait;
            assert!(sent(&observe(&mut ledger, &ctx(), &ui_error(now))));
            sends += 1;
            expected_wait = (expected_wait * 2).min(MAX_COOLDOWN_MS);
        }
        assert_eq!(sends, 6);
    }

    #[test]
    fn a_render_loop_costs_a_handful_of_requests_not_thousands() {
        let mut ledger = Ledger::default();
        // 30 minutes at 60 commits a second.
        let mut at = 0i64;
        let mut requests = 0;
        while at < 30 * 60 * 1000 {
            if sent(&observe(&mut ledger, &ctx(), &ui_error(at))) {
                requests += 1;
            }
            at += 1000 / 60;
        }
        assert!(requests <= 7, "{requests} requests for one render loop");
        assert!(ledger.suppressed() > 100_000);
    }

    // -- api grouping -------------------------------------------------------

    fn api_500(server: &str, at_ms: i64) -> Occurrence {
        Occurrence {
            source: Source::Api,
            severity: Severity::Error,
            kind: "http".into(),
            message: "An error occurred".into(),
            http: Some(Http {
                method: "GET".into(),
                url: format!("https://api.gethomerun.app/api/server/{server}/"),
                status: 500,
                body: Some("upstream unavailable".into()),
            }),
            at_ms,
            ..Occurrence::default()
        }
    }

    #[test]
    fn every_server_failing_the_same_way_is_one_group() {
        let mut ledger = Ledger::default();
        let first = observe(&mut ledger, &ctx(), &api_500("9f2a1c0b7d3e4f55", 0));
        assert!(sent(&first));
        assert_eq!(body_of(&first)["http"]["path"], "/api/server/{id}/");

        // Nine more servers, all 500ing. Without path templating each would
        // be its own fingerprint and each would send.
        for (n, server) in ["91ab2c3d4e5f6071", "1122334455667788"]
            .iter()
            .cycle()
            .take(9)
            .enumerate()
        {
            let verdict = observe(&mut ledger, &ctx(), &api_500(server, n as i64 + 1));
            assert!(!sent(&verdict), "server {server} sent a second report");
        }
        assert_eq!(ledger.sent(), 1);
    }

    // -- the rails ----------------------------------------------------------

    fn distinct(n: u32, at_ms: i64) -> Occurrence {
        Occurrence {
            source: Source::Ui,
            severity: Severity::Error,
            kind: format!("Error{n}"),
            message: format!("component {n} failed"),
            at_ms,
            ..Occurrence::default()
        }
    }

    #[test]
    fn a_cascade_of_distinct_failures_is_capped_by_the_burst_window() {
        let mut ledger = Ledger::default();
        let sends = (0..50)
            .filter(|n| sent(&observe(&mut ledger, &ctx(), &distinct(*n, 10))))
            .count();
        assert_eq!(sends, BURST_MAX as usize);
    }

    #[test]
    fn the_burst_window_reopens() {
        let mut ledger = Ledger::default();
        for n in 0..50 {
            observe(&mut ledger, &ctx(), &distinct(n, 10));
        }
        assert!(sent(&observe(
            &mut ledger,
            &ctx(),
            &distinct(999, BURST_WINDOW_MS + 10)
        )));
    }

    #[test]
    fn a_session_cannot_send_more_than_its_share() {
        let mut ledger = Ledger::default();
        // Spread across windows so the burst rail never fires.
        for n in 0..200 {
            observe(
                &mut ledger,
                &ctx(),
                &distinct(n, n as i64 * BURST_WINDOW_MS),
            );
        }
        assert_eq!(ledger.sent(), SESSION_MAX);
    }

    #[test]
    fn a_new_fatal_still_gets_out_past_the_ordinary_cap() {
        let mut ledger = Ledger::default();
        for n in 0..200 {
            observe(
                &mut ledger,
                &ctx(),
                &distinct(n, n as i64 * BURST_WINDOW_MS),
            );
        }
        assert_eq!(ledger.sent(), SESSION_MAX);

        let verdict = observe(&mut ledger, &ctx(), &ui_error(500 * BURST_WINDOW_MS));
        assert!(sent(&verdict), "a first-seen fatal must not be silenced");
    }

    #[test]
    fn even_the_exemption_has_a_ceiling() {
        let mut ledger = Ledger::default();
        for n in 0..500 {
            let mut fatal = distinct(n, n as i64 * BURST_WINDOW_MS);
            fatal.severity = Severity::Fatal;
            observe(&mut ledger, &ctx(), &fatal);
        }
        assert_eq!(ledger.sent(), SESSION_HARD_MAX);
    }

    #[test]
    fn a_repeat_fatal_is_not_exempt() {
        // The exemption is for a fatal nobody has *seen*, not for fatals.
        let mut ledger = Ledger::default();
        for n in 0..200 {
            observe(
                &mut ledger,
                &ctx(),
                &distinct(n, n as i64 * BURST_WINDOW_MS),
            );
        }
        let at = 500 * BURST_WINDOW_MS;
        assert!(sent(&observe(&mut ledger, &ctx(), &ui_error(at))));
        let again = observe(&mut ledger, &ctx(), &ui_error(at + MAX_COOLDOWN_MS * 4));
        assert!(!sent(&again));
    }

    // -- eviction and clocks ------------------------------------------------

    #[test]
    fn the_ledger_does_not_grow_without_bound() {
        let mut ledger = Ledger::default();
        for n in 0..500 {
            observe(
                &mut ledger,
                &ctx(),
                &distinct(n, n as i64 * BURST_WINDOW_MS),
            );
        }
        assert!(ledger.entries.len() <= MAX_TRACKED);
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_wedge_the_window() {
        // A phone's clock does this: NTP, a timezone change, a user setting
        // the date. A window that never expires would silence the session.
        let mut ledger = Ledger::default();
        for n in 0..50 {
            observe(&mut ledger, &ctx(), &distinct(n, 1_000_000));
        }
        assert!(sent(&observe(&mut ledger, &ctx(), &distinct(999, 0))));
    }

    // -- redaction and truncation reach the body ----------------------------

    #[test]
    fn secrets_do_not_reach_the_body() {
        let mut ledger = Ledger::default();
        let seen = Occurrence {
            source: Source::Api,
            kind: "http".into(),
            message: "rejected for someone@example.com".into(),
            stack: Some("at fn (C:\\Users\\Justin\\app.js:1:2)".into()),
            http: Some(Http {
                method: "POST".into(),
                url: "https://api.gethomerun.app/api/auth/?code=SECRET".into(),
                status: 400,
                body: Some("Authorization: Bearer abc123def456".into()),
            }),
            at_ms: 0,
            ..Occurrence::default()
        };

        let verdict = observe(&mut ledger, &ctx(), &seen);
        let encoded = serde_json::to_string(body_of(&verdict)).unwrap();

        for secret in ["someone@example.com", "Justin", "SECRET", "abc123def456"] {
            assert!(!encoded.contains(secret), "{secret} survived: {encoded}");
        }
        // …and the diagnosis did not go with them.
        assert!(encoded.contains("/api/auth/"), "{encoded}");
        assert!(encoded.contains("app.js"), "{encoded}");
    }

    #[test]
    fn the_url_never_reaches_the_body_even_unredacted() {
        // Only the shape goes. A URL is where query strings hide.
        let mut ledger = Ledger::default();
        let verdict = observe(&mut ledger, &ctx(), &api_500("9f2a1c0b7d3e4f55", 0));
        let encoded = serde_json::to_string(body_of(&verdict)).unwrap();
        assert!(
            !encoded.contains("api.gethomerun.app/api/server"),
            "{encoded}"
        );
    }

    #[test]
    fn oversized_fields_are_capped_and_the_body_fits() {
        let mut ledger = Ledger::default();
        let seen = Occurrence {
            kind: "K".repeat(500),
            message: "m".repeat(50_000),
            stack: Some("s".repeat(200_000)),
            location: Some("l".repeat(5_000)),
            at_ms: 0,
            ..Occurrence::default()
        };

        let verdict = observe(&mut ledger, &ctx(), &seen);
        let body = body_of(&verdict);
        assert!(body["message"].as_str().unwrap().len() <= MAX_MESSAGE);
        assert!(body["stack"].as_str().unwrap().len() <= MAX_STACK);
        assert!(body["location"].as_str().unwrap().len() <= MAX_LOCATION);
        assert!(body["kind"].as_str().unwrap().len() <= MAX_KIND);
        assert!(serde_json::to_string(body).unwrap().len() <= MAX_BODY);
    }

    #[test]
    fn an_oversized_extra_map_is_dropped_whole_rather_than_halved() {
        let mut ledger = Ledger::default();
        let mut extra = Map::new();
        extra.insert("componentStack".into(), json!("x".repeat(10_000)));
        let seen = Occurrence {
            message: "boom".into(),
            extra,
            at_ms: 0,
            ..Occurrence::default()
        };

        let verdict = observe(&mut ledger, &ctx(), &seen);
        assert_eq!(body_of(&verdict)["extra"], json!({ "_dropped": true }));
    }

    #[test]
    fn a_small_extra_map_survives_with_its_strings_redacted() {
        let mut ledger = Ledger::default();
        let mut extra = Map::new();
        extra.insert("route".into(), json!("/server/[id]"));
        extra.insert("who".into(), json!("player@example.com"));
        extra.insert("attempt".into(), json!(3));
        let seen = Occurrence {
            message: "boom".into(),
            extra,
            at_ms: 0,
            ..Occurrence::default()
        };

        let verdict = observe(&mut ledger, &ctx(), &seen);
        let body = body_of(&verdict);
        assert_eq!(body["extra"]["route"], "/server/[id]");
        assert_eq!(body["extra"]["attempt"], 3);
        assert_eq!(body["extra"]["who"], "[email redacted]");
    }

    // -- degenerate input ---------------------------------------------------

    #[test]
    fn an_empty_sighting_is_held_and_does_not_take_a_slot() {
        let mut ledger = Ledger::default();
        let verdict = observe(&mut ledger, &ctx(), &Occurrence::default());
        assert!(matches!(
            verdict,
            Verdict::Hold {
                reason: Hold::Empty,
                ..
            }
        ));
        assert!(ledger.entries.is_empty());
        assert_eq!(ledger.sent(), 0);
    }

    #[test]
    fn a_sighting_with_only_a_stack_is_still_reportable() {
        let mut ledger = Ledger::default();
        let seen = Occurrence {
            stack: Some("    at Foo (https://h/a.js:1:2)".into()),
            at_ms: 0,
            ..Occurrence::default()
        };
        assert!(sent(&observe(&mut ledger, &ctx(), &seen)));
    }

    #[test]
    fn a_context_with_nothing_in_it_still_produces_a_report() {
        // The most valuable report of all is the one from a boot that failed
        // before anything could be primed.
        let mut ledger = Ledger::default();
        let verdict = observe(&mut ledger, &Context::default(), &ui_error(0));
        assert!(sent(&verdict));
        assert_eq!(body_of(&verdict)["deployment"], "unknown");
    }

    #[test]
    fn non_ascii_survives_the_whole_pipeline() {
        let mut ledger = Ledger::default();
        let seen = Occurrence {
            message: "mod “Térraforge” failed for 玩家".into(),
            at_ms: 0,
            ..Occurrence::default()
        };
        let verdict = observe(&mut ledger, &ctx(), &seen);
        assert!(body_of(&verdict)["message"]
            .as_str()
            .unwrap()
            .contains("玩家"));
    }

    #[test]
    fn a_ledger_round_trips_through_json() {
        // Not persisted by policy, but the FFI crate serialises it in tests
        // and a silent serde break would only show up there.
        let mut ledger = Ledger::default();
        observe(&mut ledger, &ctx(), &ui_error(0));
        let encoded = serde_json::to_string(&ledger).unwrap();
        assert_eq!(serde_json::from_str::<Ledger>(&encoded).unwrap(), ledger);
    }
}

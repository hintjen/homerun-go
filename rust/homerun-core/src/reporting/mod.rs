//! What a host tells the API about the server it is running.
//!
//! Reference: `nativeServerManager.ts` — `reportCrash`, `reportServiceStats`,
//! `collectServiceStats`, `maybeReportPlayerPresence`,
//! `maybeReportMinigameStats`.
//!
//! # Why this is here rather than in each host
//!
//! None of this is required to *run* a server, which is exactly why it drifts:
//! a host that never reports is a host that looks fine. The desktop has
//! reported for years; Android reported its instance heartbeat, its state and
//! its backups, and nothing else. So a crashed server on a phone gave the
//! player no explanation and support no logs, the dashboard's graphs stayed
//! empty, and journeys never saw a session shorter than the reporting
//! interval.
//!
//! Every decision behind those reports is the same on every platform — what a
//! crash log means, what the payload contains, how often to send, when a join
//! is worth an early report. Only the request itself differs, and that is the
//! one thing a phone cannot share.
//!
//! # The shape
//!
//! Each module here answers with a [`Request`]: the method, the path, the
//! body, and **which credential to sign it with**. The host performs it. It
//! chooses no path, builds no body, and — the part worth stating — never has
//! to work out whether something is signed by the device or by the person at
//! the keyboard, because getting that wrong is either a silent 403 or a report
//! attributed to the wrong user.
//!
//! ```text
//!   host: a line, a crash, a timer   →  reporting::…
//!   core:                            →  Request { method, path, body, auth }
//!   host: sign it, send it, forget it
//! ```
//!
//! Nothing here retries, and nothing here fails loudly. A report that does not
//! arrive is a gap in a graph; a report that interrupts hosting is a session
//! lost. Every caller is expected to fire and forget.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod app_error;
pub mod crash;
pub mod minigame;
pub mod scrub;
pub mod stats;
mod truncate;

/// Which credential signs a request.
///
/// Not interchangeable. The device token identifies *this install* and is what
/// the reporting endpoints accept; the user token identifies the person, and
/// the API's role engine judges what they may change with it. Reporting is
/// always the device — a report is a fact about a machine, not an act by a
/// person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Auth {
    /// The device token from registration.
    Device,
    /// The signed-in user's access token.
    User,
}

/// How to send it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Method {
    Post,
    Patch,
}

/// One API call, decided here and performed by the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub method: Method,
    /// Rooted, with its trailing slash — the API is Django and redirects
    /// without one, which turns a POST into a GET and loses the body.
    pub path: String,
    pub body: Value,
    pub auth: Auth,
}

impl Request {
    /// A device-signed POST, which is every report but one.
    pub(crate) fn post(path: impl Into<String>, body: Value) -> Self {
        Request {
            method: Method::Post,
            path: path.into(),
            body,
            auth: Auth::Device,
        }
    }
}

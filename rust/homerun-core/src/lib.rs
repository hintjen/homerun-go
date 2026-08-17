//! The host logic every Homerun app needs, in one place.
//!
//! # Why this crate exists
//!
//! Three apps host Minecraft servers — the Electron desktop, iOS, and Android
//! — and until now each carried its own copy of the same decisions. Bringing
//! Android up meant hand-mirroring, from TypeScript into Kotlin: how a server
//! jar is chosen and verified, how a wireproxy config is laid out, when a
//! tunnel counts as failed, what a server's state machine looks like, and how
//! the gateway's credentials are judged fresh or stale.
//!
//! Hand-mirroring drifts, and it already had. Two divergences existed before
//! this crate was written:
//!
//!  - the desktop picks Paper's **oldest** build, an alpha, because the v3 API
//!    returns newest-first and the code takes the last element (`jar::paper`
//!    has the regression test)
//!  - the desktop pushes an instance report the moment a server goes running
//!    and Android did not, so a server that was genuinely up read as stopped
//!    for up to half a minute
//!
//! Neither was a hard bug in one place. Both were two implementations of one
//! decision, drifting apart.
//!
//! # What belongs here, and what does not
//!
//! **Decisions and shapes belong here. Transport and processes do not.**
//!
//! Every function in this crate is pure: give it the JSON an endpoint
//! returned, or the state something is in, and it tells you what to do. It
//! opens no sockets, spawns nothing, and has no async runtime — which is what
//! makes it exhaustively testable and what keeps the FFI surface a plain C
//! ABI rather than a runtime bridge.
//!
//! The platform keeps what only the platform can do: making the HTTP request,
//! spawning the JVM, spawning the tunnel, sampling CPU. That split is not a
//! compromise — iOS cannot spawn a process at all, so a "shared supervisor"
//! that owned process handling could never have been shared with it. What
//! *can* be shared is everything it would have decided.
//!
//! # Provenance
//!
//! The desktop is the reference implementation for all of this, and the module
//! docs name the file each behaviour came from. Where this crate deliberately
//! differs from the desktop, it says so and why.

// The game-agnostic layer. None of this knows what it is hosting.
pub mod backup;
pub mod bundle;
pub mod device_ws;
pub mod game;
pub mod launch;
pub mod lifecycle;
pub mod link;
pub mod metrics;
pub mod properties;
pub mod reporting;
pub mod state;
pub mod tunnel;

// One game, implementing `game::Game`. Everything Minecraft-specific in this
// crate is under here and nothing above depends on it except the registry.
pub mod minecraft;

// Two hashes, both private, both here for the same reason: a derivation this
// crate has to reproduce byte-for-byte and neither of them trusted with
// anything that matters. Each module's header has the argument for why it is
// hand-rolled rather than a dependency.
mod md5;
mod sha1;

/// Anything this crate can refuse to do.
///
/// Deliberately small and free of transport errors — a caller that could not
/// reach an endpoint knows that already, and how it says so is its own
/// business. These are the cases where the *answer* is no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The response parsed, but did not contain what it must.
    Malformed(String),
    /// Asked for something real that this build cannot do.
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Malformed(what) => write!(f, "{what}"),
            Error::Unsupported(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

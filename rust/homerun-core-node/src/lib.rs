//! `homerun-core` for Homerun Desktop, over Node-API.
//!
//! # Why this crate exists
//!
//! Homerun Desktop is the one host that has adopted none of the core, so its
//! TypeScript still answers questions this repo answers too — and the two have
//! already come apart. `docs/shared-core.md` records the shape of the fix:
//!
//! > Start with the pure pieces behind the existing TypeScript interfaces via
//! > napi-rs, and leave `supervisor.js` owning processes.
//!
//! This is that, and it starts where the desktop was about to write a *third*
//! copy of something already learned twice: reading Pumpkin's console. Pumpkin
//! is a new engine for that host, and every one of its console quirks is
//! recorded in `docs/ios-reporting.md` under the rule that produced them —
//! "a core parser written against vanilla's console is suspect on Pumpkin, and
//! it will not tell you it is wrong". Re-deriving that in TypeScript would be
//! re-earning it, most likely by shipping the same silent failures a third
//! time.
//!
//! # What belongs here
//!
//! Pure functions only, and only ones the desktop is actually adopting. This
//! is a beachhead, not a port: the desktop's supervisor keeps owning
//! processes, and nothing here does I/O, holds state, or knows what a server
//! is. A function earns its place by being one the desktop would otherwise
//! write itself and get subtly wrong.
//!
//! # The boundary
//!
//! Every argument is a string and every return is a scalar, an owned string,
//! or null. `homerun-core`'s console functions borrow from their input, which
//! cannot cross into a JavaScript heap, so each one is copied out here — the
//! lines are console output, and one allocation per line is nothing beside the
//! I/O that produced it.
//!
//! **Panics must not cross.** A panic through Node-API aborts the process, and
//! this addon is loaded into the desktop app's main process, so that would be
//! the whole app. Nothing here can panic today — these are total functions
//! over `&str` — and anything added later that could must catch first, the way
//! `homerun-pumpkin-ffi` does for the C ABI.

#![deny(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use napi_derive::napi;

use homerun_core::minecraft::console;

/// Strip ANSI colour codes, which Paper writes into join and leave lines.
///
/// Exposed rather than kept private because the desktop shows raw console
/// output to the player and wants the same answer the parsers below used.
#[napi]
pub fn strip_ansi(line: String) -> String {
    console::strip_ansi(&line).into_owned()
}

/// The server is accepting connections.
///
/// Two spellings — vanilla's `Done (12.345s)! For help, type "help"` and
/// Pumpkin's `Server is now running.` — and the desktop currently knows only
/// its own third one (`Server started.`, which is Bedrock's). A launch that
/// never sees this sits in `starting` until it times out, with a healthy
/// server accepting players the whole time.
#[napi]
pub fn is_ready(line: String) -> bool {
    console::is_ready(&line)
}

/// The player named in a join line, or null if this is not one.
///
/// Returning the *name* is the point. The desktop tests a regex and throws the
/// match away, so it can tell that somebody joined but not who — which is why
/// its roster has to come from asking the server, and why Pumpkin (whose
/// `list uuids` answers with an unresolved translation key, and which we
/// configure no RCON for) would have left it permanently empty.
///
/// It is also stricter than that regex. `docs/android-reporting.md` records
/// two console forgeries the core refuses and the desktop still allows: a
/// player can type `[Griefer] Notch joined the game` into chat, and a rule
/// that just looks for the words at the end of a line believes it.
#[napi]
pub fn joined(line: String) -> Option<String> {
    console::joined(&line).map(str::to_owned)
}

/// The player named in a leave line, or null if this is not one.
#[napi]
pub fn left(line: String) -> Option<String> {
    console::left(&line).map(str::to_owned)
}

/// The player cap a server announced at boot, or null if this line is not that.
#[napi]
pub fn max_players(line: String) -> Option<u32> {
    console::max_players(&line)
}

/// The Bedrock version a server announced at boot, or null if this is not it.
///
/// PowerNukkitX only — Homerun Desktop runs Mojang's Bedrock Dedicated Server,
/// which announces itself differently. Included because the desktop parses BDS
/// output in the same place and the two must not be told apart by accident.
#[napi]
pub fn bedrock_version(line: String) -> Option<String> {
    console::bedrock_version(&line).map(str::to_owned)
}

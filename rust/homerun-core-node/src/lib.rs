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
//! The second thing it carries is the over-the-air bundle verifier, because
//! the desktop is taking UI bundles over the air the way the phones do. That
//! one is not a matter of drift but of trust: the payload is the entire user
//! interface, and a signature check that is slightly looser on one host is a
//! CDN that can replace that host's app. Node could check an Ed25519 signature
//! on its own, but the judgement is more than the signature — strict
//! verification, field validation, `minHost`, the strictly-climbing serial,
//! the signed platform — and `homerun_core::bundle` is the copy that the phones
//! run and that a pinned vector holds against the signer. A TypeScript
//! re-derivation would be the first host to accept something the others
//! refuse, and nothing would ever say so.
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
//! Structured answers — so far only [`bundle_evaluate`] — cross as a JSON
//! *string*, not a JavaScript object. That is the shape the phones already
//! receive from `homerun-supervisor`, so the field names and the tagged
//! verdict are defined once, by serde, and not a second time by hand-built
//! napi objects that could quietly spell `minHost` differently.
//!
//! Failures that are the input's fault throw a JavaScript `Error` carrying the
//! core's own sentence, so the desktop's log reads the same as Android's.
//!
//! **Panics must not cross.** A panic through Node-API aborts the process, and
//! this addon is loaded into the desktop app's main process, so that would be
//! the whole app. The console functions are total over `&str`. The bundle
//! functions call into `serde_json` and `ed25519-dalek`, and `bundle::verify`
//! holds two `expect`s that are unreachable today — "unreachable today" being
//! a claim about someone else's code — so they run inside `catch_unwind`, the
//! way `homerun-supervisor` does for the C ABI, and a panic becomes a thrown
//! error naming the function. Anything added later that could panic does the
//! same.

#![deny(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::panic::{catch_unwind, UnwindSafe};

use napi_derive::napi;

use homerun_core::bundle;
use homerun_core::minecraft::console;

/// Node addon contract version, distinct from the mobile C FFI ABI.
pub const CORE_NODE_ABI_VERSION: u32 = 1;

#[napi]
pub fn core_abi_version() -> u32 {
    CORE_NODE_ABI_VERSION
}

#[napi]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Source provenance; the manifest separately identifies the signed bytes.
#[napi]
pub fn core_build_id() -> String {
    env!("HOMERUN_CORE_BUILD_ID").to_owned()
}

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

// --- over-the-air UI bundles -------------------------------------------------

/// Verify a manifest and judge it against what this host is serving, as one
/// call. Returns `{manifest, verdict, reason, install}` as a JSON string;
/// throws with the core's sentence if `installed` does not parse or the
/// manifest does not verify.
///
/// One call on purpose, and the same one the phones make (`bundle.evaluate` in
/// `homerun-supervisor`). Two would let the desktop judge a manifest it had
/// not verified, and that mistake has no symptom: everything keeps working,
/// against any manifest anyone serves. The only way to get a manifest's
/// fields out of this addon is to have had them verified.
///
/// `installed` is `{bundle, serial, hostRevision, platform}`. `platform` is
/// whatever the desktop calls itself — the core compares it to the signed
/// field and has no list of allowed values, so `windows` needs nothing here.
///
/// A declined bundle is **not** a throw: `install` is false and `reason` is
/// the line for the log, because a host that silently declines an update is
/// indistinguishable from one that cannot reach the network.
#[napi]
pub fn bundle_evaluate(
    manifest: String,
    public_key: String,
    installed: String,
) -> napi::Result<String> {
    guarded("bundleEvaluate", move || {
        evaluate(&manifest, &public_key, &installed).map_err(napi::Error::from_reason)
    })
}

/// Whether the SHA-256 the desktop computed over a downloaded archive is the
/// one the manifest signed.
///
/// Hashing stays in the host — `node:crypto` streams the file, and crossing
/// Node-API once per chunk would be slower for no gain — but the *comparison*
/// is the core's, so the desktop does not write its own with `===` and one day
/// with `startsWith`.
#[napi]
pub fn bundle_digest_matches(expected: String, actual: String) -> napi::Result<bool> {
    guarded("bundleDigestMatches", move || {
        Ok(bundle::digest_matches(&expected, &actual))
    })
}

/// The body of [`bundle_evaluate`], in plain Rust so the reply can be tested
/// without a Node runtime.
///
/// Mirrors the FFI arm line for line — the installed record is parsed first,
/// and the error texts match — so a desktop log and an Android log of the same
/// refusal are the same sentence.
fn evaluate(manifest: &str, public_key: &str, installed: &str) -> Result<String, String> {
    let installed: bundle::Installed =
        serde_json::from_str(installed).map_err(|e| format!("bad installed record: {e}"))?;
    let manifest = bundle::verify(manifest, public_key).map_err(|e| e.to_string())?;
    let verdict = bundle::judge(&manifest, &installed);
    let reply = serde_json::json!({
        "manifest": serde_json::to_value(&manifest).map_err(|e| e.to_string())?,
        "verdict": serde_json::to_value(&verdict).map_err(|e| e.to_string())?,
        "reason": verdict.reason(),
        "install": verdict.should_install(),
    });
    Ok(reply.to_string())
}

/// Run `f`, turning a panic into a thrown JavaScript error instead of an
/// aborted desktop app. Seeing the message means a bug in native code, not bad
/// input, and it says so rather than dressing it up as a user-facing failure.
fn guarded<T>(name: &str, f: impl FnOnce() -> napi::Result<T> + UnwindSafe) -> napi::Result<T> {
    catch_unwind(f).unwrap_or_else(|_| {
        Err(napi::Error::from_reason(format!(
            "the native core panicked handling {name}"
        )))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// The pinned vector from `homerun_core::bundle`'s tests — signed by
    /// `scripts/sign-manifest.js` with a throwaway key published in the repo.
    /// Reused rather than re-signed so this crate cannot pass against a
    /// signer of its own.
    const MANIFEST: &str = r#"{"bundle":"2026-08-14.1","url":"https://cdn.gethomerun.app/ui/2026-08-14.1.zip","sha256":"d2045f55566b0d63ab5ac9216c8b068117a18043f0ba6453f7098dcbf8a4b038","minHost":1,"serial":3,"platform":"android","signature":"18b8a9dcd15af0a141d87eaf72b130e7698df55ec25744245f690bcaf0d4082fa0d373ed30f12c3536972f7d0d820e841671958fc1d042a206cd872670755506"}"#;
    const PUBLIC_KEY: &str = "f94519c8187b4ea306e539eb27010b6074e1a12bcc8b7fe654a27978abaefd21";

    fn reply(installed: &str) -> serde_json::Value {
        serde_json::from_str(&evaluate(MANIFEST, PUBLIC_KEY, installed).unwrap()).unwrap()
    }

    #[test]
    fn replies_in_the_shape_the_ffi_does() {
        let reply = reply(r#"{"bundle":null,"serial":0,"hostRevision":1,"platform":"android"}"#);
        assert_eq!(reply["install"], true);
        assert_eq!(reply["verdict"]["verdict"], "install");
        assert_eq!(reply["manifest"]["minHost"], 1);
        assert_eq!(reply["manifest"]["serial"], 3);
        assert!(reply["reason"].as_str().unwrap().contains("newer"));
    }

    #[test]
    fn a_tampered_manifest_is_an_error_not_a_verdict() {
        let tampered = MANIFEST.replace(r#""serial":3"#, r#""serial":4"#);
        let error = evaluate(
            &tampered,
            PUBLIC_KEY,
            r#"{"bundle":null,"serial":0,"hostRevision":1,"platform":"android"}"#,
        )
        .unwrap_err();
        assert!(error.contains("signature"), "{error}");
    }

    #[test]
    fn a_malformed_installed_record_is_named_as_such() {
        let error = evaluate(MANIFEST, PUBLIC_KEY, "{").unwrap_err();
        assert!(error.starts_with("bad installed record"), "{error}");
    }

    /// Asserting "we call catch_unwind" by reading the code is not evidence.
    #[test]
    fn a_panic_becomes_an_error() {
        let result: napi::Result<()> = guarded("test", || panic!("on purpose"));
        let error = result.unwrap_err();
        assert!(
            error.reason.contains("panicked handling test"),
            "{}",
            error.reason
        );
    }
}

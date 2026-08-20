//! Deciding when two failures are the same failure.
//!
//! # What this is for
//!
//! Everything else in this module tree — the counting, the cooldown, the
//! session cap — is downstream of one question: *have I seen this before?*
//! Get it wrong in the loose direction and two unrelated bugs share a row and
//! neither gets fixed. Get it wrong in the strict direction and the rate
//! limiter never fires, because every sighting of one render loop looks new.
//!
//! The strict direction is the dangerous one, and it is the easy mistake:
//! stack traces carry line numbers, URLs carry ids, messages carry the value
//! that was wrong. Left alone, a single bug produces a distinct fingerprint on
//! every device, every release, and often every occurrence.
//!
//! # The signature is sent, not just hashed
//!
//! [`signature`] builds a short human-readable string, and the report carries
//! it beside the hash. Without it a reviewer looking at a group of 18,000
//! occurrences has to reverse-engineer what the group *is*. It costs 300 bytes
//! and it is the difference between a table and a mystery.
//!
//! # What is deliberately not in the fingerprint
//!
//! **The app version, the bundle id, and the platform.** Grouping *across*
//! releases is what answers "is this still happening after the fix" — the one
//! question the report exists to answer. The API can group by
//! `(fingerprint, app_version)` at query time when it wants the other view;
//! it cannot ungroup what was split here.
//!
//! **Line and column numbers.** The UI bundle is minified and rebuilt weekly,
//! so a line number identifies a build rather than a bug. Including one splits
//! every fingerprint on every release.

use crate::sha1;

use super::{Occurrence, Source};

/// The unit separator. A control character, so it cannot occur in any of the
/// three parts and there is no escaping to get wrong.
const SEP: char = '\u{1f}';

/// How many hex characters of the digest to keep.
///
/// 16 hex characters is 64 bits. At the volumes this feature is designed for
/// — tens of thousands of distinct fingerprints, ever — a collision is not a
/// consideration, and a short id is one somebody can read out loud.
const LEN: usize = 16;

/// How many stack frames identify a bug.
///
/// One is too few: `Array.map` throwing says nothing about which caller. Many
/// is too many: the deeper the frame, the more likely it is framework
/// plumbing that shifts between releases.
const FRAMES: usize = 3;

/// The cap the signature is truncated to before it is sent.
pub(crate) const MAX_SIGNATURE: usize = 300;

/// `sha1(source ␟ kind ␟ signature)`, first [`LEN`] hex characters.
///
/// SHA-1 is not a security primitive here and is not treated as one — see
/// [`crate::sha1`]. This is a grouping key: the only property required of it
/// is that the same input gives the same answer on Android, on iOS, and in a
/// unit test on somebody's laptop.
pub(crate) fn hash(source: Source, kind: &str, signature: &str) -> String {
    let input = format!("{}{SEP}{kind}{SEP}{signature}", source.as_str());
    let mut digest = sha1::hex(input.as_bytes());
    digest.truncate(LEN);
    digest
}

/// A short description of *what kind of thing this is*, stable across
/// occurrences of the same bug.
pub(crate) fn signature(seen: &Occurrence) -> String {
    let raw = match (&seen.source, &seen.http) {
        // An HTTP failure is identified by what was called and what came
        // back. The message is ignored outright: DRF writes a different
        // sentence for the same fault depending on which serializer field
        // objected, and grouping on that splits one broken endpoint into
        // twenty.
        (Source::Api, Some(http)) => {
            format!("{} {} {}", http.method, path_shape(&http.url), http.status)
        }
        _ => match seen.stack.as_deref().map(frames) {
            Some(frames) if !frames.is_empty() => frames.join(" < "),
            // No usable stack. A route plus a generalised message is weaker
            // but it is what a `window.onerror` from a cross-origin script
            // gives us, and a weak group beats no group.
            _ => match seen.location.as_deref() {
                Some(at) if !at.is_empty() => format!("{at}: {}", generalise(&seen.message)),
                _ => generalise(&seen.message),
            },
        },
    };

    crate::reporting::truncate::head(&raw, MAX_SIGNATURE)
}

// ---------------------------------------------------------------------------
// HTTP paths
// ---------------------------------------------------------------------------

/// A URL reduced to the shape of its path: no scheme, no host, no query, and
/// every id replaced with `{id}`.
///
/// This is the single most important normalisation in the module. Without it
/// every server produces its own fingerprint for one broken endpoint, the
/// dedup finds nothing to merge, and a fleet-wide 500 arrives as one report
/// per server per device.
///
/// Also used to build the `http.path` field, so a reviewer sees the same
/// shape the grouping used.
pub(crate) fn path_shape(url: &str) -> String {
    let path = strip_origin(url);
    let path = path.split(['?', '#']).next().unwrap_or(path);

    let mut out = String::with_capacity(path.len());
    for (n, segment) in path.split('/').enumerate() {
        if n > 0 {
            out.push('/');
        }
        if is_id(segment) {
            out.push_str("{id}");
        } else {
            out.push_str(segment);
        }
    }
    out
}

/// Drop `scheme://host` if there is one, keeping the leading `/` of the path.
fn strip_origin(url: &str) -> &str {
    let Some(after_scheme) = url.find("://").map(|i| i + 3) else {
        return url;
    };
    match url[after_scheme..].find('/') {
        Some(slash) => &url[after_scheme + slash..],
        // A bare origin with no path at all.
        None => "/",
    }
}

/// Whether a path segment identifies one thing rather than naming a kind.
///
/// Deliberately conservative in one direction only: a missed id splits a
/// group, which is visible in the data and fixable here. A false positive
/// erases the difference between `/api/server/` and `/api/status/`, which is
/// not visible at all.
fn is_id(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    if segment.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    // A UUID with or without dashes, or any long hex run — Matrix ids, sha
    // digests and device ids all land here.
    segment.len() >= 16
        && segment.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        && segment.bytes().any(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Stack frames
// ---------------------------------------------------------------------------

/// The first [`FRAMES`] meaningful frames, each as `file:symbol`.
///
/// Four languages produce the stacks this reads and none of them agree on a
/// format, so the parse is deliberately forgiving: find a location, find a
/// symbol, keep whichever it got. A frame it cannot parse is kept whole
/// rather than dropped — an ugly group is still a group.
fn frames(stack: &str) -> Vec<String> {
    // The raw line is kept beside the normalised frame because noise is a
    // property of the *path*, and normalising throws the path away: once
    // `.../node_modules/react-dom/index.js` is `index.js`, nothing is left to
    // recognise it by.
    let parsed: Vec<(String, &str)> = stack
        .lines()
        .filter_map(|line| parse_frame(line).map(|frame| (frame, line)))
        .collect();

    let mut meaningful: Vec<String> = parsed
        .iter()
        .filter(|(frame, line)| !is_noise(frame) && !is_noise(line))
        .map(|(frame, _)| frame.clone())
        .collect();

    // Everything was framework plumbing. That happens for a throw inside
    // React's own reconciler, and dropping to nothing would collapse every
    // such bug into one group — so keep the top frame and let it be noisy.
    if meaningful.is_empty() {
        meaningful = parsed.into_iter().take(1).map(|(frame, _)| frame).collect();
    }

    meaningful.truncate(FRAMES);
    meaningful
}

/// One line of a stack, normalised. None for blank lines and the header line
/// that repeats the message.
fn parse_frame(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // `at fn (loc)` — JavaScript and Kotlin. `0: fn` — Rust. `3 Bin 0x… fn`
    // — Swift; its leading columns are dropped by the same trimming.
    let body = line
        .strip_prefix("at ")
        .or_else(|| strip_numeric_prefix(line))
        .unwrap_or(line);

    // Only lines that look like frames. A stack's first line is usually the
    // message again, which would otherwise become the whole signature.
    if body == line && !line.starts_with("at ") && !body.contains('(') && !body.contains('/') {
        return None;
    }

    let (symbol, location) = match (body.rfind('('), body.rfind(')')) {
        (Some(open), Some(close)) if close > open => {
            (body[..open].trim(), Some(&body[open + 1..close]))
        }
        _ => (body, None),
    };

    let file = location.map(|at| strip_chunk_hash(&basename(at)));
    let symbol = clean_symbol(symbol);

    match (file, symbol) {
        (Some(file), Some(symbol)) if !file.is_empty() => Some(format!("{file}:{symbol}")),
        (Some(file), None) if !file.is_empty() => Some(file),
        (_, Some(symbol)) => Some(symbol),
        _ => None,
    }
}

/// `   0: ` / `3   ` — the ordinal Rust and Swift put in front of a frame.
fn strip_numeric_prefix(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.len() == line.len() {
        return None;
    }
    Some(rest.trim_start_matches([':', ' ']))
}

/// The filename out of a location, with any query and `:line:col` removed.
fn basename(location: &str) -> String {
    let file = location
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(location)
        .split('?')
        .next()
        .unwrap_or(location);

    // Trim trailing `:line:col`. Only numeric tails — a Windows drive letter
    // and a Kotlin file both contain colons that are not positions.
    let mut out = file;
    for _ in 0..2 {
        if let Some((head, tail)) = out.rsplit_once(':') {
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                out = head;
                continue;
            }
        }
        break;
    }
    out.to_string()
}

/// Drop the content hash a bundler writes into a filename.
///
/// `index-9f2a.js` and `index-4b71.js` are the same module from two builds.
/// Keeping the hash would give one bug a new fingerprint on every publish,
/// which defeats the one thing the fingerprint exists for — and the UI bundle
/// is republished weekly, so this is the common case rather than the corner.
///
/// The tail must be at least four characters and contain a digit, so an
/// ordinary name survives: `server-2.js` keeps its `-2`, and `main.js` and
/// `Reporting.kt` are untouched.
fn strip_chunk_hash(file: &str) -> String {
    let Some((stem, extension)) = file.rsplit_once('.') else {
        return file.to_string();
    };
    for separator in ['-', '.'] {
        if let Some((head, tail)) = stem.rsplit_once(separator) {
            if !head.is_empty() && is_content_hash(tail) {
                return format!("{head}.{extension}");
            }
        }
    }
    file.to_string()
}

fn is_content_hash(tail: &str) -> bool {
    tail.len() >= 4
        && tail.bytes().all(|b| b.is_ascii_hexdigit())
        && tail.bytes().any(|b| b.is_ascii_digit())
}

/// A symbol without the address noise Swift and Rust put around it.
fn clean_symbol(symbol: &str) -> Option<String> {
    let symbol = symbol
        .split_whitespace()
        .find(|part| !part.starts_with("0x") && !part.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(symbol)
        .trim();

    // Rust monomorphisation suffixes: `foo::bar::h3f2a9c`.
    let symbol = match symbol.rsplit_once("::h") {
        Some((head, tail)) if tail.bytes().all(|b| b.is_ascii_hexdigit()) => head,
        _ => symbol,
    };

    (!symbol.is_empty()).then(|| symbol.to_string())
}

/// Frames that are somebody else's code, in every language at once.
///
/// A frame is noise when it cannot distinguish one bug from another: every
/// React error passes through `react-dom`, every coroutine through
/// `kotlinx.coroutines`. Keeping them would make the top three frames
/// identical for unrelated failures.
fn is_noise(frame: &str) -> bool {
    const MARKERS: &[&str] = &[
        // JavaScript
        "node_modules",
        "react-dom",
        "react.production",
        "webpack",
        "next/dist",
        "_next/static/chunks/framework",
        // JVM
        "java.lang.",
        "java.util.",
        "kotlin.",
        "kotlinx.coroutines",
        "android.os.",
        "android.app.",
        "dalvik.system",
        // Apple
        "Foundation:",
        "UIKit:",
        "libswiftCore",
        "libdyld",
        "CoreFoundation",
        // Rust
        "core::panicking",
        "std::panicking",
        "core::result",
    ];
    MARKERS.iter().any(|marker| frame.contains(marker))
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A message with the parts that vary per occurrence replaced by `#`.
///
/// Only used for *hashing*. The message that is sent is the real one — a
/// generalised message is for deciding sameness, and a reviewer needs the
/// actual value that was wrong.
///
/// # Quoted text is deliberately left alone
///
/// It is tempting to collapse `'players'` and `'ops'` in
/// `cannot read properties of undefined (reading 'players')`, and it would be
/// wrong. The property name *is* the bug's identity — those two are different
/// faults in different components, and merging them produces a group nobody
/// can act on. Ids and offsets vary within one bug; a field name does not.
fn generalise(message: &str) -> String {
    let bytes = message.as_bytes();
    let mut out = String::with_capacity(message.len());
    let mut i = 0;

    while i < bytes.len() {
        // A long hex or UUID-shaped run: an id, a digest, an address.
        if bytes[i].is_ascii_hexdigit() {
            let mut end = i;
            while end < bytes.len() && (bytes[end].is_ascii_hexdigit() || bytes[end] == b'-') {
                end += 1;
            }
            if end - i >= 8 && bytes[i..end].iter().any(u8::is_ascii_digit) {
                out.push('#');
                i = end;
                continue;
            }
        }

        // Any run of digits: a count, an offset, a port, a status.
        if bytes[i].is_ascii_digit() {
            let end = {
                let mut e = i;
                while e < bytes.len() && bytes[e].is_ascii_digit() {
                    e += 1;
                }
                e
            };
            out.push('#');
            i = end;
            continue;
        }

        let ch = message[i..]
            .chars()
            .next()
            .expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::app_error::{Http, Severity};

    fn occurrence(source: Source, kind: &str, message: &str) -> Occurrence {
        Occurrence {
            source,
            severity: Severity::Error,
            kind: kind.to_string(),
            message: message.to_string(),
            ..Occurrence::default()
        }
    }

    // -- paths --------------------------------------------------------------

    #[test]
    fn a_uuid_segment_becomes_an_id() {
        assert_eq!(
            path_shape(
                "https://api.gethomerun.app/api/server/9f2a1c0b-7d3e-4f55-a1b2-c3d4e5f60718/logs/"
            ),
            "/api/server/{id}/logs/"
        );
    }

    #[test]
    fn a_numeric_segment_becomes_an_id() {
        assert_eq!(path_shape("/api/page/42/"), "/api/page/{id}/");
    }

    #[test]
    fn a_named_segment_is_kept() {
        // The whole point: `/server/` and `/status/` must not merge.
        assert_eq!(path_shape("/api/status/"), "/api/status/");
        assert_ne!(path_shape("/api/server/"), path_shape("/api/status/"));
    }

    #[test]
    fn the_query_and_fragment_are_dropped() {
        assert_eq!(path_shape("/api/x/?code=abc#frag"), "/api/x/");
    }

    #[test]
    fn a_relative_path_survives_having_no_origin() {
        assert_eq!(path_shape("/api/user/"), "/api/user/");
    }

    #[test]
    fn a_bare_origin_reduces_to_a_root() {
        assert_eq!(path_shape("https://api.gethomerun.app"), "/");
    }

    // -- api grouping -------------------------------------------------------

    #[test]
    fn two_servers_failing_the_same_way_are_one_group() {
        let one = Occurrence {
            http: Some(Http {
                method: "GET".into(),
                url: "https://api.gethomerun.app/api/server/9f2a1c0b7d3e4f55/".into(),
                status: 500,
                body: None,
            }),
            ..occurrence(Source::Api, "http", "An error occurred")
        };
        let two = Occurrence {
            http: Some(Http {
                method: "GET".into(),
                url: "https://api.gethomerun.app/api/server/91ab2c3d4e5f6071/".into(),
                status: 500,
                // A different sentence for the same fault — deliberately so.
                body: None,
            }),
            ..occurrence(Source::Api, "http", "Resource not found")
        };

        assert_eq!(signature(&one), signature(&two));
        assert_eq!(
            hash(Source::Api, "http", &signature(&one)),
            hash(Source::Api, "http", &signature(&two))
        );
    }

    #[test]
    fn a_different_status_is_a_different_group() {
        let base = |status| Occurrence {
            http: Some(Http {
                method: "GET".into(),
                url: "/api/server/".into(),
                status,
                body: None,
            }),
            ..occurrence(Source::Api, "http", "x")
        };
        assert_ne!(signature(&base(500)), signature(&base(503)));
    }

    // -- stacks -------------------------------------------------------------

    #[test]
    fn a_javascript_stack_groups_across_releases() {
        // Same bug, two builds: different chunk hash, different line numbers.
        let a = "TypeError: x\n    at ServerCard (https://h/_next/static/chunks/pages/index-9f2a.js:1:200)\n    at div";
        let b = "TypeError: x\n    at ServerCard (https://h/_next/static/chunks/pages/index-4b71.js:88:9)\n    at div";

        let one = Occurrence {
            stack: Some(a.into()),
            ..occurrence(Source::Ui, "TypeError", "x")
        };
        let two = Occurrence {
            stack: Some(b.into()),
            ..occurrence(Source::Ui, "TypeError", "x")
        };

        assert!(
            signature(&one).contains("ServerCard"),
            "{}",
            signature(&one)
        );
        assert_eq!(signature(&one), signature(&two));
    }

    #[test]
    fn framework_frames_do_not_decide_the_group() {
        let stack = "\
    at commitHookEffectListMount (https://h/_next/static/chunks/framework-1.js:1:1)
    at node_modules/react-dom/index.js:2:2
    at OverviewSettings (https://h/_next/static/chunks/pages/server-2.js:3:3)";
        let seen = Occurrence {
            stack: Some(stack.into()),
            ..occurrence(Source::Ui, "TypeError", "x")
        };
        let sig = signature(&seen);
        // The component is what identifies the bug...
        assert!(sig.contains("OverviewSettings"), "{sig}");
        // ...and neither framework frame may appear at all. Asserting only
        // the first of these passes even with the filter removed, because
        // three frames are kept and the component is the third.
        assert!(!sig.contains("react-dom"), "{sig}");
        assert!(!sig.contains("framework"), "{sig}");
        assert!(!sig.contains("commitHookEffectListMount"), "{sig}");
    }

    #[test]
    fn a_bundler_content_hash_does_not_split_a_group() {
        assert_eq!(strip_chunk_hash("index-9f2a.js"), "index.js");
        assert_eq!(strip_chunk_hash("index.4b71c8de.js"), "index.js");
        assert_eq!(strip_chunk_hash("framework-1a2b3c4d.js"), "framework.js");
    }

    #[test]
    fn an_ordinary_filename_keeps_every_part_of_its_name() {
        // The rule must not eat a name that merely ends in something short,
        // or two unrelated modules merge into one group.
        assert_eq!(strip_chunk_hash("server-2.js"), "server-2.js");
        assert_eq!(strip_chunk_hash("main.js"), "main.js");
        assert_eq!(strip_chunk_hash("Reporting.kt"), "Reporting.kt");
        assert_eq!(
            strip_chunk_hash("BridgeController.swift"),
            "BridgeController.swift"
        );
        // All letters, no digit: a word, not a digest.
        assert_eq!(strip_chunk_hash("index-beef.js"), "index-beef.js");
    }

    #[test]
    fn an_all_framework_stack_keeps_its_top_frame_rather_than_collapsing() {
        let stack = "    at node_modules/react-dom/index.js:2:2";
        let seen = Occurrence {
            stack: Some(stack.into()),
            ..occurrence(Source::Ui, "TypeError", "x")
        };
        assert!(!signature(&seen).is_empty());
    }

    #[test]
    fn a_kotlin_stack_parses() {
        let stack = "\
java.lang.IllegalStateException: nope
\tat app.gethomerun.mobile.Reporting.send(Reporting.kt:374)
\tat app.gethomerun.mobile.BridgeRouter.dispatch(BridgeRouter.kt:343)";
        let seen = Occurrence {
            stack: Some(stack.into()),
            ..occurrence(Source::Host, "kotlin.IllegalStateException", "nope")
        };
        let sig = signature(&seen);
        assert!(sig.contains("Reporting.kt"), "{sig}");
        assert!(!sig.contains("374"), "line numbers must not group: {sig}");
    }

    #[test]
    fn two_different_bugs_do_not_merge() {
        let one = Occurrence {
            stack: Some("    at A (https://h/a.js:1:1)".into()),
            ..occurrence(
                Source::Ui,
                "TypeError",
                "cannot read properties of undefined (reading 'players')",
            )
        };
        let two = Occurrence {
            stack: Some("    at B (https://h/b.js:1:1)".into()),
            ..occurrence(
                Source::Ui,
                "TypeError",
                "cannot read properties of undefined (reading 'ops')",
            )
        };
        assert_ne!(signature(&one), signature(&two));
    }

    #[test]
    fn a_property_name_survives_generalising() {
        // The field name is the bug's identity; ids and offsets are not.
        let one = occurrence(Source::Ui, "TypeError", "undefined (reading 'players')");
        let two = occurrence(Source::Ui, "TypeError", "undefined (reading 'ops')");
        assert_ne!(signature(&one), signature(&two));
    }

    #[test]
    fn ids_inside_a_message_are_generalised_away() {
        let one = occurrence(Source::Host, "Timeout", "server 4f2a1c0b7d3e timed out");
        let two = occurrence(Source::Host, "Timeout", "server 91ab2c3d4e5f timed out");
        assert_eq!(signature(&one), signature(&two));
    }

    #[test]
    fn a_location_carries_the_group_when_there_is_no_stack() {
        let seen = Occurrence {
            location: Some("/server/[id]".into()),
            ..occurrence(Source::Ui, "Error", "Script error.")
        };
        assert!(signature(&seen).contains("/server/[id]"));
    }

    // -- hashing ------------------------------------------------------------

    #[test]
    fn the_hash_is_stable_and_short() {
        let a = hash(Source::Ui, "TypeError", "a.js:Foo");
        assert_eq!(a.len(), LEN);
        assert_eq!(a, hash(Source::Ui, "TypeError", "a.js:Foo"));
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn each_part_of_the_hash_input_matters() {
        let base = hash(Source::Ui, "TypeError", "a.js:Foo");
        assert_ne!(base, hash(Source::Api, "TypeError", "a.js:Foo"));
        assert_ne!(base, hash(Source::Ui, "RangeError", "a.js:Foo"));
        assert_ne!(base, hash(Source::Ui, "TypeError", "b.js:Foo"));
    }

    #[test]
    fn the_separator_cannot_be_forged_from_a_field() {
        // `kind` and `signature` are attacker-influenced. A separator that
        // could appear inside one would let two distinct pairs collide.
        assert!(!SEP.is_ascii_graphic());
        let shifted = hash(Source::Ui, "Type", "Error\u{1f}a.js");
        assert_ne!(shifted, hash(Source::Ui, "TypeError", "a.js"));
    }

    #[test]
    fn a_signature_is_capped() {
        let seen = occurrence(Source::Host, "Error", &"x".repeat(5000));
        assert!(signature(&seen).len() <= MAX_SIGNATURE);
    }

    #[test]
    fn non_ascii_does_not_panic_anywhere() {
        let seen = Occurrence {
            stack: Some("    at 玩家 (https://h/файл.js:1:1)".into()),
            location: Some("/世界".into()),
            ..occurrence(Source::Ui, "TypeError", "mod “Térraforge” failed 12 times")
        };
        let sig = signature(&seen);
        assert!(!sig.is_empty());
        let _ = hash(Source::Ui, "TypeError", &sig);
    }
}

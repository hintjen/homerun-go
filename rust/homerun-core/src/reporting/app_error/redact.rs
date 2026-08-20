//! Removing secrets and personal data from an error report before it leaves
//! the device.
//!
//! # Why this is not [`crate::reporting::scrub`]
//!
//! `scrub` answers a different question about a different kind of text. It
//! reads Minecraft console lines, where the risk is *other people* — the
//! players on somebody's server, whose IP addresses and chat the operator
//! cannot consent away on their behalf.
//!
//! An error report is mostly the app talking about itself, and its risks are
//! the ones that come with that: a stack trace quotes the URL it was fetching,
//! and that URL carries an OAuth `code`; an HTTP body quotes the request that
//! failed, and that request carried an `Authorization` header; a file path
//! quotes the home directory, and a home directory is named after a person.
//! None of those appear in a console log, and none of `scrub`'s two rules
//! would catch them.
//!
//! What the two share is one hazard — an IP literal — and that scanner is
//! imported rather than rewritten. Two address scanners in one crate is how
//! one of them ends up missing bracketed IPv6.
//!
//! # No regex, for the same reason
//!
//! This crate depends on serde and one signature verifier. Hand-written
//! scanning is also independently right here, and more obviously so than in
//! `scrub`: the text being scanned includes HTTP response bodies from an API
//! anybody can call, and error messages built from strings a page put there.
//! A redaction pass is the wrong place for a regex whose behaviour on
//! adversarial input nobody has measured.
//!
//! # What is deliberately kept
//!
//! **UUIDs.** Device and server ids are already structured fields on the
//! report and are what make it actionable — "which install" and "which
//! server" are the first two questions anyone asks. Redacting them from prose
//! while sending them in a column beside it would be theatre.
//!
//! **Player names**, consistent with the judgement `scrub` already makes and
//! documents. That call is made in one place; this module defers to it.
//!
//! **API hostnames.** `api.gethomerun.app` versus a staging host is how a
//! report is attributed to a deployment, and the host is ours either way. A
//! host that is an IP *literal* is still redacted — that is somebody's home
//! network, not our infrastructure — which falls out of running the address
//! scanner last.
//!
//! # Order is load-bearing
//!
//! Each rule can contain the next. A bearer token can look like a path
//! segment; a URL query can contain an email; a home directory can appear
//! inside a URL. Running them in the order below means the widest match wins
//! and the narrower rules never see what the earlier ones already took.

use crate::reporting::scrub;

const TOKEN: &str = "[token redacted]";
const QUERY: &str = "[query redacted]";
const EMAIL: &str = "[email redacted]";
const USER: &str = "[user]";
const CONTAINER: &str = "[container]";

/// Below this, an `eyJ…` run is far more likely to be a word than a
/// credential. A real JWT header alone decodes to at least this much.
const MIN_JWT_LEN: usize = 40;

/// Run every rule, in the order the module header describes.
pub(crate) fn text(input: &str) -> String {
    let out = tokens(input);
    let out = url_queries(&out);
    let out = emails(&out);
    let out = home_dirs(&out);
    scrub::redact_addresses(&out)
}

// ---------------------------------------------------------------------------
// 1. Credentials
// ---------------------------------------------------------------------------

/// `Authorization` header values, `Bearer …` runs, and bare JWTs.
///
/// Three rules rather than one, because the credential appears three ways in
/// this codebase's own error text: the hosts build `Authorization: Bearer …`
/// and `Authorization: Access-Token …` by hand, the Rust device socket logs a
/// `bearer_auth` failure, and a decoded JWT can arrive on its own inside a
/// body that quoted it back.
fn tokens(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        if let Some(next) = auth_header(input, bytes, i, &mut out) {
            i = next;
            continue;
        }
        if let Some(next) = scheme_token(input, bytes, i, &mut out) {
            i = next;
            continue;
        }
        if let Some(next) = bare_jwt(bytes, i, &mut out) {
            i = next;
            continue;
        }
        i = push_char(input, &mut out, i);
    }

    out
}

/// `Authorization: <anything to end of line>`.
///
/// The widest of the three and therefore first: the value may be a scheme
/// this module has never heard of, and an unknown credential is still a
/// credential.
fn auth_header(input: &str, bytes: &[u8], i: usize, out: &mut String) -> Option<usize> {
    let after = match_ci(bytes, i, b"authorization")?;
    let mut j = after;
    while j < bytes.len() && bytes[j] == b' ' {
        j += 1;
    }
    if j >= bytes.len() || !matches!(bytes[j], b':' | b'=') {
        return None;
    }
    out.push_str(&input[i..=j]);
    out.push(' ');
    out.push_str(TOKEN);
    Some(line_end(bytes, j + 1))
}

/// `Bearer <run>` / `Access-Token <run>` — keep the scheme, lose the value.
///
/// The scheme is worth keeping on its own. "Which credential did this use" is
/// exactly the question behind a silent 403, and this codebase has two that
/// are not interchangeable.
fn scheme_token(input: &str, bytes: &[u8], i: usize, out: &mut String) -> Option<usize> {
    for scheme in [b"bearer ".as_slice(), b"access-token ".as_slice()] {
        let Some(after) = match_ci(bytes, i, scheme) else {
            continue;
        };
        let end = run_end(bytes, after, is_token_byte);
        if end > after {
            out.push_str(&input[i..after]);
            out.push_str(TOKEN);
            return Some(end);
        }
    }
    None
}

/// A JWT with nothing in front of it.
///
/// Every token this app handles is one, and they are recognisable without
/// knowing where they came from: `eyJ` is `{"` in base64url, so it is how
/// every encoded JSON header begins.
fn bare_jwt(bytes: &[u8], i: usize, out: &mut String) -> Option<usize> {
    if preceded_by_ident(bytes, i) {
        return None;
    }
    let after = match_ci(bytes, i, b"eyj")?;
    let end = run_end(bytes, after, is_base64url_byte);
    if end - i < MIN_JWT_LEN {
        return None;
    }
    out.push_str(TOKEN);
    Some(end)
}

// ---------------------------------------------------------------------------
// 2. URL query strings
// ---------------------------------------------------------------------------

/// Everything after `?` in an `http(s)://` URL.
///
/// Wholesale rather than per-parameter, and that is the important part. A
/// per-parameter allowlist means a query string this app has not shipped yet
/// arrives unredacted, and the ones it ships today are already the worst case:
/// the OAuth redirect carries `code`, `state` and `nonce`, and registration
/// carries an email address. There is no query parameter in this system whose
/// diagnostic value would survive the review that keeping it would need.
fn url_queries(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        let scheme = match_ci(bytes, i, b"https://").or_else(|| match_ci(bytes, i, b"http://"));
        if let Some(after) = scheme {
            if !preceded_by_ident(bytes, i) {
                let end = run_end(bytes, after, is_url_byte);
                let url = &input[i..end];
                match url.find('?') {
                    Some(q) => {
                        out.push_str(&url[..q]);
                        out.push('?');
                        out.push_str(QUERY);
                    }
                    None => out.push_str(url),
                }
                i = end;
                continue;
            }
        }
        i = push_char(input, &mut out, i);
    }

    out
}

// ---------------------------------------------------------------------------
// 3. Email addresses
// ---------------------------------------------------------------------------

/// `local@domain.tld`, wherever it appears.
///
/// The registration and claim endpoints quote the address back in their error
/// messages, so this is not hypothetical — it is the single most likely piece
/// of personal data in an API failure report.
///
/// A Matrix id (`@user:example.com`) is *not* an email and is not matched: the
/// scan requires at least one character before the `@`, and a Matrix id begins
/// with it. Deliberate — the owner's Matrix id is already a field on the
/// device this report is attributed to.
fn emails(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    // Input below this has already been replaced, so the local-part scan must
    // not walk back into it — `out` is only a byte-for-byte copy above here.
    let mut floor = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'@' {
            if let (Some(start), Some(end)) =
                (local_start(bytes, i, floor), domain_end(bytes, i + 1))
            {
                // Un-push the local part, which was copied verbatim.
                out.truncate(out.len() - (i - start));
                out.push_str(EMAIL);
                i = end;
                floor = i;
                continue;
            }
        }
        i = push_char(input, &mut out, i);
    }

    out
}

// ---------------------------------------------------------------------------
// 4. Home directories
// ---------------------------------------------------------------------------

/// The one path segment that is named after a person.
///
/// A stack trace from the desktop is full of `C:\Users\<name>\AppData\…`, and
/// the name is very often the person's real one. The rest of the path is kept
/// — it is how a file is found — and so is Android's
/// `/data/user/0/app.gethomerun.mobile/`, which carries no name and is
/// load-bearing for diagnosis.
///
/// iOS is the odd one out: its data container is a UUID that changes on every
/// install, so it identifies nothing *and* diagnoses nothing. It goes as
/// noise rather than as a secret.
fn home_dirs(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        if let Some(next) = ios_container(bytes, i, &mut out) {
            i = next;
            continue;
        }
        if let Some(next) = home_prefix(input, bytes, i, &mut out) {
            i = next;
            continue;
        }
        i = push_char(input, &mut out, i);
    }

    out
}

/// `/var/mobile/Containers/Data/Application/<UUID>` → `[container]`.
fn ios_container(bytes: &[u8], i: usize, out: &mut String) -> Option<usize> {
    let after = match_ci(bytes, i, b"/var/mobile/Containers/Data/Application/")?;
    let end = run_end(bytes, after, |b| b.is_ascii_hexdigit() || b == b'-');
    if end == after {
        return None;
    }
    out.push_str(CONTAINER);
    Some(end)
}

/// `C:\Users\<name>`, `/Users/<name>`, `/home/<name>` → the same with `[user]`.
fn home_prefix(input: &str, bytes: &[u8], i: usize, out: &mut String) -> Option<usize> {
    if preceded_by_ident(bytes, i) {
        return None;
    }

    let after = if i + 3 < bytes.len()
        && bytes[i].is_ascii_alphabetic()
        && bytes[i + 1] == b':'
        && matches!(bytes[i + 2], b'\\' | b'/')
    {
        // Any drive letter, either slash — a stack trace quotes both.
        match_ci(bytes, i + 3, b"users\\").or_else(|| match_ci(bytes, i + 3, b"users/"))?
    } else {
        match_ci(bytes, i, b"/users/").or_else(|| match_ci(bytes, i, b"/home/"))?
    };

    let end = run_end(bytes, after, |b| !matches!(b, b'/' | b'\\'));
    if end == after {
        return None;
    }
    out.push_str(&input[i..after]);
    out.push_str(USER);
    Some(end)
}

// ---------------------------------------------------------------------------
// Scanning helpers
// ---------------------------------------------------------------------------

/// Case-insensitively match `needle` at `i`; the offset just past it, or None.
fn match_ci(bytes: &[u8], i: usize, needle: &[u8]) -> Option<usize> {
    let end = i.checked_add(needle.len())?;
    if end > bytes.len() {
        return None;
    }
    bytes[i..end]
        .iter()
        .zip(needle)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
        .then_some(end)
}

/// The end of the run of bytes at `i` satisfying `keep`.
fn run_end(bytes: &[u8], i: usize, keep: impl Fn(u8) -> bool) -> usize {
    let mut end = i;
    while end < bytes.len() && keep(bytes[end]) {
        end += 1;
    }
    end
}

/// The end of the line containing `i`, not including the newline.
fn line_end(bytes: &[u8], i: usize) -> usize {
    run_end(bytes, i, |b| b != b'\n' && b != b'\r')
}

/// Push one whole char. Slicing mid-codepoint panics, and every field here
/// carries text somebody else wrote.
fn push_char(input: &str, out: &mut String, i: usize) -> usize {
    let ch = input[i..]
        .chars()
        .next()
        .expect("index is on a char boundary");
    out.push(ch);
    i + ch.len_utf8()
}

/// True when the byte before `i` could make this the tail of a longer word.
fn preceded_by_ident(bytes: &[u8], i: usize) -> bool {
    i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
}

fn is_base64url_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'=')
}

fn is_url_byte(b: u8) -> bool {
    !b.is_ascii_whitespace() && !matches!(b, b'"' | b'\'' | b'<' | b'>' | b'`' | b')' | b',')
}

fn is_email_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')
}

/// Walk back from an `@` over the local part. None when there is none, which
/// is what keeps Matrix ids out of this rule.
fn local_start(bytes: &[u8], at: usize, floor: usize) -> Option<usize> {
    let mut start = at;
    while start > floor && is_email_byte(bytes[start - 1]) {
        start -= 1;
    }
    (start < at).then_some(start)
}

/// Walk forward from just past an `@` over the domain. Requires an interior
/// dot, so `user@localhost` and a lone `@` ending a sentence do not match.
fn domain_end(bytes: &[u8], from: usize) -> Option<usize> {
    let end = run_end(bytes, from, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')
    });
    let dot = bytes[from..end].iter().position(|&b| b == b'.')?;
    (dot > 0 && end > from + dot + 1).then_some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens_lose_their_value_and_keep_their_scheme() {
        let out = text("failed with Bearer abc123.def456-ghi on retry");
        assert_eq!(out, "failed with Bearer [token redacted] on retry");
    }

    #[test]
    fn the_other_credential_scheme_goes_too() {
        // Two schemes that are not interchangeable; both are secrets.
        let out = text("Authorization: Access-Token 9f2a1c0b7d3e");
        assert!(!out.contains("9f2a"), "{out}");
    }

    #[test]
    fn a_whole_authorization_header_goes_even_when_the_scheme_is_unknown() {
        let out = text("Authorization: Weird-Scheme sekrit\nnext line survives");
        assert!(!out.contains("sekrit"), "{out}");
        assert!(out.contains("next line survives"), "{out}");
    }

    #[test]
    fn a_bare_jwt_is_recognised_without_a_scheme() {
        let jwt = format!("eyJ{}", "a".repeat(60));
        let out = text(&format!("token was {jwt} apparently"));
        assert!(!out.contains(&jwt), "{out}");
        assert!(out.contains("apparently"), "{out}");
    }

    #[test]
    fn a_short_eyj_word_is_left_alone() {
        // The rule is length-gated so it cannot eat ordinary text.
        assert_eq!(text("eyJshort"), "eyJshort");
    }

    #[test]
    fn a_query_string_goes_wholesale_and_the_path_stays() {
        let out = text("GET https://api.gethomerun.app/api/auth/?code=abc&state=xyz failed");
        assert_eq!(
            out,
            "GET https://api.gethomerun.app/api/auth/?[query redacted] failed"
        );
    }

    #[test]
    fn a_url_without_a_query_is_untouched() {
        let url = "https://api.gethomerun.app/api/server/9f2a/";
        assert_eq!(text(url), url);
    }

    #[test]
    fn the_api_host_survives_because_it_names_the_deployment() {
        assert!(text("https://api.fractalnetworks.co/api/x/").contains("api.fractalnetworks.co"));
    }

    #[test]
    fn emails_go() {
        let out = text("no account for player.one+tag@example.co.uk here");
        assert_eq!(out, "no account for [email redacted] here");
    }

    #[test]
    fn a_matrix_id_is_not_an_email() {
        // No local part before the `@`, so the rule declines. The owner's
        // Matrix id is already a field on the device.
        let out = text("owner is @steve:gethomerun.app");
        assert!(out.contains("@steve:gethomerun.app"), "{out}");
    }

    #[test]
    fn a_sentence_ending_dot_is_not_a_tld() {
        assert_eq!(text("mail user@host."), "mail user@host.");
    }

    #[test]
    fn windows_home_directories_lose_the_name() {
        let out = text(r"at C:\Users\Justin\AppData\Roaming\homerun\log.txt");
        assert_eq!(out, r"at C:\Users\[user]\AppData\Roaming\homerun\log.txt");
    }

    #[test]
    fn unix_home_directories_lose_the_name() {
        assert_eq!(text("/home/justin/.config/x"), "/home/[user]/.config/x");
        assert_eq!(text("/Users/justin/Library/x"), "/Users/[user]/Library/x");
    }

    #[test]
    fn the_android_data_dir_is_kept_because_it_names_no_one() {
        let path = "/data/user/0/app.gethomerun.mobile/files/ui";
        assert_eq!(text(path), path);
    }

    #[test]
    fn the_ios_container_uuid_goes_as_noise() {
        let out = text("/var/mobile/Containers/Data/Application/1E2A-4B/Documents/x");
        assert_eq!(out, "[container]/Documents/x");
    }

    #[test]
    fn ip_literals_go_through_the_shared_scanner() {
        let out = text("connect to 203.0.113.4:25565 refused");
        assert!(!out.contains("203.0.113.4"), "{out}");
    }

    #[test]
    fn a_url_whose_host_is_an_ip_loses_the_host() {
        // Somebody's home network, not our infrastructure. Falls out of
        // running the address scanner after the URL rule.
        let out = text("http://192.168.1.50:8000/api/");
        assert!(!out.contains("192.168.1.50"), "{out}");
    }

    #[test]
    fn an_email_inside_a_query_is_already_gone_before_the_email_rule_runs() {
        let out = text("https://api.gethomerun.app/register/?email=someone@example.com");
        assert!(!out.contains("someone"), "{out}");
        assert!(out.contains("[query redacted]"), "{out}");
    }

    #[test]
    fn a_home_directory_inside_a_stack_frame_still_goes() {
        let out = text("TypeError\n    at fn (C:\\Users\\Sam\\app\\main.js:1:2)");
        assert!(!out.contains("Sam"), "{out}");
        assert!(out.contains("main.js"), "{out}");
    }

    #[test]
    fn non_ascii_text_does_not_panic_and_survives() {
        let out = text("mod “Térraforge” failed for 玩家 at /home/josé/x");
        assert!(out.contains("Térraforge"), "{out}");
        assert!(out.contains("玩家"), "{out}");
        assert!(!out.contains("josé"), "{out}");
    }

    #[test]
    fn ordinary_prose_is_left_alone() {
        let plain = "The server stopped because the world could not be saved.";
        assert_eq!(text(plain), plain);
    }

    #[test]
    fn empty_input_is_fine() {
        assert_eq!(text(""), "");
    }
}

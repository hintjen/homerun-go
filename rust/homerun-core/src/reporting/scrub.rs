//! Removing other people's personal data from console output before it leaves
//! the device.
//!
//! # Why this exists
//!
//! A crash report uploads the tail of a Minecraft server's console. That
//! console is not the host's own diagnostic log — it is a record of *other
//! people*, and the server's operator is usually a teenager hosting for
//! friends. Every join line carries a player's IP address. Every chat line
//! carries whatever they typed. None of those people installed this app, saw a
//! consent screen, or have any idea a crash sends their words to a company.
//!
//! Crash reports are worth having, and this module is what makes them worth
//! having *and* defensible: the lines that diagnose a crash — stack traces,
//! mod load failures, out-of-memory kills — survive intact, and the lines that
//! are only about somebody's afternoon do not.
//!
//! # What is redacted, and what deliberately is not
//!
//! **IP addresses** and **chat**, which is what an operator cannot consent to
//! on their friends' behalf.
//!
//! Player *names* survive. A crash that only happens when a particular player
//! joins is diagnosable and nearly impossible to describe without saying who,
//! a username is already public to everyone on that server, and it is not the
//! thing that locates somebody in the world. Names ride into the report on
//! `stats` anyway, where they are load-bearing for the leaderboard the player
//! signed up for. If that judgement is ever revisited, revisit it here — this
//! is the one place it is made.
//!
//! # No regex
//!
//! This crate depends on serde and (for signature verification) ed25519-dalek,
//! and nothing else. Hand-written scanning is also the right call
//! independently: the patterns are simple, and a redaction pass is exactly the
//! wrong place for a regex whose behaviour on adversarial input nobody has
//! measured. Console lines are attacker-influenced by definition — anyone on
//! the server can type one.

/// What replaces an address. Deliberately visible: a reader who sees a crash
/// they cannot diagnose should be able to tell that something was removed
/// rather than that the server never logged it.
const IP: &str = "[ip redacted]";

/// What replaces the body of a message.
const CHAT: &str = "[chat redacted]";

/// Scrub one console line.
///
/// Order matters: chat is handled first and takes the whole rest of the line,
/// because a message body can contain anything — including something that
/// looks like an address, which the IP pass would otherwise "redact" while
/// leaving the rest of the sentence intact.
pub fn console_line(line: &str) -> String {
    match redact_message(line) {
        Some(redacted) => redacted,
        None => redact_addresses(line),
    }
}

/// Scrub every line, for a caller about to upload them.
pub fn console_lines<S: AsRef<str>>(lines: &[S]) -> Vec<String> {
    lines.iter().map(|l| console_line(l.as_ref())).collect()
}

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

/// Player-authored text, replaced from the point it starts to end of line.
///
/// Four shapes, all of them things a player types:
///
/// ```text
/// [12:00:00] [Server thread/INFO]: <Steve> anything at all
/// [12:00:00] [Server thread/INFO]: [Not Secure] <Steve> anything at all
/// [12:00:00] [Server thread/INFO]: Steve whispers to Alex: anything at all
/// [12:00:00] [Server thread/INFO]: * Steve waves
/// ```
///
/// The prefix is kept because it is the part that says *when* and *which
/// thread*, which is what makes a crash line up with a chat burst. The name is
/// kept for the reason in the module docs. Everything after it goes.
fn redact_message(line: &str) -> Option<String> {
    if let Some(name_end) = chat_name_end(line) {
        return Some(format!("{} {}", &line[..name_end], CHAT));
    }

    // `* name emote`
    if let Some(star) = line.find(": * ") {
        return Some(format!("{} {}", &line[..star + 3], CHAT));
    }

    // `name whispers to other: message`. The recipient is a name too, and the
    // fact that a whisper happened is worth keeping — the words are not.
    if let Some(marker) = line.find(" whispers to ") {
        if let Some(colon) = line[marker..].find(": ") {
            let body = marker + colon + 1;
            return Some(format!("{} {}", &line[..body], CHAT));
        }
    }

    None
}

/// The index just past the `>` of a chat line's `<name>`, if this is one.
///
/// Matching on `": <"` was the obvious approach and it is wrong twice: it
/// misses `[Not Secure] <Steve>`, which is what a vanilla 1.19+ server prints
/// for every unsigned message, and it would still need a guard against
/// `List<String>` in a stack trace.
///
/// So the shape is what identifies it. A chat prefix is `<name>` where the
/// `<` opens a word — preceded by a space or nothing — the name has no spaces
/// and is short, and a space follows the `>`. A Java generic fails the first
/// test, because its `<` is always welded to an identifier.
fn chat_name_end(line: &str) -> Option<usize> {
    /// Bedrock names via Geyser run longer than Java's 16, and a prefix plugin
    /// can pad one. Beyond this it is not a name and this is not chat.
    const MAX_NAME: usize = 48;

    let bytes = line.as_bytes();
    for (i, _) in line.char_indices().filter(|(_, c)| *c == '<') {
        let opens_a_word = i == 0 || bytes[i - 1] == b' ';
        if !opens_a_word {
            continue;
        }
        let Some(close) = line[i + 1..].find('>').map(|c| i + 1 + c) else {
            continue;
        };
        let name = &line[i + 1..close];
        if name.is_empty() || name.len() > MAX_NAME || name.contains(' ') || name.contains('<') {
            continue;
        }
        // A trailing `>` with nothing after it is not a message; leave the
        // line to the address pass rather than claiming it.
        if line[close + 1..].starts_with(' ') {
            return Some(close + 1);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Replace every IPv4 and bracketed IPv6 literal, and any `:port` after one.
///
/// The join line this exists for looks like:
///
/// ```text
/// Steve[/203.0.113.4:52341] logged in with entity id 42 at (…)
/// ```
fn redact_addresses(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;

    while i < bytes.len() {
        // A bracketed IPv6 literal: `[::1]`, `[2001:db8::1]:25565`.
        if bytes[i] == b'[' {
            if let Some(end) = ipv6_bracket_end(line, i) {
                out.push_str(IP);
                i = skip_port(bytes, end);
                continue;
            }
        }

        if bytes[i].is_ascii_digit() && !preceded_by_ident(bytes, i) {
            if let Some(end) = ipv4_end(line, i) {
                out.push_str(IP);
                i = skip_port(bytes, end);
                continue;
            }
        }

        // Push one whole char, not one byte: slicing mid-codepoint panics, and
        // chat has already been removed but names and mod titles have not.
        let ch = line[i..]
            .chars()
            .next()
            .expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

/// True if the byte before `i` could make this the tail of an identifier or a
/// longer number — `entity id 42`, `1.21.4-rc1`, a hex hash.
///
/// Without this, the `42` in a coordinate could start a match and the version
/// `10.0.0.1` inside a mod name would be eaten. Being conservative here costs
/// nothing: a real address is preceded by `/`, `(`, a space or a bracket.
fn preceded_by_ident(bytes: &[u8], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let prev = bytes[i - 1];
    prev.is_ascii_alphanumeric() || prev == b'.' || prev == b'-' || prev == b'_'
}

/// The index just past a dotted quad starting at `start`, if there is one.
///
/// Every octet must be 1-3 digits and no more than 255, which is what keeps
/// this off version strings and dates: `2026.08.13.1` fails on the first
/// octet, and `1.21.4` has too few.
fn ipv4_end(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = start;

    for octet in 0..4 {
        if octet > 0 {
            if i >= bytes.len() || bytes[i] != b'.' {
                return None;
            }
            i += 1;
        }
        let digits_from = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() && i - digits_from < 3 {
            i += 1;
        }
        if i == digits_from {
            return None;
        }
        if line[digits_from..i].parse::<u16>().ok()? > 255 {
            return None;
        }
    }

    // A fifth dotted number means this was never an address.
    if i < bytes.len() && bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    Some(i)
}

/// The index just past `[…]` when it holds something that looks like an IPv6
/// literal: hex digits, colons, and at least one colon.
fn ipv6_bracket_end(line: &str, start: usize) -> Option<usize> {
    let close = line[start..].find(']')? + start;
    let inner = &line[start + 1..close];
    if inner.is_empty() {
        return None;
    }
    if !inner
        .bytes()
        .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
    {
        return None;
    }

    // **A log timestamp is `[12:00:00]`**, which passes every test above: all
    // its characters are hex digits and colons. Eating it would blank the
    // start of every line in the file — which is exactly what the first draft
    // of this did, and what `brackets_that_are_not_addresses_survive` caught.
    //
    // What separates them is that IPv6 is either abbreviated, and so contains
    // `::`, or written in full, and so has seven colons. A timestamp has two
    // and no run.
    let colons = inner.bytes().filter(|b| *b == b':').count();
    if !inner.contains("::") && colons < 7 {
        return None;
    }
    Some(close + 1)
}

/// Skip a `:port` immediately after an address, so the port does not survive
/// as a bare number attached to the placeholder.
fn skip_port(bytes: &[u8], mut i: usize) -> usize {
    if i < bytes.len() && bytes[i] == b':' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line this whole module exists for.
    #[test]
    fn a_join_line_loses_the_address_and_keeps_everything_else() {
        let line = "[12:00:00] [Server thread/INFO]: Steve[/203.0.113.4:52341] logged in with entity id 42 at (1.5, 64.0, -8.5)";
        let scrubbed = console_line(line);
        assert!(!scrubbed.contains("203.0.113.4"), "{scrubbed}");
        assert!(
            !scrubbed.contains("52341"),
            "the port is part of the address: {scrubbed}"
        );
        assert!(
            scrubbed.contains("Steve"),
            "the name is deliberately kept: {scrubbed}"
        );
        assert!(
            scrubbed.contains("logged in with entity id 42"),
            "{scrubbed}"
        );
        assert!(
            scrubbed.contains("(1.5, 64.0, -8.5)"),
            "coordinates are not addresses: {scrubbed}"
        );
    }

    #[test]
    fn chat_is_replaced_whole() {
        let line = "[12:00:00] [Server thread/INFO]: <Steve> my address is 10.0.0.7 come over";
        let scrubbed = console_line(line);
        assert_eq!(
            scrubbed,
            "[12:00:00] [Server thread/INFO]: <Steve> [chat redacted]"
        );
        // Belt and braces: the IP inside the message went with the message.
        assert!(!scrubbed.contains("10.0.0.7"));
    }

    #[test]
    fn the_other_things_a_player_can_type() {
        for line in [
            "[12:00:00] [Server thread/INFO]: [Not Secure] <Steve> hello",
            "[12:00:00] [Server thread/INFO]: * Steve waves at everyone",
            "[12:00:00] [Server thread/INFO]: Steve whispers to Alex: meet me at mine",
        ] {
            let scrubbed = console_line(line);
            assert!(scrubbed.contains(CHAT), "not redacted: {scrubbed}");
            assert!(
                !scrubbed.contains("hello")
                    && !scrubbed.contains("waves at")
                    && !scrubbed.contains("meet me"),
                "message body survived: {scrubbed}"
            );
        }
    }

    /// The whole point of redacting rather than dropping the line.
    #[test]
    fn a_stack_trace_is_untouched() {
        for line in [
            "[12:00:00] [Server thread/ERROR]: java.lang.OutOfMemoryError: Java heap space",
            "\tat net.minecraft.server.MinecraftServer.run(MinecraftServer.java:1041)",
            "[12:00:00] [main/INFO]: Loading Minecraft 1.21.4 with Fabric Loader 0.16.9",
            "Caused by: java.util.List<String> cannot be cast to java.lang.String",
        ] {
            assert_eq!(console_line(line), line, "a diagnostic line was altered");
        }
    }

    /// Version strings and dates are dotted numbers too. Redacting one of them
    /// would make a crash undiagnosable for no privacy gain at all.
    #[test]
    fn dotted_numbers_that_are_not_addresses_survive() {
        for line in [
            "Starting minecraft server version 1.21.4",
            "bundle 2026.08.13.1 staged",
            "took 12.5s",
            "at (1.5, 64.0, -8.5)",
        ] {
            assert_eq!(console_line(line), line, "over-redacted: {line}");
        }
    }

    #[test]
    fn ipv6_goes_too() {
        let scrubbed = console_line("Disconnecting Steve (/[2001:db8::1]:25565): timed out");
        assert!(!scrubbed.contains("2001"), "{scrubbed}");
        assert!(scrubbed.contains("timed out"), "{scrubbed}");
        assert!(scrubbed.contains("Steve"), "{scrubbed}");
    }

    /// A log prefix is `[12:00:00]`, and a mod list is `[a, b]`. Neither is an
    /// address, and eating them would gut every line in the file.
    #[test]
    fn brackets_that_are_not_addresses_survive() {
        let line = "[12:00:00] [Server thread/INFO]: mods [alpha, beta]";
        assert_eq!(console_line(line), line);
    }

    #[test]
    fn several_addresses_on_one_line() {
        let scrubbed = console_line("proxy 10.0.0.1:25565 -> backend 10.0.0.2:25566 ok");
        assert_eq!(scrubbed, "proxy [ip redacted] -> backend [ip redacted] ok");
    }

    /// Console lines are attacker-influenced by definition — anyone on the
    /// server can type one — so the scrubber must not panic on any of them.
    #[test]
    fn survives_hostile_input() {
        for line in [
            "",
            ":",
            "<",
            ": <unclosed",
            "[",
            "[::",
            "1.2.3.",
            "999.999.999.999",
            "こんにちは 1.2.3.4 さようなら",
            "<Steve> 🎮🎮🎮",
        ] {
            let _ = console_line(line);
        }
        // The multi-byte cases matter most: slicing mid-codepoint panics.
        let scrubbed = console_line("こんにちは 1.2.3.4 さようなら");
        assert!(scrubbed.contains("こんにちは"), "{scrubbed}");
        assert!(!scrubbed.contains("1.2.3.4"), "{scrubbed}");
    }

    #[test]
    fn scrubbing_a_whole_console() {
        let lines = vec![
            "[12:00:00] [Server thread/INFO]: Steve[/203.0.113.4:52341] logged in",
            "[12:00:01] [Server thread/INFO]: <Steve> hi",
            "[12:00:02] [Server thread/ERROR]: java.lang.OutOfMemoryError",
        ];
        let scrubbed = console_lines(&lines);
        assert_eq!(scrubbed.len(), 3);
        assert!(!scrubbed[0].contains("203.0.113.4"));
        assert!(!scrubbed[1].contains("hi"));
        assert_eq!(scrubbed[2], lines[2]);
    }
}

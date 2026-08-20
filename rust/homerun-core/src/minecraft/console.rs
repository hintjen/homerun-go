//! Reading a Minecraft server's console: the ring buffer the UI pages
//! through, and the few lines that mean something.
//!
//! Reference: `JavaServerBackend` (Android) and the supervisor's log handling.
//!
//! Parsing the console rather than opening RCON is deliberate. Vanilla prints
//! joins and leaves, and reading them costs no port, no password and no second
//! protocol to keep alive. The trade-off is that a modded server may word
//! things differently, so everything here is best-effort and nothing blocks on
//! it — a roster that misses a player is survivable, a launch that hangs
//! waiting for one is not.

use std::borrow::Cow;
use std::collections::VecDeque;

const ESC: u8 = 0x1b;

/// A line with its colour codes taken out, borrowed when there were none.
///
/// Paper colours its console — including the join and leave messages — so on
/// most real servers every line below arrives wrapped in `ESC [ 0;39;22 m`.
/// The escape byte is optional here on purpose: some log paths (a pty stripped
/// of control characters, a Java `PrintStream` that dropped it) deliver the
/// remnant `[0m` as plain text, and the desktop learned the same thing.
///
/// This runs on every line during world generation, a few hundred a second, so
/// the clean case must not allocate — hence [`Cow`] and a single pass.
pub fn strip_ansi(line: &str) -> Cow<'_, str> {
    let bytes = line.as_bytes();
    let mut out: Option<String> = None;
    let (mut copied, mut at) = (0, 0);
    while at < bytes.len() {
        // Multi-byte UTF-8 is all >= 0x80, so no sequence can start mid-char
        // and every index below lands on a character boundary.
        if bytes[at] == ESC || bytes[at] == b'[' {
            if let Some(len) = ansi_len(bytes, at) {
                let out = out.get_or_insert_with(|| String::with_capacity(line.len()));
                out.push_str(&line[copied..at]);
                at += len;
                copied = at;
                continue;
            }
        }
        at += 1;
    }
    match out {
        Some(mut owned) => {
            owned.push_str(&line[copied..]);
            Cow::Owned(owned)
        }
        None => Cow::Borrowed(line),
    }
}

/// The length of the colour sequence starting at `at`, if one does.
///
/// Only SGR (`m`) is matched. Cursor moves and the like do not appear in a
/// server log, and matching them too would risk eating real text — `[20:37:28`
/// is one character away from looking like a sequence already.
fn ansi_len(bytes: &[u8], at: usize) -> Option<usize> {
    let mut end = at;
    if bytes.get(end) == Some(&ESC) {
        end += 1;
    }
    if bytes.get(end) != Some(&b'[') {
        return None;
    }
    end += 1;
    while matches!(bytes.get(end), Some(b'0'..=b'9' | b';')) {
        end += 1;
    }
    (bytes.get(end) == Some(&b'm')).then_some(end + 1 - at)
}

/// The server is accepting connections. Two spellings, because the engines
/// word it differently:
///
/// ```text
/// Done (12.345s)! For help, type "help"          // vanilla, Paper, the loaders
/// Server is now running. Connect using port: ... // Pumpkin
/// ```
///
/// Both are matched loosely: the first because the timing text moves, the
/// second because everything after the port is colour codes and edition names.
///
/// **Pumpkin's line is load-bearing, and it was missing.** A child process
/// reaches `on_ready` only through here, so a Pumpkin server announced
/// nothing, never left `starting`, and failed its launch on a timeout — with
/// a healthy server accepting players the whole time. The linked engine
/// announces readiness itself and never consults this, so nothing about the
/// gap was visible until Pumpkin was run as a process.
pub fn is_ready(line: &str) -> bool {
    let clean = strip_ansi(line);
    if clean.contains("Server is now running") {
        return true;
    }
    let Some(rest) = clean.split_once("Done (").map(|(_, r)| r) else {
        return false;
    };
    rest.split_once(')')
        .is_some_and(|(_, after)| after.trim_start().starts_with("! For help"))
}

/// The player named in a join line, if this is one.
pub fn joined(line: &str) -> Option<&str> {
    player_before(line, " joined the game")
}

/// The player named in a leave line, if this is one.
pub fn left(line: &str) -> Option<&str> {
    player_before(line, " left the game")
}

/// The player ceiling, if this line announces one.
///
/// The denominator in "3 / 20", and the console is where a host learns it,
/// because what the player *asked for* and what the server is **running with**
/// are not always the same — a rejected `server.properties` value, or a plugin
/// overriding it, and the file no longer says.
///
/// Several spellings, because the source varies: a properties dump prints
/// `max-players=20`, some loaders log `maxPlayers: 20` on boot. Same fact,
/// not worth a second parser.
///
/// Best-effort like everything else here — an unrecognised line yields None,
/// which renders as unknown rather than as a ceiling of zero.
pub fn max_players(line: &str) -> Option<u32> {
    let clean = strip_ansi(line);
    let lower = clean.to_ascii_lowercase();
    let after = ["max-players", "maxplayers"]
        .iter()
        .find_map(|key| lower.find(key).map(|at| at + key.len()))?;

    let digits: String = clean[after..]
        .trim_start_matches([':', '=', ' '])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Strip a console line's log prefix, whatever shape the engine writes it in.
///
/// Two are in use, and they do not look alike:
///
/// ```text
/// [HH:MM:SS] [Server thread/INFO]: Notch joined the game   // vanilla, Paper
/// [INFO] Kologgs joined the game                           // Pumpkin
/// ```
///
/// A prefix is a run of bracketed tags that **look like log tags** — a
/// timestamp, or something naming a level — each followed by a space or by
/// `: `. Consuming exactly those is what makes the two formats one case.
///
/// This used to split on the first `]: `, which Pumpkin never writes: the
/// whole prefix stayed in the name, so on that engine every join and leave
/// went unrecognised and presence reporting was silently dead.
///
/// The tag test is the security property, and it is not decoration. Consuming
/// *any* bracketed run instead would read a chat author as a prefix, and then
/// `[Griefer] Notch joined the game` typed into chat forges a join. Splitting
/// on the last `]: ` has the same hole with `hey]: Notch`. What the two
/// engines' prefixes have in common is that they announce a level or a time,
/// and what a griefer types does not.
fn after_log_prefix(line: &str) -> &str {
    fn is_log_tag(tag: &str) -> bool {
        let upper = tag.to_ascii_uppercase();
        if ["INFO", "WARN", "ERROR", "DEBUG", "TRACE", "FATAL"]
            .iter()
            .any(|level| upper.contains(level))
        {
            return true;
        }
        // A bare timestamp: `[20:37:28]`, or an ISO one from a `tracing`
        // subscriber. Digits and separators only, and never empty.
        !tag.is_empty()
            && tag
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | '-' | '+' | 'T' | 'Z'))
    }

    let mut rest = line.trim_start();
    while let Some(open) = rest.strip_prefix('[') {
        let Some(close) = open.find(']') else { break };
        if !is_log_tag(&open[..close]) {
            break;
        }
        let tail = &open[close + 1..];
        // `]: ` on vanilla, `] ` on Pumpkin. A tag butted straight against
        // text is not a prefix — it is text that happens to start with one.
        let tail = tail.strip_prefix(':').unwrap_or(tail);
        let Some(after) = tail.strip_prefix(' ') else {
            break;
        };
        rest = after.trim_start();
    }
    unbracketed(rest).unwrap_or(rest)
}

/// The other shape the same engine writes, and the one that actually reaches
/// a linked host: `tracing`'s default event format.
///
/// ```text
/// 2026-08-13 10:36:00  INFO tokio-rt-worker ThreadId(120) pumpkin::world: Kologgs joined the game
/// ```
///
/// Pumpkin's *file* log is `[INFO] …` and its *stdout* is this, which is a
/// good reminder to read the stream you actually consume — a host that links
/// the engine captures fd 1, not `logs/latest.log`. The thread name and id are
/// there because `logging.threads` is on, and the target because
/// `with_target(true)` is; neither is guaranteed, so the shape between the
/// level and the message is not worth enumerating.
///
/// What *is* reliable is that the **first** `": "` ends the prefix — the same
/// property vanilla's first-`]: ` rule leans on. Nothing before it can contain
/// one: a timestamp's colons are followed by digits, and a `tracing` target's
/// `::` by a letter.
///
/// Anchored at the start and applied once. A level word further along belongs
/// to whatever was typed, and a chat author's `<…>` before the separator means
/// this is a message rather than a prefix.
fn unbracketed(rest: &str) -> Option<&str> {
    const LEVELS: [&str; 6] = ["INFO", "WARN", "ERROR", "DEBUG", "TRACE", "FATAL"];

    // Whatever precedes the level may only be a timestamp.
    let level_at = LEVELS
        .iter()
        .filter_map(|level| rest.find(level).map(|at| (at, level.len())))
        .filter(|&(at, _)| {
            rest[..at]
                .chars()
                .all(|c| c.is_whitespace() || c.is_ascii_digit() || matches!(c, ':' | '.' | '-'))
        })
        .min_by_key(|&(at, _)| at)?;

    let after_level = &rest[level_at.0 + level_at.1..];
    let payload = after_level.trim_start();
    match payload.split_once(": ") {
        // A chat author reaching the separator means the `: ` was typed, not
        // logged — without this, a message of `hey: Notch joined the game`
        // would forge a join on a build with no target in its format.
        Some((preamble, message)) if !preamble.contains(['<', '>']) => Some(message.trim_start()),
        _ => Some(payload),
    }
}

/// Pull the name immediately before a marker, after the log prefix.
///
/// What follows a real join line's prefix is one bare name and nothing else.
/// A chat message puts its author in front (`<Griefer> Notch joined the
/// game`), so requiring the remainder to be a lone name is what tells the two
/// apart — the desktop does not attempt this distinction at all, and its
/// presence check `/ \S+ (?:joined|left) the game\s*$/` is satisfied by any
/// chat message.
fn player_before<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let head = line.split_once(marker)?.0;
    let clean = strip_ansi(head);
    let name = after_log_prefix(&clean).trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    // Answer with a slice of the caller's line rather than of the stripped
    // copy. The name survives stripping verbatim unless a colour code was
    // printed inside it, which no server does and which is not worth trusting
    // a reconstruction for.
    let at = head.rfind(name)?;
    Some(&head[at..at + name.len()])
}

/// A bounded console with a monotonic cursor.
///
/// The cursor keeps counting past what is still held, so a UI that was away
/// longer than the buffer is deep gets the oldest lines that survive rather
/// than a panic or a silent replay from the start.
#[derive(Debug)]
pub struct Console {
    lines: VecDeque<String>,
    first_index: usize,
    capacity: usize,
}

/// One slice of console, and where to ask from next.
#[derive(Debug, PartialEq, Eq)]
pub struct Slice {
    pub lines: Vec<String>,
    pub cursor: usize,
}

impl Console {
    pub fn new(capacity: usize) -> Self {
        Console {
            lines: VecDeque::with_capacity(capacity.min(1024)),
            first_index: 0,
            capacity: capacity.max(1),
        }
    }

    pub fn record(&mut self, line: impl Into<String>) {
        self.lines.push_back(line.into());
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
            self.first_index += 1;
        }
    }

    /// Everything since `cursor`, plus the cursor for next time.
    pub fn since(&self, cursor: usize) -> Slice {
        let from = cursor
            .saturating_sub(self.first_index)
            .min(self.lines.len());
        Slice {
            lines: self.lines.iter().skip(from).cloned().collect(),
            cursor: self.first_index + self.lines.len(),
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.first_index = 0;
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_is_recognised_as_printed() {
        assert!(is_ready(
            "[20:37:28] [Server thread/INFO]: Done (3.244s)! For help, type \"help\""
        ));
        assert!(is_ready(
            "[20:49:56 INFO]: Done (21.759s)! For help, type \"help\""
        ));
    }

    #[test]
    fn pumpkin_announces_readiness_in_its_own_words() {
        // Pumpkin never prints "Done (...)". A child process reaches
        // `on_ready` only through `is_ready`, so without this the state
        // machine sat in `starting` until the launch timed out, while a
        // perfectly healthy server accepted players behind it.
        //
        // The real line, with the colour codes Pumpkin wraps the edition
        // and the address in.
        assert!(is_ready(
            "[INFO] Server is now running. Connect using port: \u{1b}[33;22mJava Edition:\u{1b}[m \u{1b}[34;22m0.0.0.0:25565\u{1b}[m"
        ));
        // Both editions enabled, so the tail carries a separator.
        assert!(is_ready(
            "[INFO] Server is now running. Connect using port: Java Edition: 0.0.0.0:25565 | Bedrock Edition: 0.0.0.0:19132"
        ));
    }

    #[test]
    fn a_bound_server_is_not_an_accepting_one() {
        // `PumpkinServer::new` binds and `start()` accepts; this is printed
        // between them, so announcing on it would report a server that
        // cannot be joined yet.
        assert!(!is_ready("[INFO] Started server; took 1234ms"));
        assert!(!is_ready("[INFO] Loaded 1 plugin"));
    }

    #[test]
    fn other_lines_mentioning_done_are_not_ready() {
        assert!(!is_ready("[Server thread/INFO]: Done loading datapacks"));
        assert!(!is_ready(
            "[Server thread/INFO]: Preparing spawn area: 100%"
        ));
        assert!(!is_ready("Done (3.2s) with something else"));
    }

    #[test]
    fn joins_and_leaves_name_the_player() {
        assert_eq!(
            joined("[20:37:28] [Server thread/INFO]: Notch joined the game"),
            Some("Notch")
        );
        assert_eq!(
            left("[20:37:28] [Server thread/INFO]: Player_1 left the game"),
            Some("Player_1")
        );
        assert_eq!(joined("[Server thread/INFO]: Notch left the game"), None);
    }

    /// Anyone can type "Notch joined the game" in chat. Vanilla wraps a chat
    /// author in angle brackets, which is what tells the two apart.
    #[test]
    fn chat_cannot_forge_a_join() {
        assert_eq!(
            joined("[20:37:28] [Server thread/INFO]: <Griefer> Notch joined the game"),
            None
        );
    }

    /// Paper colours its join/leave messages, which is most servers, so a
    /// parser that only reads plain lines sees no joins at all on them.
    #[test]
    fn a_coloured_paper_join_names_the_player() {
        assert_eq!(
            joined(
                "\u{1b}[m\u{1b}[36;22m[20:37:28 INFO]: \u{1b}[m\u{1b}[0;39;22mNotch joined the game\u{1b}[m"
            ),
            Some("Notch")
        );
    }

    /// Some log paths lose the escape byte and leave the rest of the sequence
    /// behind as text.
    #[test]
    fn a_colour_code_that_lost_its_escape_byte_is_still_ignored() {
        assert_eq!(
            left("[m[36;22m[20:37:28 INFO]: [m[0;39;22mPlayer_1 left the game[m"),
            Some("Player_1")
        );
    }

    /// The log prefix ends at the *first* `]: `; anything after that is text
    /// somebody may have typed. Taking the last one lets a chat message carry
    /// its own fake prefix and be read as a join.
    #[test]
    fn chat_cannot_forge_a_join_by_typing_a_log_prefix() {
        assert_eq!(
            joined("[20:37:28] [Server thread/INFO]: <Griefer> hey]: Notch joined the game"),
            None
        );
        assert_eq!(
            left("[20:37:28] [Server thread/INFO]: [Griefer] hey]: Notch left the game"),
            None
        );
    }

    /// Pumpkin's console, which is `tracing` rather than Minecraft's own
    /// logger: one bracketed level and no `]: ` anywhere.
    ///
    /// Read from a real `logs/latest.log` on a phone after a player joined.
    /// Before this, the prefix stayed in the name, `joined` answered `None`,
    /// and the API learned about a player up to two minutes late — or never,
    /// for a session shorter than the reporting interval.
    #[test]
    fn a_pumpkin_join_names_the_player() {
        assert_eq!(joined("[INFO] Kologgs joined the game"), Some("Kologgs"));
        assert_eq!(left("[INFO] Kologgs left the game"), Some("Kologgs"));
    }

    /// The same engine's *stdout*, which is what a linked host captures —
    /// `tracing`'s default format, with a target and no brackets at all.
    ///
    /// The file log and the console are two different formatters in Pumpkin,
    /// and only this one reaches `push_log`.
    #[test]
    fn a_tracing_join_names_the_player() {
        // Captured from a phone's console, thread name and id included —
        // two reconstructions of this line from the engine's source were
        // wrong before someone read the real one.
        assert_eq!(
            joined(
                "2026-08-13 10:36:00  INFO tokio-rt-worker ThreadId(120) pumpkin::world: Kologgs joined the game"
            ),
            Some("Kologgs")
        );
        assert_eq!(
            left(
                "2026-08-13 10:36:00  INFO tokio-rt-worker ThreadId(120) pumpkin::world: Player_1 left the game"
            ),
            Some("Player_1")
        );
        // Neither the thread fields nor the target is guaranteed to be on.
        assert_eq!(
            joined("2026-08-13 15:12:31  INFO pumpkin::world: Notch joined the game"),
            Some("Notch")
        );
        assert_eq!(
            joined(" INFO pumpkin::world: Notch joined the game"),
            Some("Notch")
        );
    }

    /// With no target in the format there is no separator of the host's own,
    /// so a typed one must not be mistaken for it.
    #[test]
    fn chat_cannot_forge_a_join_with_its_own_separator() {
        assert_eq!(joined(" INFO <Griefer> hey: Notch joined the game"), None);
    }

    /// The level word only ends a prefix when nothing but a timestamp came
    /// before it. Otherwise a chat message could carry its own.
    #[test]
    fn chat_cannot_forge_a_join_by_typing_a_tracing_prefix() {
        assert_eq!(
            joined(
                "2026-08-13 15:12:31  INFO pumpkin::world: <Griefer> INFO x: Notch joined the game"
            ),
            None
        );
        assert_eq!(
            joined("[20:37:28] [Server thread/INFO]: <Griefer> INFO x: Notch joined the game"),
            None
        );
    }

    /// A bracketed run is only a prefix if it announces a level or a time.
    ///
    /// Without that test, consuming brackets to reach Pumpkin's name would
    /// read a chat author as a prefix, and this line — which any player can
    /// type — would be a join.
    #[test]
    fn a_bracketed_chat_author_is_not_a_log_prefix() {
        assert_eq!(joined("[INFO] [Griefer] Notch joined the game"), None);
        assert_eq!(left("[INFO] [Griefer] Notch left the game"), None);
        assert_eq!(joined("[Griefer] Notch joined the game"), None);
    }

    /// Ignoring colour codes must not become a way to erase the chat author.
    #[test]
    fn a_colour_code_in_chat_cannot_erase_the_author() {
        assert_eq!(
            joined("[20:37:28] [Server thread/INFO]: <Griefer>[0m Notch joined the game"),
            None
        );
    }

    /// A log line is mostly brackets and digits, and none of it may be
    /// mistaken for a colour code — a timestamp that got eaten would take the
    /// rest of the line with it.
    #[test]
    fn a_line_without_colour_is_returned_untouched() {
        let line = "[20:37:28] [Server thread/INFO]: Preparing spawn area: 0% [1/9] {a;b}";
        assert!(
            matches!(strip_ansi(line), Cow::Borrowed(same) if same == line),
            "a clean line was copied: {:?}",
            strip_ansi(line)
        );
    }

    #[test]
    fn colour_survives_nothing_but_takes_nothing_with_it() {
        assert_eq!(
            strip_ansi("\u{1b}[0;39;22mDone (3.2s)!\u{1b}[m"),
            "Done (3.2s)!"
        );
        // A half-written sequence is text, not a licence to eat the rest.
        assert_eq!(strip_ansi("[0;39 red"), "[0;39 red");
        assert_eq!(strip_ansi("[20:37:28] hi"), "[20:37:28] hi");
    }

    #[test]
    fn a_coloured_ready_line_and_ceiling_are_still_read() {
        assert!(is_ready(
            "\u{1b}[m\u{1b}[36;22m[20:37:28 INFO]: \u{1b}[mDone (3.244s)\u{1b}[m! For help, type \"help\""
        ));
        assert_eq!(
            max_players("\u{1b}[36;22m[12:00:00] [main/INFO]: maxPlayers: \u{1b}[0;39;22m8"),
            Some(8)
        );
    }

    #[test]
    fn a_bounded_console_drops_the_oldest() {
        let mut console = Console::new(3);
        for i in 1..=5 {
            console.record(format!("line {i}"));
        }
        assert_eq!(console.len(), 3);
        let slice = console.since(0);
        assert_eq!(slice.lines, vec!["line 3", "line 4", "line 5"]);
        assert_eq!(slice.cursor, 5);
    }

    #[test]
    fn a_cursor_returns_only_what_is_new() {
        let mut console = Console::new(100);
        console.record("a");
        console.record("b");
        let first = console.since(0);
        assert_eq!(first.lines, vec!["a", "b"]);

        console.record("c");
        let next = console.since(first.cursor);
        assert_eq!(next.lines, vec!["c"]);
        assert_eq!(next.cursor, 3);
    }

    /// A console that was away longer than the buffer is deep must get what
    /// survives, not nothing and not everything again.
    #[test]
    fn a_cursor_older_than_the_buffer_gets_what_remains() {
        let mut console = Console::new(3);
        for i in 0..10 {
            console.record(format!("{i}"));
        }
        let slice = console.since(0);
        assert_eq!(slice.lines, vec!["7", "8", "9"]);
        assert_eq!(slice.cursor, 10);
    }

    #[test]
    fn a_cursor_past_the_end_returns_nothing() {
        let mut console = Console::new(10);
        console.record("only");
        let slice = console.since(99);
        assert!(slice.lines.is_empty());
        assert_eq!(slice.cursor, 1);
    }

    #[test]
    fn clearing_resets_the_cursor_for_a_new_run() {
        let mut console = Console::new(10);
        console.record("old run");
        console.clear();
        console.record("new run");
        let slice = console.since(0);
        assert_eq!(slice.lines, vec!["new run"]);
        assert_eq!(slice.cursor, 1);
    }

    #[test]
    fn the_player_ceiling_is_read_from_either_spelling() {
        assert_eq!(max_players("max-players=20"), Some(20));
        assert_eq!(
            max_players("[12:00:00] [main/INFO]: maxPlayers: 8"),
            Some(8)
        );
        // The properties dump prints it among everything else.
        assert_eq!(
            max_players("[Server thread/INFO]: Loaded max-players = 100 from file"),
            Some(100)
        );
    }

    #[test]
    fn a_line_with_no_ceiling_in_it_says_so() {
        // Not a number, not a ceiling, and not this parser's business.
        assert_eq!(max_players("max-players="), None);
        assert_eq!(max_players("max-players=lots"), None);
        assert_eq!(max_players("Notch joined the game"), None);
        assert_eq!(max_players(""), None);
    }
}

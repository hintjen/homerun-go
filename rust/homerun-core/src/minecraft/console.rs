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

/// `§` in UTF-8. Minecraft's formatting prefix, which is not ANSI and which no
/// `--disable-ansi` flag suppresses.
const SECTION: [u8; 2] = [0xc2, 0xa7];

/// A line with its colour codes taken out, borrowed when there were none.
///
/// Paper colours its console — including the join and leave messages — so on
/// most real servers every line below arrives wrapped in `ESC [ 0;39;22 m`.
/// The escape byte is optional here on purpose: some log paths (a pty stripped
/// of control characters, a Java `PrintStream` that dropped it) deliver the
/// remnant `[0m` as plain text, and the desktop learned the same thing.
///
/// **Two vocabularies, not one.** ANSI is what a terminal understands;
/// `§`-prefixed codes are Minecraft's own, and a server writes them straight
/// into its output. PowerNukkitX writes them on every join, leave and level
/// message — `§belPTFO§f[/127.0.0.1:56926] logged in …` — and its
/// `--disable-ansi` flag does nothing about them, because they are not ANSI.
///
/// That was found on a phone, not in a test: the first real PowerNukkitX run
/// parsed its ready line correctly and then recognised **no** join and **no**
/// leave, in either of the two forms that engine emits, because every name
/// arrived with `§b` welded to the front of it. Presence reporting was silently
/// dead for the whole session while the server ran perfectly.
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
        // `§` is U+00A7: two bytes, and the code that follows it is one
        // character which may itself be multi-byte if a server emits nonsense.
        // Measured in bytes so the index never lands mid-character.
        if bytes[at] == SECTION[0] && bytes.get(at + 1) == Some(&SECTION[1]) {
            let after = at + SECTION.len();
            let code = line[after..].chars().next().map(char::len_utf8).unwrap_or(0);
            if code > 0 {
                let out = out.get_or_insert_with(|| String::with_capacity(line.len()));
                out.push_str(&line[copied..at]);
                at = after + code;
                copied = at;
                continue;
            }
        }
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
///
/// # `[m` versus `[main]`
///
/// `ESC [ m` is a legal reset with no parameters, and the escape byte is
/// optional here (see [`strip_ansi`]) — so a bare `[m` matches, and **`[main]`
/// was silently cut to `ain]`**. Nothing hit it while every engine wrote
/// `[Server thread/INFO]` or `[INFO]`; PowerNukkitX writes `[main]`, and its
/// joins and leaves went unrecognised because the log prefix no longer looked
/// like one.
///
/// Requiring a parameter would fix it and is wrong: a captured line in these
/// tests is `[m[36;22m[20:37:28 INFO]: …`, so parameterless remnants are real.
///
/// What separates them is the character *after* the `m`. A reset is followed by
/// another sequence or by the end of the line; a thread name is followed by the
/// rest of a word. So an escape-less, parameterless `[m` is a colour code only
/// when what follows it could not be one — which is the whole of the rule.
fn ansi_len(bytes: &[u8], at: usize) -> Option<usize> {
    let mut end = at;
    let escaped = bytes.get(end) == Some(&ESC);
    if escaped {
        end += 1;
    }
    if bytes.get(end) != Some(&b'[') {
        return None;
    }
    end += 1;
    let params = end;
    while matches!(bytes.get(end), Some(b'0'..=b'9' | b';')) {
        end += 1;
    }
    if bytes.get(end) != Some(&b'm') {
        return None;
    }
    if !escaped && end == params && bytes.get(end + 1).is_some_and(u8::is_ascii_alphanumeric) {
        return None;
    }
    Some(end + 1 - at)
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
///
/// Two vocabularies, because the servers word it differently:
///
/// ```text
/// Notch joined the game                                  // vanilla, Paper, Pumpkin
/// Notch[/127.0.0.1:52134] logged in with entity id 12 ... // PowerNukkitX
/// ```
///
/// The second is Nukkit's `nukkit.player.logIn`, and it is why presence works
/// on a Bedrock server at all — nothing else on that engine announces a join.
pub fn joined(line: &str) -> Option<&str> {
    player_before(line, " joined the game")
        .or_else(|| player_before_address(line, "logged in with entity id"))
}

/// The player named in a leave line, if this is one.
pub fn left(line: &str) -> Option<&str> {
    player_before(line, " left the game")
        .or_else(|| player_before_address(line, "logged out due to"))
}

/// The Bedrock version a server announced at boot.
///
/// `nukkit.server.start` — `Starting Minecraft: BE server version v1.21.100`.
/// This is the only honest source for it: a PowerNukkitX *release* number is
/// not a Minecraft version, and the release metadata does not carry one, so
/// what the server says about itself is what the player should be shown.
pub fn bedrock_version(line: &str) -> Option<&str> {
    const MARKER: &str = "Starting Minecraft: BE server version ";
    let clean = strip_ansi(line);
    // Position in the stripped copy, then the same slice of the caller's line —
    // a version never contains a colour code, so the two agree.
    let at = clean.find(MARKER)? + MARKER.len();
    let version = clean[at..].trim();
    if version.is_empty() {
        return None;
    }
    let start = line.rfind(version)?;
    Some(&line[start..start + version.len()])
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
/// ```text
/// 19:31:17 [Netty Server IO #1] [INFO] Ada[/10.0.0.4:1] logged in …  // PowerNukkitX
/// ```
///
/// A prefix is a run of bracketed tags that **look like log tags** — a
/// timestamp, or something naming a level — each followed by a space or by
/// `: `. Consuming exactly those is what makes the formats one case.
///
/// PowerNukkitX adds the third line above and it breaks two assumptions at
/// once: its log4j console pattern is `%d{HH:mm:ss} [%t] [%level] %msg`, so
/// the timestamp is **bare**, and the tag after it is a **thread name** that
/// resembles nothing. Without both fixes below the whole prefix stayed in the
/// name, every join and leave on that engine went unrecognised, and presence
/// reporting was silently dead while the server ran perfectly.
///
/// # The forgery guard, kept
///
/// The tag test is the security property and it survives: consumption always
/// stops at the **last recognised** tag in the run, never past it. An
/// unrecognised tag is skipped only when a real one follows it, which is what
/// lets a thread name through while `[Griefer] Notch joined the game` — with
/// no real tag anywhere — still consumes nothing. A trailing forgery after a
/// genuine prefix (`[19:00:00] [INFO]: [Griefer] Notch joined the game`) stays
/// in the name too, because the last real tag was `[INFO]`.
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

    // A bare leading timestamp: `19:31:17`, or a full `2026-08-21 19:31:17`.
    // Digits and separators only, and it has to be followed by whitespace —
    // which is what keeps it from eating the start of a message that merely
    // begins with a number.
    //
    // Safe against the forgery this function exists to prevent for the same
    // reason the bracket test is: chat arrives wrapped (`<Ada> …`), so a
    // player typing a timestamp cannot reach the name test below.
    fn stamp(text: &str) -> Option<&str> {
        let token = text.split_whitespace().next()?;
        let digits = token.chars().filter(|c| c.is_ascii_digit()).count();
        if digits >= 4
            && token
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ':' | '-' | '.'))
        {
            return Some(text[token.len()..].trim_start());
        }
        None
    }
    // Twice: a date and a time are two tokens.
    for _ in 0..2 {
        match stamp(rest) {
            Some(after) => rest = after,
            None => break,
        }
    }

    // Walk the run of bracketed tags, remembering where the last *recognised*
    // one ended. That offset is what gets consumed; anything past it is the
    // message, tag-shaped or not.
    let mut scan = rest;
    while let Some(open) = scan.strip_prefix('[') {
        let Some(close) = open.find(']') else { break };
        let tag = &open[..close];
        let tail = &open[close + 1..];
        // `]: ` on vanilla, `] ` on Pumpkin and PowerNukkitX. A tag butted
        // straight against text is not a prefix — it is text that happens to
        // start with one.
        let tail = tail.strip_prefix(':').unwrap_or(tail);
        let Some(after) = tail.strip_prefix(' ') else {
            break;
        };
        scan = after.trim_start();
        if is_log_tag(tag) {
            rest = scan;
        }
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
/// The name in front of a `name[/ip:port] did something` line.
///
/// PowerNukkitX's shape, and it needs its own reader: the name is not adjacent
/// to the marker, an address sits between them.
///
/// The forgery guard from [`player_before`] applies here too and is the reason
/// the marker is checked *after* the address rather than anywhere in the line.
/// Chat is `<Notch> hello`, so a griefer typing
/// `Griefer[/1.2.3.4:1] logged in with entity id 5` produces a line whose text
/// before `[/` is `<Notch> Griefer` — which fails the name test below on the
/// angle brackets, exactly as `[Griefer] Notch joined the game` fails the log
/// tag test.
///
/// Names may contain spaces here and cannot on a Java server: a Bedrock
/// identity is an Xbox gamertag, and legacy gamertags have them.
fn player_before_address<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let (head, tail) = line.split_once("[/")?;
    let after_address = tail.split_once("] ")?.1;
    if !strip_ansi(after_address).trim_start().starts_with(marker) {
        return None;
    }
    let clean = strip_ansi(head);
    let name = after_log_prefix(&clean).trim();
    // The slice handed back is of `head`, so a name that only became valid
    // once its colour codes were removed has to be found there by content.
    // `rfind` below does that; this is the guard that it can.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
    {
        return None;
    }
    let at = head.rfind(name)?;
    Some(&head[at..at + name.len()])
}

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

    // ─── PowerNukkitX ───────────────────────────────────────────────────────

    const PNX_JOIN: &str = "[09:15:02] [Server thread/INFO]: Ada[/10.0.0.4:52134] logged in with entity id 12 at (world, 0.5, 64.0, 0.5)";
    const PNX_LEFT: &str =
        "[09:20:11] [Server thread/INFO]: Ada[/10.0.0.4:52134] logged out due to Disconnected";

    #[test]
    fn a_bedrock_join_names_the_player() {
        assert_eq!(joined(PNX_JOIN), Some("Ada"));
        assert_eq!(left(PNX_JOIN), None);
    }

    #[test]
    fn a_bedrock_leave_names_the_player() {
        assert_eq!(left(PNX_LEFT), Some("Ada"));
        assert_eq!(joined(PNX_LEFT), None);
    }

    /// Xbox gamertags have them; Java names cannot.
    #[test]
    fn a_gamertag_with_a_space_survives() {
        let line = "[09:15:02] [Server thread/INFO]: Ada Lovelace[/10.0.0.4:1] logged in with entity id 12 at (world, 0, 64, 0)";
        assert_eq!(joined(line), Some("Ada Lovelace"));
    }

    /// The same forgery the vanilla parser is guarded against, in the shape
    /// this engine writes. Chat is `<Ada> ...`, so the angle brackets are what
    /// give it away.
    #[test]
    fn a_join_typed_into_chat_is_not_a_join() {
        let line = "[09:15:02] [Server thread/INFO]: <Ada> Griefer[/1.2.3.4:1] logged in with entity id 5 at (world, 0, 0, 0)";
        assert_eq!(joined(line), None);
    }

    /// An address in a line that is not about a player joining.
    #[test]
    fn an_unrelated_bracketed_address_is_not_a_join() {
        assert_eq!(
            joined("[09:15:02] [Server thread/INFO]: Query[/0.0.0.0:19132] is running"),
            None
        );
    }

    /// Verified against `nukkit.server.startFinished` in `language/eng`.
    #[test]
    fn powernukkitx_announces_ready_in_words_this_already_knew() {
        assert!(is_ready(
            "[09:15:02] [Server thread/INFO]: Done (12.345s)! For help, type \"help\" or \"?\""
        ));
    }

    /// `nukkit.server.start`. The only place the Bedrock version appears — a
    /// PowerNukkitX release number is not a Minecraft version.
    #[test]
    fn the_boot_banner_carries_the_bedrock_version() {
        assert_eq!(
            bedrock_version(
                "[09:15:00] [Server thread/INFO]: Starting Minecraft: BE server version v1.21.100"
            ),
            Some("v1.21.100")
        );
        assert_eq!(bedrock_version("[09:15:00] [INFO]: Loading pnx.yml..."), None);
    }



    // ─── the section sign, from a real device ───────────────────────────────

    /// Captured from the first PowerNukkitX run on a phone, 2026-08-21,
    /// rendered in the layout the host actually reads.
    ///
    /// `log4j2.xml` gives the `<TerminalConsole>` appender
    /// `%d{HH:mm:ss} [%t] [%level] %msg` — a **bare** timestamp, then a thread
    /// name, then the level. The `logs/server.log` appender uses a different
    /// pattern entirely, and it is tempting to test against that file because
    /// it is the one you can read off the device; nothing consumes it.
    ///
    /// Colour codes are covered both ways: whether the appender strips them
    /// depends on stdout being a terminal, and a pipe is not one.
    ///
    /// Every assertion here returned `None` before this change.
    #[test]
    fn the_console_format_the_host_reads_is_recognised() {
        let plain = "19:31:17 [Netty Server IO #1] [INFO] elPTFO[/127.0.0.1:56926] logged in with entity id 1 at (world, 0.6852, 68.0, 0.8249)";
        assert_eq!(joined(plain), Some("elPTFO"));

        let coloured = "19:31:17 [Netty Server IO #1] [INFO] \u{a7}belPTFO\u{a7}f[/127.0.0.1:56926] logged in with entity id 1 at (world, 0, 64, 0)";
        assert_eq!(joined(coloured), Some("elPTFO"));

        let out = "19:31:50 [main] [INFO] elPTFO[/127.0.0.1:56926] logged out due to Server closed";
        assert_eq!(left(out), Some("elPTFO"));

        let vanilla = "19:31:21 [main] [INFO] elPTFO joined the game";
        assert_eq!(joined(vanilla), Some("elPTFO"));

        assert!(is_ready(
            "19:30:10 [main] [INFO] Done (4.213s)! For help, type \"help\" or \"?\""
        ));
    }

    /// A bare number at the start of a message is not a timestamp.
    #[test]
    fn a_leading_number_is_not_mistaken_for_a_prefix() {
        assert_eq!(joined("12345 joined the game"), None);
        assert_eq!(joined("[19:31:17] [Server thread/INFO]: Ada joined the game"), Some("Ada"));
    }

    /// This one always worked, and is pinned so it keeps working.
    #[test]
    fn a_real_powernukkitx_ready_line_is_recognised() {
        assert!(is_ready(
            "2026-08-21 19:30:10 [main] INFO - Done (4.213s)! For help, type \"help\" or \"?\""
        ));
    }

    #[test]
    fn section_codes_are_stripped_like_ansi_is() {
        assert_eq!(strip_ansi("\u{a7}aworld\u{a7}r"), "world");
        assert_eq!(strip_ansi("\u{a7}e100\u{a7}f%"), "100%");
        // A trailing `§` with nothing after it is left alone rather than
        // eating past the end of the line.
        assert_eq!(strip_ansi("done \u{a7}"), "done \u{a7}");
        // Untouched when there is nothing to strip, and still borrowed.
        assert!(matches!(strip_ansi("plain"), std::borrow::Cow::Borrowed(_)));
    }

    /// The colour codes are a griefer's tool too: they must not let a chat
    /// line masquerade as a join once they are gone.
    #[test]
    fn stripping_colour_does_not_open_the_forgery_hole() {
        assert_eq!(
            joined("[19:31:17] [INFO]: \u{a7}b<Ada>\u{a7}f Griefer[/1.2.3.4:1] logged in with entity id 5 at (world, 0, 0, 0)"),
            None
        );
    }



    /// `[main]` is a thread name, not a reset sequence. Cutting it to `ain]`
    /// left the log prefix unrecognisable and killed presence reporting on
    /// PowerNukkitX, which is the only engine here that writes it.
    #[test]
    fn a_bare_bracket_m_is_not_a_colour_code() {
        assert_eq!(strip_ansi("19:31:21 [main] [INFO] Ada joined the game"),
                   "19:31:21 [main] [INFO] Ada joined the game");
        assert_eq!(strip_ansi("[monster] spawned"), "[monster] spawned");
    }

    /// The leniency that motivated the escape-less path still works, both with
    /// parameters and without: a parameterless reset is real, it is just never
    /// followed by the rest of a word.
    #[test]
    fn an_escapeless_remnant_is_still_stripped() {
        assert_eq!(strip_ansi("[0mAda joined the game"), "Ada joined the game");
        assert_eq!(strip_ansi("[0;39;22mAda"), "Ada");
        assert_eq!(strip_ansi("[m[0;39;22mAda"), "Ada");
        assert_eq!(strip_ansi("Ada left the game[m"), "Ada left the game");
    }

    /// With the escape byte present, a parameterless reset is a real sequence.
    #[test]
    fn a_real_escaped_reset_is_still_stripped() {
        assert_eq!(strip_ansi("\u{1b}[mAda"), "Ada");
    }

}

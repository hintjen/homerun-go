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

use std::collections::VecDeque;

/// `Done (12.345s)! For help, type "help"` — the server is accepting
/// connections. Matched loosely enough to survive the timing text changing.
pub fn is_ready(line: &str) -> bool {
    let Some(rest) = line.split_once("Done (").map(|(_, r)| r) else {
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

/// Pull the name immediately before a marker, after the log prefix.
///
/// Vanilla prints `[HH:MM:SS] [Server thread/INFO]: Name joined the game`.
/// Taking the last whitespace-separated token before the marker keeps this
/// working when a loader adds its own prefix.
fn player_before<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let head = line.split_once(marker)?.0;
    // Everything the server itself prefixes ends with "]: ", and a chat
    // message would put the name in brackets — refusing anything with a
    // bracket after the prefix keeps `<Name> joined the game` in chat from
    // being read as a real join.
    let head = head.rsplit("]: ").next()?;
    let name = head.split_whitespace().next_back()?;
    if name.is_empty() || name.len() != head.trim().len() {
        return None;
    }
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(name)
    } else {
        None
    }
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
}

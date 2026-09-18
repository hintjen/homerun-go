//! Bounded console buffer with monotonic cursors.
//!
//! Hosts poll for new lines rather than being pushed every line, so the
//! buffer has to answer "what happened since I last asked" while never
//! growing without bound — a server can emit megabytes of chat and a
//! backgrounded phone may not poll for minutes.
//!
//! Sequence numbers are monotonic and never reused, so a stale cursor is
//! detectable rather than silently returning the wrong lines.

use std::collections::VecDeque;

/// Roughly a few hundred KB of console at typical line lengths.
pub const DEFAULT_CAPACITY: usize = 2000;

pub struct LogBuffer {
    lines: VecDeque<(u64, String)>,
    capacity: usize,
    /// Sequence the next pushed line will get.
    next_seq: u64,
}

pub struct LogSlice {
    pub lines: Vec<String>,
    /// Pass as `since` next time.
    pub cursor: u64,
    /// True when the buffer discarded lines the caller had not read yet.
    pub dropped: bool,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity.min(DEFAULT_CAPACITY)),
            capacity: capacity.max(1),
            next_seq: 0,
        }
    }

    pub fn push(&mut self, line: impl Into<String>) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back((self.next_seq, line.into()));
        self.next_seq += 1;
    }

    /// Lines with sequence >= `since`, plus the cursor for the next call.
    ///
    /// A cursor older than everything retained reports `dropped`, so the host
    /// can tell the difference between "nothing new" and "you fell behind".
    pub fn since(&self, since: u64) -> LogSlice {
        let oldest = self
            .lines
            .front()
            .map(|(seq, _)| *seq)
            .unwrap_or(self.next_seq);
        LogSlice {
            lines: self
                .lines
                .iter()
                .filter(|(seq, _)| *seq >= since)
                .map(|(_, line)| line.clone())
                .collect(),
            cursor: self.next_seq,
            dropped: since < oldest,
        }
    }

    /// Drop everything but keep sequences advancing, so cursors held across
    /// a restart resolve to `dropped` instead of replaying a new run's lines.
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_from_a_cursor_and_advances_it() {
        let mut buf = LogBuffer::default();
        buf.push("one");
        buf.push("two");

        let first = buf.since(0);
        assert_eq!(first.lines, ["one", "two"]);
        assert_eq!(first.cursor, 2);
        assert!(!first.dropped);

        // Nothing new since the returned cursor.
        let second = buf.since(first.cursor);
        assert!(second.lines.is_empty());
        assert_eq!(second.cursor, 2);

        buf.push("three");
        assert_eq!(buf.since(second.cursor).lines, ["three"]);
    }

    #[test]
    fn evicts_oldest_and_reports_the_gap() {
        let mut buf = LogBuffer::new(3);
        for i in 0..5 {
            buf.push(format!("line{i}"));
        }
        assert_eq!(buf.len(), 3);

        // A caller still holding cursor 0 lost line0 and line1.
        let slice = buf.since(0);
        assert_eq!(slice.lines, ["line2", "line3", "line4"]);
        assert!(slice.dropped, "must admit lines were discarded");

        // A caller that kept up sees no gap.
        assert!(!buf.since(2).dropped);
    }

    #[test]
    fn sequences_keep_advancing_across_clear() {
        let mut buf = LogBuffer::default();
        buf.push("run one");
        let cursor = buf.since(0).cursor;

        buf.clear();
        buf.push("run two");

        // The stale cursor must not silently return the new run's line as if
        // it were the continuation of the old one.
        let slice = buf.since(cursor);
        assert_eq!(slice.lines, ["run two"]);
        assert_eq!(slice.cursor, 2);
    }

    #[test]
    fn empty_buffer_is_not_a_gap() {
        let buf = LogBuffer::default();
        let slice = buf.since(0);
        assert!(slice.lines.is_empty());
        assert!(!slice.dropped);
        assert_eq!(slice.cursor, 0);
    }
}

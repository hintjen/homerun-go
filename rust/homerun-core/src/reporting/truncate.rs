//! Cutting text to a byte budget without panicking.
//!
//! # Why this is a module rather than a line at each call site
//!
//! Slicing a `String` at an arbitrary byte offset is one of the few things in
//! safe Rust that panics outright, and every field this crate reports is UTF-8
//! that somebody else wrote: a Minecraft console carries `§` colour codes and
//! a MOTD, a JavaScript stack carries whatever the page put in an error
//! message, a file path carries whatever the player called their account. The
//! first version of this lived as one private helper in [`super::crash`]
//! because there was one caller. App error reporting adds five more, and five
//! hand-rolled boundary scans is five chances to write `&text[..max]` and ship
//! a panic that only fires for people whose names are not ASCII.
//!
//! # Head and tail are not the same decision
//!
//! [`head`] keeps the beginning, [`tail_lines`] keeps the end, and which one a
//! field wants is a property of the field rather than a preference:
//!
//! - A **stack trace** is most useful at the top. The frame that threw is the
//!   first line; the last forty are the framework that called it.
//! - A **console log** is most useful at the bottom. The crash is the last
//!   thing that happened; the first forty thousand lines are world generation.
//!
//! Getting this backwards does not fail loudly — it produces a report that
//! looks complete and contains none of the answer — so the two live next to
//! each other here, named for what they keep.

/// What replaces the part that did not fit.
///
/// Visible on purpose. A truncated field that says nothing about it reads as a
/// short field, and "the stack was three frames long" is a very different
/// diagnosis from "the stack was cut at 8 KiB".
const TRUNCATED: &str = "[truncated]";

/// The dropped-prefix marker for [`tail_lines`].
const EARLIER_DROPPED: &str = "[earlier lines dropped]";

/// The first `max` bytes, cut on a char boundary, with [`TRUNCATED`] appended.
///
/// The marker is counted *inside* the budget, so the result is never longer
/// than `max`. That matters because the callers of this are themselves inside
/// a whole-body size assertion: a per-field cap that could overshoot by a
/// marker each time turns into an overshoot of six markers at the top.
///
/// The one exception is a `max` smaller than the marker itself, where the
/// marker is returned whole. No caller does that — the smallest real budget is
/// 256 bytes — and returning a silently empty string would be worse.
pub(crate) fn head(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }

    let room = max.saturating_sub(TRUNCATED.len());
    // Walk *down* to a boundary. Walking up could exceed `room` and, at the
    // very end of the string, exceed `max`.
    let mut end = room.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }

    let mut out = String::with_capacity(end + TRUNCATED.len());
    out.push_str(&text[..end]);
    out.push_str(TRUNCATED);
    out
}

/// The last `max` bytes, cut forward to the next line boundary.
///
/// The scan for the boundary starts from a **char** boundary, not from the
/// byte offset itself, for the reason in the module header.
///
/// Unlike [`head`], the marker is added *outside* the budget: this one's
/// caller is [`super::crash`], whose cap is 128 KiB against a body with no
/// other large field, and whose tests pin the existing behaviour. Keeping it
/// exact was worth more than making the two symmetrical.
pub(crate) fn tail_lines(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }

    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }

    // Prefer starting at a whole line. Falling back to `start` covers the case
    // of a single enormous line with no newline in the window at all.
    let cut = text[start..]
        .find('\n')
        .map(|offset| start + offset + 1)
        .unwrap_or(start);

    format!("{EARLIER_DROPPED}\n{}", &text[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_leaves_short_text_alone() {
        assert_eq!(head("already small", 64), "already small");
        // Exactly at the budget is not truncation.
        assert_eq!(head("abcd", 4), "abcd");
    }

    #[test]
    fn head_stays_within_the_budget() {
        let out = head(&"x".repeat(1000), 64);
        assert!(out.len() <= 64, "{} bytes", out.len());
        assert!(out.ends_with(TRUNCATED));
        assert!(out.starts_with("xxxx"));
    }

    #[test]
    fn head_never_splits_a_char() {
        // Every char is 4 bytes, so almost every candidate cut is mid-char.
        // Whatever budget we ask for, the result must still be valid UTF-8 —
        // which it is by construction, because it is a `String`; the assertion
        // that matters is that this does not panic.
        let text = "🙂".repeat(100);
        for max in 12..80 {
            let out = head(&text, max);
            assert!(out.len() <= max, "max {max} gave {} bytes", out.len());
        }
    }

    #[test]
    fn head_returns_the_marker_when_the_budget_is_absurd() {
        assert_eq!(head("some long text here", 3), TRUNCATED);
    }

    #[test]
    fn tail_keeps_the_end_and_cuts_on_a_line() {
        let text = (0..500)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_lines(text, 100);

        assert!(out.starts_with(EARLIER_DROPPED));
        assert!(out.ends_with("line 499"));
        // No partial line survived the cut.
        assert!(!out.contains("line 4\nline"), "cut mid-line");
    }

    #[test]
    fn tail_survives_one_enormous_line() {
        // No newline anywhere in the window: the fallback path.
        let out = tail_lines("🙂".repeat(1000), 64);
        assert!(out.starts_with(EARLIER_DROPPED));
    }

    #[test]
    fn tail_leaves_short_text_alone() {
        assert_eq!(tail_lines("small".to_string(), 64), "small");
    }
}

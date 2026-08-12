//! This device's own logs, for remote support.
//!
//! The desktop answers `get-app-logs` with two things: the tail of its
//! main-process log file, and a buffer of renderer console output. A phone has
//! neither — but it has logcat, which already holds both, because every host
//! tag goes there and `MainActivity`'s `WebChromeClient` forwards the WebView's
//! console to it under `HomerunWeb`.
//!
//! Reference: `AppLogsProvider` in `deviceWebsocket/handlers.ts`, and the API's
//! `fetch_client_logs` task that calls it.
//!
//! # No permission is needed, and that is not an accident
//!
//! `READ_LOGS` is a signature permission and we do not hold it. What we do is
//! read **our own** entries: logd filters by the caller's UID, so
//! `logcat --pid=<us>` returns this app's lines and nothing else. That is the
//! whole reason this is safe to expose to a support flow — there is no way to
//! widen it into someone else's device.

/// The most of each log to send.
///
/// Support wants the recent past, not the session. The frame goes down a socket
/// with a 4 MB queue cap behind it, and two logs that between them could fill
/// it would drop the peer they were meant to help.
const MAX_BYTES: usize = 128 * 1024;

/// The tag `MainActivity` forwards the WebView console under. Splitting on it
/// is what turns one logcat read into the two fields the dashboard expects.
const RENDERER_TAG: &str = "HomerunWeb";

/// `(main, renderer)` — the same pair the desktop returns.
pub fn collect() -> (String, String) {
    read()
        .map(split)
        .unwrap_or_else(|reason| (reason, String::new()))
}

/// Read this process's own logcat.
#[cfg(target_os = "android")]
fn read() -> Result<String, String> {
    use std::process::Command;

    // One read, split afterwards. Two `logcat` invocations would be two spawns
    // and two windows onto a buffer that is still being written to, so the
    // renderer lines could not be lined up against the host's.
    let output = Command::new("logcat")
        .args(["-d", "--pid", &std::process::id().to_string()])
        .output()
        .map_err(|e| format!("logcat could not be read: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "logcat exited {}",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "on a signal".to_string())
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every other target says so plainly.
///
/// The `device-ws` feature is Android-only today, but the crate builds and
/// tests on the host, and a stub that returned an empty string would read as
/// "no problems here" rather than "not implemented".
#[cfg(not(target_os = "android"))]
fn read() -> Result<String, String> {
    Err("Logs are only available on Android.".to_string())
}

/// Divide logcat into the desktop's two fields.
fn split(raw: String) -> (String, String) {
    let mut main = String::new();
    let mut renderer = String::new();
    // The threadtime format is `<date> <time> <pid> <tid> <level> <tag>: msg`,
    // so a tag match is anchored on the `<level> <tag>: ` that precedes it —
    // looking for the bare tag anywhere would move any line that merely
    // mentioned it.
    let needle = format!(" {RENDERER_TAG}: ");
    for line in raw.lines() {
        if line.contains(&needle) {
            renderer.push_str(line);
            renderer.push('\n');
        } else {
            main.push_str(line);
            main.push('\n');
        }
    }
    (tail(main), tail(renderer))
}

/// The last [`MAX_BYTES`], cut at a line boundary.
///
/// The *end* of a log is the part that explains a problem. Cutting mid-line
/// would leave a fragment that reads like a different message.
fn tail(text: String) -> String {
    if text.len() <= MAX_BYTES {
        return text;
    }
    let start = text.len() - MAX_BYTES;
    let cut = text[start..]
        .find('\n')
        .map(|offset| start + offset + 1)
        .unwrap_or(start);
    format!("[earlier lines dropped]\n{}", &text[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_renderer_is_separated_from_the_host() {
        let raw = "08-12 14:13:37.661 1 2 I HomerunJava: server exited\n\
                   08-12 14:13:37.662 1 2 E HomerunWeb: TypeError: x is not a function\n\
                   08-12 14:13:37.663 1 2 I HomerunHost: state -> stopped\n"
            .to_string();
        let (main, renderer) = split(raw);
        assert!(main.contains("server exited"));
        assert!(main.contains("state -> stopped"));
        assert!(
            !main.contains("TypeError"),
            "the renderer line moved out of main"
        );
        assert!(renderer.contains("TypeError"));
        assert_eq!(renderer.lines().count(), 1);
    }

    /// A host line that merely mentions the tag is not a renderer line. The
    /// match is anchored on the `<tag>: ` logcat writes, not on the word.
    #[test]
    fn mentioning_the_tag_does_not_move_a_line() {
        let raw =
            "08-12 14:13:37.661 1 2 I HomerunHost: forwarding to HomerunWeb now\n".to_string();
        let (main, renderer) = split(raw);
        assert!(main.contains("forwarding"));
        assert!(renderer.is_empty());
    }

    #[test]
    fn the_tail_is_kept_and_the_cut_is_on_a_line_boundary() {
        let line = "08-12 14:13:37.661 1 2 I HomerunJava: a line of output\n";
        let raw = line.repeat(MAX_BYTES / line.len() + 200);
        let (main, _) = split(raw);

        assert!(main.len() <= MAX_BYTES + 64, "capped: {}", main.len());
        assert!(main.starts_with("[earlier lines dropped]\n"));
        // Every surviving line is whole. A fragment reads like a different
        // message, which is worse than one fewer line.
        for entry in main.lines().skip(1) {
            assert!(entry.starts_with("08-12"), "cut mid-line: {entry:?}");
        }
    }

    #[test]
    fn a_short_log_is_returned_untouched() {
        let raw = "08-12 14:13:37.661 1 2 I HomerunJava: short\n".to_string();
        let (main, _) = split(raw.clone());
        assert_eq!(main, raw);
    }

    /// The host build has no logcat, and says so rather than answering with an
    /// empty string that would read as "nothing went wrong".
    #[cfg(not(target_os = "android"))]
    #[test]
    fn a_platform_without_logcat_explains_itself() {
        let (main, renderer) = collect();
        assert!(main.contains("only available on Android"));
        assert!(renderer.is_empty());
    }
}

//! This device's own logs, for remote support.
//!
//! The desktop answers `get-app-logs` with two things: the tail of its
//! main-process log file, and a buffer of renderer console output. A phone has
//! neither, so each platform names its own source and everything after that —
//! the split, the cap, the line-boundary cut — is shared.
//!
//! **Android reads logcat**, which already holds both halves, because every
//! host tag goes there and `MainActivity`'s `WebChromeClient` forwards the
//! WebView's console to it under `HomerunWeb`.
//!
//! **Everywhere else the host supplies them**, through a function pointer it
//! registers with [`set_provider`] at startup. iOS is why: its logs live in the
//! unified logging system, which only `OSLogStore` can read and only Swift can
//! call. The alternative was a second copy of the log, written to a file for
//! this module to read — a log that exists to be read once, kept in duplicate
//! for the life of the app.
//!
//! This module is **not** behind `device-ws`, though `get-app-logs` is the only
//! thing that asks for it today. A host registers its provider at launch, long
//! before it knows whether a socket will ever come up, and an export that
//! exists in one build and not another is the FFI mismatch the ABI version is
//! there to catch.
//!
//! Reference: `AppLogsProvider` in `deviceWebsocket/handlers.ts`, and the API's
//! `fetch_client_logs` task that calls it.
//!
//! # No permission is needed on either platform, and that is not an accident
//!
//! `READ_LOGS` is an Android signature permission and we do not hold it. What
//! we do is read **our own** entries: logd filters by the caller's UID, so
//! `logcat --pid=<us>` returns this app's lines and nothing else. iOS draws the
//! same line in the API itself — `OSLogStore(scope: .currentProcessIdentifier)`
//! can see this process and no other. That is the whole reason this is safe to
//! expose to a support flow: on neither platform is there a way to widen it
//! into somebody else's device.

/// The most of each log to send.
///
/// Support wants the recent past, not the session. The frame goes down a socket
/// with a 4 MB queue cap behind it, and two logs that between them could fill
/// it would drop the peer they were meant to help.
const MAX_BYTES: usize = 128 * 1024;

/// The tag `MainActivity` forwards the WebView console under. Splitting on it
/// is what turns one logcat read into the two fields the dashboard expects.
///
/// Nothing on iOS writes it: WKWebView exposes no console callback the way
/// Android's `WebChromeClient` does, so the renderer half arrives empty there
/// rather than wrong. The split still runs — a host is free to adopt the tag if
/// it ever gains a way to capture that console.
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

/// Everywhere else, whatever the host registered.
///
/// A host that registers nothing says so plainly. An empty string would read as
/// "no problems here" rather than "nobody is answering", and the difference
/// matters most in exactly the situation this is used in — somebody looking at
/// a device they cannot hold.
#[cfg(not(target_os = "android"))]
fn read() -> Result<String, String> {
    provider::read()
}

/// The host's own logs, fetched on demand.
///
/// # Why a callback rather than a push
///
/// Support asks for the logs at an arbitrary moment and wants the *recent*
/// past. Anything pushed in advance is a snapshot of the wrong minute, and
/// pushing continuously would mean a second copy of every line crossing the
/// FFI for the one request in a thousand that reads it.
pub mod provider {
    use std::os::raw::c_char;
    use std::sync::{Mutex, OnceLock};

    /// Fill `buffer` with up to `capacity` bytes of UTF-8 and answer how many
    /// were written, or a negative number if the logs could not be read.
    ///
    /// The buffer belongs to this crate for the duration of the call and to
    /// nobody afterwards, which is what keeps a Swift allocation from having to
    /// be freed by Rust — the mistake this shape exists to make impossible.
    pub type Provider = unsafe extern "C" fn(buffer: *mut c_char, capacity: usize) -> isize;

    /// How much is asked for.
    ///
    /// Deliberately larger than [`super::MAX_BYTES`], because the split happens
    /// after the read: asking for exactly one field's worth would cap the two
    /// together at what one is allowed on its own.
    const CAPACITY: usize = 4 * super::MAX_BYTES;

    fn slot() -> &'static Mutex<Option<Provider>> {
        static SLOT: OnceLock<Mutex<Option<Provider>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    /// Register the host's provider, replacing any previous one.
    ///
    /// `None` unregisters, which is what a host passing a null pointer means.
    pub fn set(provider: Option<Provider>) {
        if let Ok(mut current) = slot().lock() {
            *current = provider;
        }
    }

    /// Ask the host, if there is one to ask.
    pub fn read() -> Result<String, String> {
        let provider = match slot().lock() {
            Ok(current) => *current,
            // A poisoned lock here means a panic while registering, which
            // cannot make the logs readable. Say so rather than retrying.
            Err(_) => return Err("The logs could not be read on this device.".to_string()),
        };
        let Some(provider) = provider else {
            return Err("This device has no logs to send.".to_string());
        };

        let mut buffer = vec![0u8; CAPACITY];
        // SAFETY: the pointer and length describe `buffer`, which outlives the
        // call, and the host contract is that it writes at most `capacity`
        // bytes and returns how many.
        let written = unsafe { provider(buffer.as_mut_ptr() as *mut c_char, CAPACITY) };
        if written < 0 {
            return Err("This device's logs could not be read.".to_string());
        }
        let written = (written as usize).min(CAPACITY);
        buffer.truncate(written);
        // Lossy on purpose. A host that fills the buffer to its last byte can
        // cut a character in half, and refusing the whole log over one broken
        // character would lose exactly the thing somebody is asking for.
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    }
}

/// Divide one log into the desktop's two fields.
fn split(raw: String) -> (String, String) {
    let mut main = String::new();
    let mut renderer = String::new();
    // logcat's threadtime format is
    // `<date> <time> <pid> <tid> <level> <tag>: msg`, so a tag match is
    // anchored on the ` <tag>: ` that precedes the message — looking for the
    // bare tag anywhere would move any line that merely mentioned it. A host
    // supplying its own log is expected to format it the same way, and iOS
    // does: `<time> <category>: <message>`.
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
///
/// The search for that boundary starts from a **char** boundary. logcat is
/// UTF-8 and carries whatever the server and the WebView printed — a player's
/// name, a `§` colour code, an emoji out of chat — and slicing a `String`
/// through the middle of a character panics. Here that panic would land in a
/// tokio task serving the device websocket, where nothing catches it.
fn tail(text: String) -> String {
    if text.len() <= MAX_BYTES {
        return text;
    }
    let mut start = text.len() - MAX_BYTES;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
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

    /// logcat is UTF-8, so the byte offset the cap computes lands inside a
    /// character for some log sizes — and slicing there panics, in a task
    /// nothing is catching.
    #[test]
    fn a_cut_landing_inside_a_character_does_not_panic() {
        for pad in 0..4 {
            let line = format!(
                "08-12 14:13:37.661 1 2 I HomerunJava: <Ünicode> {}\n",
                "é".repeat(64 + pad)
            );
            let raw = line.repeat(MAX_BYTES / line.len() + 40);
            let (main, _) = split(raw);
            assert!(main.len() <= MAX_BYTES + 64, "not capped: {}", main.len());
        }
    }

    #[test]
    fn a_short_log_is_returned_untouched() {
        let raw = "08-12 14:13:37.661 1 2 I HomerunJava: short\n".to_string();
        let (main, _) = split(raw.clone());
        assert_eq!(main, raw);
    }

    /// The provider is process-wide state, so the tests that touch it take a
    /// turn. Without this they race: one registers, another asserts there is
    /// nothing registered, and which passes depends on thread scheduling.
    #[cfg(not(target_os = "android"))]
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A host build with nothing registered says so rather than answering with
    /// an empty string that would read as "nothing went wrong".
    #[cfg(not(target_os = "android"))]
    #[test]
    fn a_platform_with_no_provider_explains_itself() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        provider::set(None);

        let (main, renderer) = collect();
        assert!(main.contains("no logs to send"), "unexpected: {main}");
        assert!(renderer.is_empty());
    }

    /// What iOS does: the host writes its own log text into the buffer, and it
    /// is split, capped and returned exactly as logcat's would be.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn a_registered_provider_is_read_split_and_capped() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());

        const LOG: &str = concat!(
            "10:11:12.001 host: state -> running\n",
            "10:11:12.002 HomerunWeb: TypeError: x is not a function\n",
        );

        unsafe extern "C" fn supply(buffer: *mut std::os::raw::c_char, capacity: usize) -> isize {
            let bytes = LOG.as_bytes();
            let len = bytes.len().min(capacity);
            // SAFETY: the caller guarantees `capacity` writable bytes, and
            // `len` is clamped to it. No inner `unsafe` block: this is already
            // an `unsafe fn`, and the crate's own `borrow` sets that style.
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, len);
            len as isize
        }

        provider::set(Some(supply));
        let (main, renderer) = collect();
        provider::set(None);

        assert!(main.contains("state -> running"));
        assert!(
            !main.contains("TypeError"),
            "the renderer line stayed in main"
        );
        assert!(renderer.contains("TypeError"));
    }

    /// A host that cannot read its own logs answers negatively, and that is
    /// reported as a reason rather than as an empty log.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn a_provider_that_fails_is_not_silence() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());

        unsafe extern "C" fn refuse(_buffer: *mut std::os::raw::c_char, _capacity: usize) -> isize {
            -1
        }

        provider::set(Some(refuse));
        let (main, renderer) = collect();
        provider::set(None);

        assert!(main.contains("could not be read"), "unexpected: {main}");
        assert!(renderer.is_empty());
    }
}

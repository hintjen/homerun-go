//! Where this crate's own diagnostics go on a platform that captures nothing.
//!
//! Everything here logs through the `log` facade — never `println!` or
//! `eprintln!`, for a reason both platforms share and neither forgives. Android
//! does not capture a process's stdout or stderr at all, so a line written
//! there is written to nothing. iOS captures them, but once a server starts the
//! supervisor has replaced fds 1 and 2 with the pipe feeding the
//! **player-visible** Minecraft console, so a line written there is worse than
//! lost: it is shown to a player as if the server had said it.
//!
//! Android wires the facade to logcat in [`crate::jni_bridge`] and needs
//! nothing from here. iOS has no equivalent — the unified logging system is
//! reached through `os_log`, whose entry points are C macros rather than
//! functions — so the host registers a sink and this module forwards to it.
//!
//! # Why the alternative is not "no logs"
//!
//! A device websocket fails in ways that are invisible from both ends. The
//! Android port lost a debugging round to exactly that: a certificate was
//! ordered, issued, stored, and then never served, because a panic inside a
//! tokio task was printed to a stderr the platform discards. The socket was up,
//! the dashboard saw a refusal, and nothing anywhere said why. A sink is what
//! makes the next one an hour instead of a day.

use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::{Mutex, OnceLock};

/// Receive one log line: a level, and a NUL-terminated UTF-8 message.
///
/// Levels are the `log` crate's, narrowed to what a host needs to choose a
/// severity with: 1 error, 2 warn, 3 info, 4 debug, 5 trace.
///
/// Called from whatever thread produced the line, including tokio workers. The
/// message pointer is valid for the duration of the call and not afterwards, so
/// a host that wants to keep it must copy it. It must not unwind.
pub type Sink = unsafe extern "C" fn(level: u8, message: *const c_char);

fn slot() -> &'static Mutex<Option<Sink>> {
    static SLOT: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Register the host's sink, replacing any previous one.
///
/// `None` unregisters, which leaves the facade installed and dropping lines —
/// deliberately. `log::set_logger` may be called once per process and cannot be
/// undone, so the installation is permanent and only the destination moves.
pub fn set(sink: Option<Sink>) {
    if let Ok(mut current) = slot().lock() {
        *current = sink;
    }

    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // A second logger is an error rather than a panic: Android installs
        // `android_logger` from `nativeInitLogging`, and a host that somehow
        // did both should keep the one that was already working.
        static FORWARDER: Forwarder = Forwarder;
        if log::set_logger(&FORWARDER).is_ok() {
            // Debug and trace are not forwarded. The console poll runs four
            // times a second per subscriber, and a log of that is a log nobody
            // reads twice.
            log::set_max_level(log::LevelFilter::Info);
        }
    });
}

struct Forwarder;

impl log::Log for Forwarder {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Copied out and the guard dropped before the call. Holding it across
        // a call into the host would deadlock this thread the moment a sink
        // logged anything itself, and the failure would look like a hang in
        // whatever produced the line.
        let sink = match slot().lock() {
            Ok(guard) => *guard,
            Err(_) => return,
        };
        let Some(sink) = sink else { return };

        // An interior NUL cannot cross into C, and a log line is never worth
        // failing over: strip and carry on rather than dropping the line.
        let message = record.args().to_string().replace('\0', "?");
        let Ok(message) = CString::new(message) else { return };

        // SAFETY: the pointer is valid until this call returns, which is the
        // contract on `Sink`, and the host promises not to unwind.
        unsafe { sink(level_of(record.level()), message.as_ptr()) };
    }

    fn flush(&self) {}
}

fn level_of(level: log::Level) -> u8 {
    match level {
        log::Level::Error => 1,
        log::Level::Warn => 2,
        log::Level::Info => 3,
        log::Level::Debug => 4,
        log::Level::Trace => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEEN: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn count(_level: u8, _message: *const c_char) {
        SEEN.fetch_add(1, Ordering::SeqCst);
    }

    /// Registering, logging and unregistering, in the order a host does them.
    ///
    /// One test rather than three: the facade is process-wide and installed
    /// once, so separate tests would race each other for the sink.
    #[test]
    fn a_registered_sink_receives_lines_and_a_cleared_one_stops() {
        set(Some(count));
        log::warn!("a device websocket line");
        let after_registering = SEEN.load(Ordering::SeqCst);
        assert!(after_registering > 0, "the sink saw nothing");

        set(None);
        log::warn!("another line, into nothing");
        assert_eq!(
            SEEN.load(Ordering::SeqCst),
            after_registering,
            "lines kept arriving after the sink was cleared"
        );
    }

    #[test]
    fn levels_are_the_facades_own_order() {
        assert_eq!(level_of(log::Level::Error), 1);
        assert_eq!(level_of(log::Level::Info), 3);
        assert!(level_of(log::Level::Error) < level_of(log::Level::Warn));
    }
}

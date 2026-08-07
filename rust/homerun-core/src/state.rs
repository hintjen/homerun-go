//! What state a server is in, and what the API should be told about it.
//!
//! Reference: `onServerFullyRunning` and the supervisor's exit handling in the
//! `homerun` repo.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed,
}

impl State {
    /// The three the bridge event contract carries.
    ///
    /// `starting` and `stopping` are ours alone — the UI infers those from the
    /// pending call, so emitting them would be inventing states the contract
    /// does not have.
    pub fn wire(self) -> Option<&'static str> {
        match self {
            State::Running => Some("running"),
            State::Stopped => Some("stopped"),
            State::Crashed => Some("crashed"),
            State::Starting | State::Stopping => None,
        }
    }

    /// What `POST /api/server/<id>/state/` should say, if anything.
    ///
    /// The API models only running and stopped; a crash is a stop it did not
    /// ask for.
    pub fn api_status(self) -> Option<&'static str> {
        match self {
            State::Running => Some("running"),
            State::Stopped | State::Crashed => Some("stopped"),
            State::Starting | State::Stopping => None,
        }
    }
}

/// How a launch ended, once the process is gone.
///
/// A stop the user asked for and a process that fell over look identical from
/// the exit code alone — a Minecraft server exits 0 on `stop` and often 0 on
/// its own after a fatal error too. What separates them is whether we asked.
pub fn exit_state(intentional: bool, exit_code: i32) -> State {
    if intentional || exit_code == 0 {
        State::Stopped
    } else {
        State::Crashed
    }
}

/// Why a tunnel failed, for `native-server-network-error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkFailure {
    /// Never came up: the gateway never provisioned, or the process would not
    /// spawn.
    Provisioning,
    /// Came up, then the gateway stopped answering — in practice its keys were
    /// regenerated and ours are permanently dead.
    Handshake,
}

impl NetworkFailure {
    pub fn wire(self) -> &'static str {
        match self {
            NetworkFailure::Provisioning => "provisioning",
            NetworkFailure::Handshake => "handshake",
        }
    }
}

/// wireproxy retries a failed handshake forever, so a dead credential set is
/// indistinguishable from a slow network until you count. Ten consecutive
/// failures at roughly five seconds apart is the desktop's threshold.
pub const HANDSHAKE_FAIL_THRESHOLD: u32 = 10;

/// The lines wireproxy prints that mean anything to us.
pub const HANDSHAKE_TIMEOUT_LINE: &str = "Handshake did not complete after 5 seconds";
pub const HANDSHAKE_OK_LINE: &str = "Received handshake response";

/// Counts consecutive handshake failures and says when to give up.
///
/// Kept here rather than in each host so the threshold, and the fact that a
/// success resets it, cannot drift between platforms.
#[derive(Debug, Default, Clone)]
pub struct HandshakeWatch {
    failures: u32,
    signalled: bool,
}

impl HandshakeWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line of wireproxy output. True means give up on the tunnel —
    /// returned **once** per watch, so a caller cannot stop a server twice.
    pub fn observe(&mut self, line: &str) -> bool {
        if line.contains(HANDSHAKE_TIMEOUT_LINE) {
            self.failures += 1;
            if self.failures >= HANDSHAKE_FAIL_THRESHOLD && !self.signalled {
                self.signalled = true;
                return true;
            }
        } else if line.contains(HANDSHAKE_OK_LINE) {
            self.failures = 0;
        }
        false
    }

    /// True once the tunnel has recovered after having been given up on, which
    /// is worth telling the user.
    pub fn recovered(&self) -> bool {
        self.signalled && self.failures == 0
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_three_states_reach_the_bridge() {
        assert_eq!(State::Running.wire(), Some("running"));
        assert_eq!(State::Stopped.wire(), Some("stopped"));
        assert_eq!(State::Crashed.wire(), Some("crashed"));
        assert_eq!(State::Starting.wire(), None);
        assert_eq!(State::Stopping.wire(), None);
    }

    #[test]
    fn the_api_hears_stopped_for_a_crash() {
        assert_eq!(State::Crashed.api_status(), Some("stopped"));
        assert_eq!(State::Running.api_status(), Some("running"));
        assert_eq!(State::Starting.api_status(), None);
    }

    /// A Minecraft server exits 0 on `stop`, so intent is what distinguishes
    /// a clean shutdown from a fall-over.
    #[test]
    fn intent_decides_stopped_versus_crashed() {
        assert_eq!(exit_state(true, 0), State::Stopped);
        assert_eq!(exit_state(true, 1), State::Stopped);
        assert_eq!(exit_state(false, 0), State::Stopped);
        assert_eq!(exit_state(false, 1), State::Crashed);
        assert_eq!(exit_state(false, 137), State::Crashed);
    }

    #[test]
    fn the_watch_gives_up_on_the_tenth_consecutive_failure() {
        let mut watch = HandshakeWatch::new();
        for attempt in 1..HANDSHAKE_FAIL_THRESHOLD {
            assert!(
                !watch.observe(HANDSHAKE_TIMEOUT_LINE),
                "gave up at {attempt}"
            );
        }
        assert!(
            watch.observe(HANDSHAKE_TIMEOUT_LINE),
            "did not give up at 10"
        );
    }

    /// A slow network that eventually connects must not stop the server.
    #[test]
    fn a_success_resets_the_count() {
        let mut watch = HandshakeWatch::new();
        for _ in 0..9 {
            watch.observe(HANDSHAKE_TIMEOUT_LINE);
        }
        watch.observe(HANDSHAKE_OK_LINE);
        assert_eq!(watch.failures(), 0);
        for _ in 0..9 {
            assert!(!watch.observe(HANDSHAKE_TIMEOUT_LINE));
        }
    }

    /// Stopping the server twice would race the world save against itself.
    #[test]
    fn it_only_ever_gives_up_once() {
        let mut watch = HandshakeWatch::new();
        let mut signals = 0;
        for _ in 0..40 {
            if watch.observe(HANDSHAKE_TIMEOUT_LINE) {
                signals += 1;
            }
        }
        assert_eq!(signals, 1);
    }

    #[test]
    fn recovery_is_only_reported_after_giving_up() {
        let mut watch = HandshakeWatch::new();
        watch.observe(HANDSHAKE_OK_LINE);
        assert!(!watch.recovered(), "nothing had failed yet");

        for _ in 0..HANDSHAKE_FAIL_THRESHOLD {
            watch.observe(HANDSHAKE_TIMEOUT_LINE);
        }
        watch.observe(HANDSHAKE_OK_LINE);
        assert!(watch.recovered());
    }

    /// Real wireproxy output has a timestamp and peer prefix around it.
    #[test]
    fn it_matches_lines_as_wireproxy_actually_prints_them() {
        let mut watch = HandshakeWatch::new();
        let line = "DEBUG: 2026/08/07 20:45:14 peer(FkEG…eFE8) - \
                    Handshake did not complete after 5 seconds, retrying (try 2)";
        for _ in 0..HANDSHAKE_FAIL_THRESHOLD - 1 {
            assert!(!watch.observe(line));
        }
        assert!(watch.observe(line));
    }

    #[test]
    fn network_failures_use_the_contract_names() {
        assert_eq!(NetworkFailure::Provisioning.wire(), "provisioning");
        assert_eq!(NetworkFailure::Handshake.wire(), "handshake");
    }
}

//! Server state machine.
//!
//! One server runs at a time (the engine keeps global state and switches
//! worlds by process CWD), so this is a single global rather than a map. The
//! transition rules are enforced here instead of in each host, because both
//! platforms otherwise reinvent them and drift.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed,
}

impl ServerState {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Crashed => "crashed",
        }
    }

    /// Whether the engine is occupied. Used to reject a second start.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }

    /// Legal transitions. Anything else is a bug in the host.
    pub fn can_transition_to(self, next: ServerState) -> bool {
        use ServerState::*;
        match (self, next) {
            (Stopped | Crashed, Starting) => true,
            (Starting, Running | Crashed | Stopped) => true,
            (Running, Stopping | Crashed) => true,
            // A stop can still fail, and the engine may die mid-shutdown.
            (Stopping, Stopped | Crashed) => true,
            _ => false,
        }
    }
}

impl fmt::Display for ServerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire())
    }
}

/// Which server the engine is bound to, and how far along it is.
pub struct ServerStatus {
    pub state: ServerState,
    pub server_id: Option<String>,
    /// Milliseconds since the epoch when it reached `Running`.
    pub started_at_ms: Option<u64>,
    pub port: Option<u16>,
}

impl ServerStatus {
    pub fn idle() -> Self {
        Self {
            state: ServerState::Stopped,
            server_id: None,
            started_at_ms: None,
            port: None,
        }
    }

    /// Apply a transition, returning an error rather than corrupting state.
    pub fn transition(&mut self, next: ServerState) -> Result<(), String> {
        if !self.state.can_transition_to(next) {
            return Err(format!(
                "illegal server state transition {} -> {}",
                self.state, next
            ));
        }
        self.state = next;
        if matches!(next, ServerState::Stopped | ServerState::Crashed) {
            self.started_at_ms = None;
            self.port = None;
            self.server_id = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_run_is_allowed_end_to_end() {
        let mut status = ServerStatus::idle();
        status.server_id = Some("s1".into());
        for next in [
            ServerState::Starting,
            ServerState::Running,
            ServerState::Stopping,
            ServerState::Stopped,
        ] {
            status.transition(next).expect("legal transition");
        }
        assert_eq!(status.state, ServerState::Stopped);
    }

    #[test]
    fn a_failed_start_lands_in_a_restartable_state() {
        let mut status = ServerStatus::idle();
        status.transition(ServerState::Starting).unwrap();
        // e.g. the port was taken
        status.transition(ServerState::Stopped).unwrap();
        assert!(status.state.can_transition_to(ServerState::Starting));
    }

    #[test]
    fn a_crash_can_be_restarted() {
        let mut status = ServerStatus::idle();
        status.transition(ServerState::Starting).unwrap();
        status.transition(ServerState::Running).unwrap();
        status.transition(ServerState::Crashed).unwrap();
        assert!(status.state.can_transition_to(ServerState::Starting));
    }

    #[test]
    fn cannot_start_twice() {
        let mut status = ServerStatus::idle();
        status.transition(ServerState::Starting).unwrap();
        assert!(status.transition(ServerState::Starting).is_err());
        assert_eq!(status.state, ServerState::Starting);
    }

    #[test]
    fn cannot_run_without_starting() {
        let mut status = ServerStatus::idle();
        assert!(status.transition(ServerState::Running).is_err());
    }

    #[test]
    fn stopping_clears_the_run_details() {
        let mut status = ServerStatus::idle();
        status.server_id = Some("s1".into());
        status.transition(ServerState::Starting).unwrap();
        status.transition(ServerState::Running).unwrap();
        status.started_at_ms = Some(123);
        status.port = Some(25565);

        status.transition(ServerState::Stopping).unwrap();
        status.transition(ServerState::Stopped).unwrap();

        // Stale port/uptime after a stop would be reported to the UI as if
        // the server were still reachable.
        assert!(status.port.is_none());
        assert!(status.started_at_ms.is_none());
        assert!(status.server_id.is_none());
    }

    #[test]
    fn active_covers_every_occupied_state() {
        assert!(ServerState::Starting.is_active());
        assert!(ServerState::Running.is_active());
        assert!(ServerState::Stopping.is_active());
        assert!(!ServerState::Stopped.is_active());
        assert!(!ServerState::Crashed.is_active());
    }
}

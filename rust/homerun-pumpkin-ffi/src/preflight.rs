//! Bind pre-flight.
//!
//! The engine calls `process::exit(1)` when it cannot bind its port. In a
//! CLI that is merely rude; inside an app it terminates the whole process
//! with no crash report and no way for the UI to explain what happened.
//!
//! So we try the bind ourselves first and refuse to start on failure. This
//! is a race — something could take the port between our probe and the
//! engine's bind — but it converts the overwhelmingly common case (the user
//! already has a server running, or another app holds 25565) from a silent
//! app death into an error message.

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, UdpSocket};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Udp,
}

/// Check a port is bindable on all interfaces, releasing it immediately.
pub fn port_available(port: u16, transport: Transport) -> bool {
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    match transport {
        Transport::Tcp => TcpListener::bind(addr).is_ok(),
        Transport::Udp => UdpSocket::bind(addr).is_ok(),
    }
}

/// Every port a server needs, checked together so the user gets one message
/// instead of discovering the next conflict on the following attempt.
///
/// `Ok(())` or a player-facing description of what is in the way.
pub fn check_ports(java_port: u16, bedrock_port: Option<u16>) -> Result<(), String> {
    let mut taken = Vec::new();

    if !port_available(java_port, Transport::Tcp) {
        taken.push(format!("{java_port} (Java Edition)"));
    }
    if let Some(port) = bedrock_port {
        if !port_available(port, Transport::Udp) {
            taken.push(format!("{port} (Bedrock Edition)"));
        }
    }

    if taken.is_empty() {
        return Ok(());
    }

    // Shown verbatim in the app, so it has to make sense to a player.
    Err(format!(
        "Port {} already in use. Another server may still be running — stop it and try again.",
        taken.join(" and ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unused_port_is_available() {
        // Port 0 asks the OS for a free one, then we probe what it gave us.
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        assert!(port_available(port, Transport::Tcp));
    }

    #[test]
    fn a_held_port_is_detected() {
        let held = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        assert!(!port_available(port, Transport::Tcp));
        assert!(check_ports(port, None).is_err());
    }

    #[test]
    fn the_error_names_the_port_and_stays_readable() {
        let held = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = held.local_addr().unwrap().port();

        let err = check_ports(port, None).unwrap_err();
        assert!(err.contains(&port.to_string()));
        assert!(err.contains("Java Edition"));
        // No jargon a player would have to look up.
        assert!(!err.to_lowercase().contains("bind"));
        assert!(!err.to_lowercase().contains("errno"));
    }

    #[test]
    fn the_probe_releases_the_port() {
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        // Probing must not itself hold the port, or the engine's own bind
        // would then fail — exactly what this exists to prevent.
        assert!(port_available(port, Transport::Tcp));
        assert!(port_available(port, Transport::Tcp));
        assert!(TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok());
    }
}

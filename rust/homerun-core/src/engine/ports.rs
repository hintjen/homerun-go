//! Which ports the gateway carries, and how they group into services.
//!
//! # Two numbers that are never the same number
//!
//! A descriptor's `port` is what the server prefers to bind **and** the
//! gateway's *dest* port. The port a player types is neither: public ports
//! come from the gateway's allocator pool and cannot be requested, so
//! external port never equals game port. Anything that assumes otherwise
//! produces a server that runs and cannot be joined.
//!
//! The third number is the port actually bound, which is the only one that
//! can move: the runner falls back when the preferred port is taken. That is
//! what a [`Forward`]'s target is, and what `{port:…}` resolves to.
//!
//! # Services
//!
//! A gateway *service* is at most one TCP mapping plus any number of UDP
//! mappings; a *link* carries many services. The descriptor groups its
//! exposed ports into services by name, and the API turns each group into one
//! gateway service on the server's single link.
//!
//! The Homerun API provisions **one** service per server today. That is the
//! API's own limit rather than the gateway's, so a game needing two is a
//! platform gap to be recorded — a YELLOW verdict — and not an engine error.
//! [`super::doctor`] reports it as a warning for exactly that reason.

use std::collections::BTreeMap;

use super::descriptor::Port;
use crate::tunnel::{Forward, Protocol};
use crate::{Error, Result};

/// The service an exposed port belongs to.
///
/// An exposed port that names no service is a service of its own, named after
/// the port — which is the right answer for a single-port game and keeps the
/// common descriptor short.
pub fn service_of(port: &Port) -> &str {
    match &port.service {
        Some(name) if !name.is_empty() => name,
        _ => &port.name,
    }
}

/// Exposed ports, grouped into the services the gateway will see.
pub fn services(ports: &[Port]) -> BTreeMap<&str, Vec<&Port>> {
    let mut grouped: BTreeMap<&str, Vec<&Port>> = BTreeMap::new();
    for port in ports.iter().filter(|p| p.expose) {
        grouped.entry(service_of(port)).or_default().push(port);
    }
    grouped
}

/// The wireproxy forwards for a running server.
///
/// `bound` is what the server actually bound, by port name. A port the host
/// did not report is an error rather than a guess: forwarding to the port the
/// descriptor *preferred* when the server moved off it is a tunnel that
/// connects, loads cleanly and carries nothing.
pub fn forwards(ports: &[Port], bound: &BTreeMap<String, u16>) -> Result<Vec<Forward>> {
    let mut out = Vec::new();
    for port in ports.iter().filter(|p| p.expose) {
        let target = *bound.get(&port.name).ok_or_else(|| {
            Error::Malformed(format!(
                "this server did not report which port it bound for \"{}\", so it \
                 cannot be made reachable.",
                port.name
            ))
        })?;
        out.push(Forward {
            protocol: port.proto,
            // The gateway-facing port is the descriptor's, never the bound
            // one -- see the module header and `crate::tunnel`.
            listen_port: port.port,
            target_port: target,
        });
    }
    Ok(out)
}

/// Whether these ports fit the one-service-per-server shape the API
/// provisions today.
///
/// Returns what is wrong, in words, or nothing. Not an error: a game that
/// needs more is a platform gap.
pub fn fits_one_service(ports: &[Port]) -> Option<String> {
    let grouped = services(ports);
    if grouped.len() > 1 {
        let names: Vec<&str> = grouped.keys().copied().collect();
        return Some(format!(
            "this game exposes {} groups of ports ({}), and Homerun can publish only \
             one group per server today.",
            grouped.len(),
            names.join(", ")
        ));
    }
    for (name, group) in &grouped {
        let tcp = group.iter().filter(|p| p.proto == Protocol::Tcp).count();
        if tcp > 1 {
            return Some(format!(
                "this game needs {tcp} TCP ports in its \"{name}\" group, and Homerun \
                 can publish only one per group today."
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::descriptor::GameDescriptor;
    use serde_json::json;

    fn rust() -> GameDescriptor {
        GameDescriptor::parse(include_str!("testdata/rust.json")).unwrap()
    }

    fn bound(pairs: &[(&str, u16)]) -> BTreeMap<String, u16> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn only_exposed_ports_are_carried() {
        let f = forwards(
            &rust().ports,
            &bound(&[("game", 28015), ("query", 28017), ("rcon", 28016)]),
        )
        .unwrap();
        assert_eq!(
            f.len(),
            2,
            "rcon is loopback-only and must not be forwarded"
        );
        assert!(f.iter().all(|f| f.protocol == Protocol::Udp));
    }

    /// The distinction the module header opens with, as an assertion: the
    /// gateway-facing port follows the descriptor, the target follows what
    /// the server actually bound.
    #[test]
    fn a_server_that_moved_off_its_preferred_port_is_still_reachable() {
        let f = forwards(
            &rust().ports,
            &bound(&[("game", 30001), ("query", 30002), ("rcon", 30003)]),
        )
        .unwrap();
        let game = f
            .iter()
            .find(|f| f.listen_port == 28015)
            .expect("dest port is the descriptor's");
        assert_eq!(game.target_port, 30001, "target follows the bound port");
    }

    #[test]
    fn a_port_the_server_never_reported_is_refused_rather_than_guessed() {
        let err = forwards(&rust().ports, &bound(&[("game", 28015)]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("query"), "{err}");
        assert!(!err.contains("None"), "reads as a verdict: {err}");
    }

    #[test]
    fn the_pilot_groups_its_exposed_ports_into_one_service() {
        let d = rust();
        let grouped = services(&d.ports);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped["game"].len(), 2);
        assert!(fits_one_service(&rust().ports).is_none());
    }

    #[test]
    fn an_exposed_port_with_no_service_is_its_own() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "terraria",
            "ports": [{ "name": "game", "proto": "tcp", "port": 7777, "expose": true }]
        }))
        .unwrap();
        assert_eq!(services(&d.ports).keys().collect::<Vec<_>>(), vec![&"game"]);
        assert!(fits_one_service(&d.ports).is_none());
    }

    /// A platform gap, reported in words rather than raised as an error.
    #[test]
    fn two_groups_are_named_as_something_homerun_cannot_publish_yet() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "ports": [
                { "name": "game", "proto": "udp", "port": 1, "expose": true, "service": "game" },
                { "name": "web",  "proto": "tcp", "port": 2, "expose": true, "service": "web" }
            ]
        }))
        .unwrap();
        let why = fits_one_service(&d.ports).expect("two groups should be reported");
        assert!(why.contains("game") && why.contains("web"), "{why}");
    }

    #[test]
    fn two_tcp_ports_in_one_group_are_named_too() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "ports": [
                { "name": "a", "proto": "tcp", "port": 1, "expose": true, "service": "s" },
                { "name": "b", "proto": "tcp", "port": 2, "expose": true, "service": "s" }
            ]
        }))
        .unwrap();
        let why = fits_one_service(&d.ports).expect("two TCP ports should be reported");
        assert!(why.contains("TCP") && why.contains('s'), "{why}");
    }

    #[test]
    fn a_game_with_nothing_exposed_forwards_nothing_and_fits() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "ports": [{ "name": "rcon", "proto": "tcp", "port": 1 }]
        }))
        .unwrap();
        assert!(forwards(&d.ports, &bound(&[])).unwrap().is_empty());
        assert!(fits_one_service(&d.ports).is_none());
    }
}

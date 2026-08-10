//! The wireproxy config that makes a hosted server reachable.
//!
//! Reference: `src/electron/wireproxyConfig.ts` in the `homerun` repo. The
//! gateway is the same on every platform, so a divergence here is a bug by
//! definition — the tests below are byte-exact against what the desktop
//! generates.
//!
//! # Nothing here knows what game it is carrying
//!
//! A tunnel forwards ports. Which ports, and whether they are TCP or UDP, is
//! the game's business — [`crate::game::Game::forwards`] answers it, and a
//! second game answers it for itself without this module changing. What lives here is the part that is the same regardless: the
//! WireGuard interface, the peer, the keepalive, and the shape of a forward.
//!
//! # The numbers that must not change
//!
//! Every `ListenPort` is **fixed by the gateway**, not by the server. The
//! gateway DNATs player traffic to a known port on the WireGuard interface
//! whatever local port the server actually bound; only `Target` follows the
//! local one. "Correcting" a `ListenPort` to match the local port produces a
//! config that loads cleanly, connects cleanly, and is unreachable.

use serde::{Deserialize, Serialize};

/// The gateway's half of the tunnel — the API's `native_config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub client_privkey: String,
    pub gateway_pubkey: String,
    /// `host:port` — the gateway's WireGuard UDP endpoint.
    pub link_address: String,
    /// Gateway v2 only: the /32 this peer was allocated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Gateway v2 only: the gateway's own address on the shared interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_ips: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

/// One port carried from the gateway to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub protocol: Protocol,
    /// The gateway-facing port. Fixed — see the module docs.
    pub listen_port: u16,
    /// The loopback port the server actually bound.
    pub target_port: u16,
}

impl Forward {
    pub fn tcp(listen_port: u16, target_port: u16) -> Self {
        Self {
            protocol: Protocol::Tcp,
            listen_port,
            target_port,
        }
    }

    pub fn udp(listen_port: u16, target_port: u16) -> Self {
        Self {
            protocol: Protocol::Udp,
            listen_port,
            target_port,
        }
    }

    fn render(&self) -> String {
        // `[UDPServerTunnel]` exists only in our wireproxy fork. Upstream has
        // no inbound UDP tunnel at all, which is why anything using UDP needs
        // the fork rather than a released build.
        let section = match self.protocol {
            Protocol::Tcp => "TCPServerTunnel",
            Protocol::Udp => "UDPServerTunnel",
        };
        format!(
            "[{section}]\nListenPort = {}\nTarget = 127.0.0.1:{}\n",
            self.listen_port, self.target_port
        )
    }
}

/// Everything a config needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub link: Link,
    /// Rendered in order. Empty is legal and produces a tunnel that connects
    /// and carries nothing — useful only for testing reachability.
    pub forwards: Vec<Forward>,
}

/// The legacy gateway hardcoded both sides of a per-server tunnel. Gateway v2
/// runs one shared multi-peer interface and allocates per peer, so those
/// values arrive in the link and these are only the fallback.
const LEGACY_CLIENT_ADDRESS: &str = "10.0.0.2/24";
const LEGACY_GATEWAY_ADDRESS: &str = "10.0.0.1/32";

/// 1280 keeps the tunnel inside the smallest MTU we expect to cross.
const MTU: u16 = 1280;

impl Config {
    /// Render the INI wireproxy reads.
    pub fn render(&self) -> String {
        let mut out = String::new();

        out.push_str("[Interface]\n");
        out.push_str(&format!("PrivateKey = {}\n", self.link.client_privkey));
        out.push_str(&format!(
            "Address = {}\n",
            self.link
                .address
                .as_deref()
                .unwrap_or(LEGACY_CLIENT_ADDRESS)
        ));
        out.push_str(&format!("MTU = {MTU}\n"));
        out.push('\n');

        out.push_str("[Peer]\n");
        out.push_str(&format!("PublicKey = {}\n", self.link.gateway_pubkey));
        out.push_str(&format!("Endpoint = {}\n", self.link.link_address));
        out.push_str(&format!(
            "AllowedIPs = {}\n",
            self.link
                .allowed_ips
                .as_deref()
                .unwrap_or(LEGACY_GATEWAY_ADDRESS)
        ));
        // Holds the NAT mapping open. On mobile this is what keeps a tunnel
        // usable across a carrier's aggressive UDP timeouts.
        out.push_str("PersistentKeepalive = 30\n");

        for forward in &self.forwards {
            out.push('\n');
            out.push_str(&forward.render());
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_link() -> Link {
        Link {
            client_privkey: "CLIENT_PRIVATE_KEY".into(),
            gateway_pubkey: "GATEWAY_PUBLIC_KEY".into(),
            link_address: "gateway.example.com:51820".into(),
            address: None,
            allowed_ips: None,
        }
    }

    fn v2_link() -> Link {
        Link {
            address: Some("10.100.0.3/32".into()),
            allowed_ips: Some("10.100.0.1/32".into()),
            ..legacy_link()
        }
    }

    /// Byte-exact against `generateWireproxyConfig(config, 25565)`.
    #[test]
    fn a_single_tcp_forward_matches_the_desktop() {
        let config = Config {
            link: legacy_link(),
            forwards: vec![Forward::tcp(25565, 25565)],
        };
        assert_eq!(
            config.render(),
            "[Interface]\n\
             PrivateKey = CLIENT_PRIVATE_KEY\n\
             Address = 10.0.0.2/24\n\
             MTU = 1280\n\
             \n\
             [Peer]\n\
             PublicKey = GATEWAY_PUBLIC_KEY\n\
             Endpoint = gateway.example.com:51820\n\
             AllowedIPs = 10.0.0.1/32\n\
             PersistentKeepalive = 30\n\
             \n\
             [TCPServerTunnel]\n\
             ListenPort = 25565\n\
             Target = 127.0.0.1:25565\n"
        );
    }

    /// The local port moves; the gateway-facing one must not.
    #[test]
    fn a_nonstandard_local_port_moves_only_the_target() {
        let rendered = Config {
            link: legacy_link(),
            forwards: vec![Forward::tcp(25565, 25570)],
        }
        .render();
        assert!(rendered.contains("ListenPort = 25565"));
        assert!(rendered.contains("Target = 127.0.0.1:25570"));
    }

    #[test]
    fn gateway_v2_supplies_both_addresses() {
        let rendered = Config {
            link: v2_link(),
            forwards: vec![Forward::tcp(25565, 25565)],
        }
        .render();
        assert!(rendered.contains("Address = 10.100.0.3/32"));
        assert!(rendered.contains("AllowedIPs = 10.100.0.1/32"));
    }

    #[test]
    fn the_private_key_is_emitted_verbatim() {
        let rendered = Config {
            link: legacy_link(),
            forwards: vec![],
        }
        .render();
        assert!(rendered.contains("PrivateKey = CLIENT_PRIVATE_KEY"));
    }

    #[test]
    fn udp_forwards_use_the_forks_section() {
        let rendered = Config {
            link: legacy_link(),
            forwards: vec![Forward::udp(19132, 19132)],
        }
        .render();
        assert!(rendered.contains("[UDPServerTunnel]"), "{rendered}");
    }

    /// Order is the caller's, and every forward is separated by a blank line.
    #[test]
    fn forwards_render_in_order() {
        let rendered = Config {
            link: legacy_link(),
            forwards: vec![
                Forward::tcp(25565, 25565),
                Forward::udp(19132, 19132),
                Forward::udp(24454, 24454),
            ],
        }
        .render();
        let tcp = rendered.find("[TCPServerTunnel]").unwrap();
        let first_udp = rendered.find("[UDPServerTunnel]").unwrap();
        assert!(tcp < first_udp, "declared order must survive");
        assert_eq!(rendered.matches("[UDPServerTunnel]").count(), 2);
        assert!(!rendered.contains("\n\n\n"), "no double blank lines");
    }

    #[test]
    fn ends_with_a_newline() {
        let rendered = Config {
            link: legacy_link(),
            forwards: vec![Forward::tcp(25565, 25565)],
        }
        .render();
        assert!(rendered.ends_with('\n'));
    }

    /// A tunnel with nothing to carry still produces a valid interface and
    /// peer, so a host can bring one up before it knows the ports.
    #[test]
    fn no_forwards_still_renders_a_valid_peer() {
        let rendered = Config {
            link: legacy_link(),
            forwards: vec![],
        }
        .render();
        assert!(rendered.contains("[Peer]"));
        assert!(!rendered.contains("ServerTunnel"));
        assert!(rendered.ends_with("PersistentKeepalive = 30\n"));
    }
}

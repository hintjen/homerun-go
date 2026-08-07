//! The wireproxy config that makes a hosted server reachable.
//!
//! Reference: `src/electron/wireproxyConfig.ts` in the `homerun` repo. The
//! gateway is the same on every platform, so a divergence here is a bug by
//! definition — the tests below are byte-exact against what the desktop
//! generates.
//!
//! # The numbers that must not change
//!
//! Every `ListenPort` is **fixed**. The gateway always DNATs player traffic to
//! 25565 (Java), 19132 (Bedrock) and 24454 (voice) on the WireGuard interface,
//! whatever local port the server actually bound. Only `Target` follows the
//! local port. Editing a `ListenPort` to "match" the local one produces a
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

/// What the server needs exposed.
///
/// The desktop expresses this as three booleans (`udp`, `crossplay`,
/// `voiceChat`) that cannot all be meaningfully combined; making it an enum
/// removes the states nobody can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// Java Edition. One TCP tunnel.
    Java { port: u16 },
    /// Bedrock dedicated server. One UDP tunnel.
    Bedrock { port: u16 },
    /// Java plus Geyser, so Bedrock clients can join a Java world.
    Crossplay { java_port: u16, geyser_port: u16 },
}

/// Everything a config needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub link: Link,
    pub exposure: Exposure,
    /// Simple Voice Chat's local port, when the server runs it.
    pub voice_chat_port: Option<u16>,
}

/// The legacy gateway hardcoded both sides of a per-server tunnel. Gateway v2
/// runs one shared multi-peer interface and allocates per peer, so those
/// values arrive in the link and these are only the fallback.
const LEGACY_CLIENT_ADDRESS: &str = "10.0.0.2/24";
const LEGACY_GATEWAY_ADDRESS: &str = "10.0.0.1/32";

/// Gateway-facing ports. See the module docs before touching these.
const LISTEN_JAVA: u16 = 25565;
const LISTEN_BEDROCK: u16 = 19132;
const LISTEN_VOICE: u16 = 24454;

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
        out.push('\n');

        match self.exposure {
            Exposure::Java { port } => {
                out.push_str(&tcp_tunnel(LISTEN_JAVA, port));
            }
            Exposure::Bedrock { port } => {
                out.push_str(&udp_tunnel(LISTEN_BEDROCK, port));
            }
            Exposure::Crossplay {
                java_port,
                geyser_port,
            } => {
                out.push_str(&tcp_tunnel(LISTEN_JAVA, java_port));
                out.push('\n');
                out.push_str(&udp_tunnel(LISTEN_BEDROCK, geyser_port));
            }
        }

        if let Some(voice) = self.voice_chat_port {
            out.push('\n');
            out.push_str(&udp_tunnel(LISTEN_VOICE, voice));
        }

        out
    }
}

fn tcp_tunnel(listen: u16, target: u16) -> String {
    format!("[TCPServerTunnel]\nListenPort = {listen}\nTarget = 127.0.0.1:{target}\n")
}

/// `[UDPServerTunnel]` exists only in our wireproxy fork. Upstream has no
/// inbound UDP tunnel at all, which is why Bedrock, crossplay and voice chat
/// need the fork rather than a released build.
fn udp_tunnel(listen: u16, target: u16) -> String {
    format!("[UDPServerTunnel]\nListenPort = {listen}\nTarget = 127.0.0.1:{target}\n")
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
    fn java_matches_the_desktop() {
        let config = Config {
            link: legacy_link(),
            exposure: Exposure::Java { port: 25565 },
            voice_chat_port: None,
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
        let config = Config {
            link: legacy_link(),
            exposure: Exposure::Java { port: 25570 },
            voice_chat_port: None,
        };
        let rendered = config.render();
        assert!(rendered.contains("ListenPort = 25565"), "{rendered}");
        assert!(rendered.contains("Target = 127.0.0.1:25570"), "{rendered}");
    }

    #[test]
    fn gateway_v2_supplies_both_addresses() {
        let config = Config {
            link: v2_link(),
            exposure: Exposure::Java { port: 25565 },
            voice_chat_port: None,
        };
        let rendered = config.render();
        assert!(rendered.contains("Address = 10.100.0.3/32"), "{rendered}");
        assert!(
            rendered.contains("AllowedIPs = 10.100.0.1/32"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("10.0.0."),
            "the legacy fallback leaked into a v2 config:\n{rendered}"
        );
    }

    #[test]
    fn bedrock_is_udp_on_19132() {
        let config = Config {
            link: legacy_link(),
            exposure: Exposure::Bedrock { port: 19132 },
            voice_chat_port: None,
        };
        let rendered = config.render();
        assert!(rendered.contains("[UDPServerTunnel]"), "{rendered}");
        assert!(rendered.contains("ListenPort = 19132"), "{rendered}");
        assert!(
            !rendered.contains("[TCPServerTunnel]"),
            "bedrock has no TCP tunnel:\n{rendered}"
        );
    }

    /// Crossplay is the one shape that needs both, and the reason the fork
    /// exists at all.
    #[test]
    fn crossplay_emits_tcp_then_udp() {
        let config = Config {
            link: legacy_link(),
            exposure: Exposure::Crossplay {
                java_port: 25565,
                geyser_port: 19132,
            },
            voice_chat_port: None,
        };
        let rendered = config.render();
        let tcp = rendered.find("[TCPServerTunnel]").expect("tcp section");
        let udp = rendered.find("[UDPServerTunnel]").expect("udp section");
        assert!(tcp < udp, "desktop emits TCP first:\n{rendered}");
        assert!(rendered.contains("ListenPort = 25565"));
        assert!(rendered.contains("ListenPort = 19132"));
    }

    #[test]
    fn voice_chat_appends_its_own_udp_tunnel() {
        let config = Config {
            link: legacy_link(),
            exposure: Exposure::Java { port: 25565 },
            voice_chat_port: Some(24460),
        };
        let rendered = config.render();
        assert!(rendered.contains("ListenPort = 24454"), "{rendered}");
        assert!(rendered.contains("Target = 127.0.0.1:24460"), "{rendered}");
    }

    #[test]
    fn crossplay_and_voice_chat_together_emit_three_tunnels() {
        let config = Config {
            link: legacy_link(),
            exposure: Exposure::Crossplay {
                java_port: 25565,
                geyser_port: 19132,
            },
            voice_chat_port: Some(24454),
        };
        let rendered = config.render();
        assert_eq!(
            rendered.matches("[UDPServerTunnel]").count(),
            2,
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("[TCPServerTunnel]").count(),
            1,
            "{rendered}"
        );
    }

    /// wireproxy's INI parser wants a trailing newline on the last section,
    /// and the desktop supplies one. Easy to lose in a refactor.
    #[test]
    fn ends_with_a_newline() {
        let config = Config {
            link: legacy_link(),
            exposure: Exposure::Java { port: 25565 },
            voice_chat_port: None,
        };
        assert!(config.render().ends_with('\n'));
    }

    /// The private key is the one value in here that must never be logged or
    /// truncated. Guards against a "tidy up the render" change that quotes or
    /// wraps it.
    #[test]
    fn the_private_key_is_emitted_verbatim() {
        let mut link = legacy_link();
        link.client_privkey = "aB3+/xyz=".into();
        let config = Config {
            link,
            exposure: Exposure::Java { port: 25565 },
            voice_chat_port: None,
        };
        assert!(config.render().contains("PrivateKey = aB3+/xyz=\n"));
    }
}

//! Being found on the local network: the bind and the announcement.
//!
//! # Two discoveries, one switch
//!
//! Minecraft's Multiplayer screen says "Scanning for games on your local
//! network" and lists whatever shouts at it: a Java Edition client listens on
//! the multicast group `224.0.2.60:4445` for `[MOTD]…[/MOTD][AD]port[/AD]`,
//! sent every 1.5 s. Only the client's own integrated server ("Open to LAN")
//! ever sends that — a dedicated Paper or vanilla server never announces
//! itself, so a host that wants its server listed has to send the announcement on
//! the server's behalf. Bedrock works the other way round: the client
//! broadcasts RakNet pings and lists whoever answers, which PowerNukkitX does
//! natively as soon as it is reachable.
//!
//! Neither works while a server is bound to loopback, which is every phone's
//! default and the desktop's without its toggle. So "expose to the local
//! network" means two things at once — bind every interface, and announce —
//! and this module is the shared half of both: which address to bind, what to
//! print about it, and the exact bytes of the announcement. Three hosts had the
//! console line spelled three ways before this; the announcement format is the kind
//! of thing that is right on the first host and subtly wrong on the second.
//!
//! The *sending* is a host effect and stays there: a `dgram` socket on the
//! desktop, a `DatagramSocket` under a multicast lock on Android, and Pumpkin's
//! own `lan_broadcast` task on the hosts that run it — which formats the same
//! payload, so the two spellings must agree and the test below pins that.
//! iOS additionally needs Apple's multicast entitlement before a send leaves
//! the phone at all.

use serde::{Deserialize, Serialize};

/// The multicast group every Java Edition client listens on for LAN games.
pub const MULTICAST_GROUP: &str = "224.0.2.60";
/// The port it listens on.
pub const MULTICAST_PORT: u16 = 4445;
/// How often the client expects to hear a game before it drops it from the
/// list. The integrated server sends every 1.5 s; so does Pumpkin.
pub const INTERVAL_MS: u64 = 1500;

/// Where a server listens, and what to tell the player about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bind {
    /// `0.0.0.0` when exposed to the local network, `127.0.0.1` otherwise.
    pub address: String,
    /// A console line worth printing, only when exposed: a server reachable
    /// by every device on the Wi-Fi is worth saying out loud.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

/// The bind for a server, given whether the player exposed it.
///
/// Loopback is the default everywhere and on purpose: players reach a server
/// through the gateway tunnel, and a phone on a shared network has no
/// business listening on every interface unless its owner said so.
pub fn bind(exposed: bool, port: u16) -> Bind {
    if exposed {
        Bind {
            address: "0.0.0.0".into(),
            line: Some(format!(
                "[Homerun] Local network exposure is on — binding 0.0.0.0:{port} so other \
                 devices on your network can connect, and announcing the server to \
                 Minecraft's local-network list."
            )),
        }
    } else {
        Bind {
            address: "127.0.0.1".into(),
            line: None,
        }
    }
}

/// What an announcement sender needs: the bytes, where to send them, and how often.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Announcement {
    /// `[MOTD]name[/MOTD][AD]port[/AD]`, as the client parses it.
    pub payload: String,
    pub group: String,
    pub port: u16,
    pub interval_ms: u64,
}

/// The announcement for a server, from its MOTD and the port it bound.
///
/// The client takes the text between `[MOTD]` and the first `[/MOTD]` and
/// draws it as one line through its ordinary text renderer — so the classic
/// `§a`-style colours and `§l`/`§o` styles a player put in their MOTD show in
/// the list exactly as they do in the server list, and are kept. What the
/// line cannot show is folded or dropped: line breaks become spaces, and the
/// `§x§r§r§g§g§b§b` hex form (RGB, and the gradients built from it) is
/// removed, because the legacy parser reads it as six stray codes. The two
/// closing tags are removed from the text so a joker's MOTD cannot end the
/// name early or forge the port. Nothing visible falls back to the words
/// vanilla shows for an unnamed server.
pub fn announcement(motd: &str, port: u16) -> Announcement {
    Announcement {
        payload: format!("[MOTD]{}[/MOTD][AD]{port}[/AD]", list_name(motd)),
        group: MULTICAST_GROUP.into(),
        port: MULTICAST_PORT,
        interval_ms: INTERVAL_MS,
    }
}

/// The MOTD as one line the LAN list can show, formatting kept.
pub fn list_name(motd: &str) -> String {
    let chars: Vec<char> = motd.chars().collect();
    let mut out = String::with_capacity(motd.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // `§x` introduces a hex colour as six more `§?` pairs — fourteen
        // characters the LAN line would render as garbage.
        if c == '§'
            && i + 13 < chars.len()
            && matches!(chars[i + 1], 'x' | 'X')
            && (1..=6).all(|n| chars[i + 2 * n] == '§')
        {
            i += 14;
            continue;
        }
        match c {
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
        i += 1;
    }
    let flat = out.replace("[/MOTD]", "").replace("[/AD]", "");
    let trimmed = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if visible_text(&trimmed).is_empty() {
        "A Minecraft Server".into()
    } else {
        trimmed
    }
}

/// The text with every `§?` code removed — what a player actually sees.
fn visible_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact bytes a Java client lists. Pumpkin formats the same string
    /// in `net/lan_broadcast.rs`; a host sending this for a JVM must match.
    #[test]
    fn the_announcement_is_what_the_client_parses() {
        let b = announcement("Player's Minecraft Server", 25565);
        assert_eq!(
            b.payload,
            "[MOTD]Player's Minecraft Server[/MOTD][AD]25565[/AD]"
        );
        assert_eq!(b.group, "224.0.2.60");
        assert_eq!(b.port, 4445);
        assert_eq!(b.interval_ms, 1500);
    }

    /// The list shows one line, coloured and styled as the player wrote it —
    /// the same look as the server list, as near as a LAN entry can get.
    #[test]
    fn the_name_keeps_its_colours_on_one_line() {
        assert_eq!(
            list_name("§aHomerun §fserver\nline two"),
            "§aHomerun §fserver line two"
        );
        assert_eq!(list_name("§l§6Bold gold§r plain"), "§l§6Bold gold§r plain");
        assert_eq!(list_name("  spaced   out  "), "spaced out");
    }

    /// What the line cannot render is dropped rather than shown as garbage:
    /// the hex form, alone or as a gradient. A lone `§x` with no hex pairs
    /// behind it is just a stray code, and stays.
    #[test]
    fn hex_colours_are_dropped_and_classic_ones_kept() {
        assert_eq!(list_name("§x§f§f§0§0§0§0Red §atext"), "Red §atext");
        assert_eq!(
            list_name("§x§a§b§c§d§e§fA§x§1§2§3§4§5§6B"),
            "AB",
            "a gradient is one hex code per letter"
        );
        assert_eq!(list_name("§xnot hex"), "§xnot hex");
    }

    /// Nothing visible — empty, or codes with no text — falls back to the
    /// words vanilla shows for an unnamed server.
    #[test]
    fn nothing_visible_gets_the_default_name() {
        assert_eq!(list_name(""), "A Minecraft Server");
        assert_eq!(list_name("§a§l"), "A Minecraft Server");
        assert_eq!(list_name("§x§f§f§0§0§0§0"), "A Minecraft Server");
    }

    /// A MOTD cannot close the tag early and hand the client a port of its
    /// own choosing.
    #[test]
    fn a_motd_cannot_forge_the_port() {
        let b = announcement("evil[/MOTD][AD]1337[/AD]", 25565);
        assert_eq!(b.payload, "[MOTD]evil[AD]1337[/MOTD][AD]25565[/AD]");
        assert_eq!(b.payload.matches("[/AD]").count(), 1);
    }

    /// Loopback unless the player said otherwise, and a line only when they
    /// did — the quiet default prints nothing.
    #[test]
    fn the_bind_is_loopback_unless_exposed() {
        let quiet = bind(false, 25565);
        assert_eq!(quiet.address, "127.0.0.1");
        assert!(quiet.line.is_none());

        let open = bind(true, 25566);
        assert_eq!(open.address, "0.0.0.0");
        let line = open.line.expect("an exposed server says so");
        assert!(
            line.starts_with("[Homerun] "),
            "badged for the console: {line}"
        );
        assert!(line.contains("0.0.0.0:25566"), "names the address: {line}");
    }

    #[test]
    fn the_bind_serialises_for_hosts() {
        let v = serde_json::to_value(bind(false, 1)).unwrap();
        assert_eq!(v, serde_json::json!({ "address": "127.0.0.1" }));
        let v = serde_json::to_value(bind(true, 1)).unwrap();
        assert_eq!(v["address"], "0.0.0.0");
        assert!(v["line"].is_string());
    }
}

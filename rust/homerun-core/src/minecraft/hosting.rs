//! Which servers a host can actually run.
//!
//! # Why this is not a host's decision
//!
//! Every host already refuses *something*. Android refuses Bedrock with a
//! string typed into its bridge router; iOS refuses it somewhere else, in
//! different words; and neither refused a modded server at all — so an iPhone
//! would accept a Forge pack, spend minutes fetching and unpacking it, start a
//! Pumpkin that loads none of it, and present the player with a working
//! vanilla world where their mods used to be. Nothing about that reads as a
//! refusal. It reads as data loss.
//!
//! That is the shape of divergence [`crate`] exists to remove: two platforms
//! answering the same question differently, both plausibly, one of them wrong.
//!
//! # The two limits, and why they are separate inputs
//!
//! **A linked engine runs vanilla only.** [`Engine::Linked`] is a server
//! compiled into the app — Pumpkin, on iOS — and Pumpkin's plugins are
//! WASM/native. It cannot load a Bukkit plugin or a Fabric mod, and no
//! configuration makes it. This follows from the engine, so it is read off
//! [`Engine`] directly.
//!
//! **Bedrock does not.** It would be convenient if it did, but
//! [`Engine::Spawned`] covers the desktop, which hosts Bedrock Dedicated
//! Server perfectly well, *and* Android, which ships a JRE and no BDS. Same
//! engine, different answer — so the host has to say, and [`Host::bedrock`]
//! is where it says it. Keying this off the engine would have quietly given
//! Android the desktop's answer.
//!
//! # What a refusal is for
//!
//! A player, not a log. [`Refusal::message`] is written to be shown as-is;
//! [`Refusal::code`] is for a host that wants to branch. The message never
//! names a platform, because this crate does not know which one it is running
//! on — it names the limit, which is what the player can act on anyway.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::loader_of;
use crate::launch::Engine;

/// What a host can run, beyond what its engine implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    /// Spawned or linked. A linked engine is vanilla-only.
    #[serde(default)]
    pub engine: Engine,
    /// This host can run a Bedrock server. False on both phones; true on the
    /// desktop, which has BDS.
    #[serde(default)]
    pub bedrock: bool,
}

impl Default for Host {
    fn default() -> Self {
        // The conservative host: a linked engine that cannot do Bedrock. A
        // caller that forgets a field gets refusals, not a launch it cannot
        // honour.
        Self {
            engine: Engine::Linked,
            bedrock: false,
        }
    }
}

/// The server being asked about, as the API described it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    /// The API's `game_type`, **verbatim** — `native-bedrock`, not `bedrock`.
    /// A host that reduces it before asking loses the distinction
    /// `native-crossplay` carries, which is the one that matters below.
    #[serde(default)]
    pub game_type: String,
    /// The server's settings as the API expressed them. Only `TYPE` is read,
    /// via [`loader_of`], so the whole object can be passed through.
    #[serde(default)]
    pub env: Value,
}

/// Why a launch was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refusal {
    /// Shown to the player as-is.
    pub message: String,
    /// Stable, for a host that wants to branch rather than display.
    pub code: &'static str,
}

impl Refusal {
    fn new(code: &'static str, message: &str) -> Self {
        Self {
            message: message.to_string(),
            code,
        }
    }
}

/// Whether `host` can run `server`, and if not, what to tell the player.
///
/// `None` means go ahead. Checked in order of how badly the launch would fail
/// without it: a Bedrock server on a Java host cannot start at all, while a
/// modded server on a linked engine starts *successfully* and is wrong.
pub fn refuse(host: Host, server: &Server) -> Option<Refusal> {
    if is_bedrock(&server.game_type) && !host.bedrock {
        return Some(Refusal::new(
            "bedrock-unsupported",
            "This is a Bedrock server, and this device can only host Java Edition.",
        ));
    }

    if host.engine == Engine::Linked {
        // Crossplay is Java plus Geyser — a plugin — so it fails the same way
        // a modpack does, and for the same reason. `loader_of` cannot see it:
        // the pack is not in `TYPE`, it is in the game type.
        if is_crossplay(&server.game_type) {
            return Some(Refusal::new(
                "crossplay-unsupported",
                "Crossplay needs a plugin, and this device can only host vanilla Minecraft.",
            ));
        }
        if loader_of(&server.env) != "vanilla" {
            return Some(Refusal::new(
                "mods-unsupported",
                "This server uses mods or plugins, and this device can only host vanilla Minecraft.",
            ));
        }
    }

    None
}

/// Both spellings the API uses. `bedrock` is the reduced form some hosts pass
/// on; `native-bedrock` is the verbatim one.
fn is_bedrock(game_type: &str) -> bool {
    matches!(game_type, "bedrock" | "native-bedrock")
}

fn is_crossplay(game_type: &str) -> bool {
    game_type == "native-crossplay"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const IOS: Host = Host {
        engine: Engine::Linked,
        bedrock: false,
    };
    const ANDROID: Host = Host {
        engine: Engine::Spawned,
        bedrock: false,
    };
    const DESKTOP: Host = Host {
        engine: Engine::Spawned,
        bedrock: true,
    };

    fn server(game_type: &str, env: Value) -> Server {
        Server {
            game_type: game_type.into(),
            env,
        }
    }

    fn vanilla(game_type: &str) -> Server {
        server(game_type, json!({}))
    }

    #[test]
    fn ios_runs_vanilla_java() {
        assert_eq!(refuse(IOS, &vanilla("native")), None);
        assert_eq!(refuse(IOS, &vanilla("minecraft/native")), None);
    }

    #[test]
    fn ios_refuses_mods() {
        let r = refuse(IOS, &server("native", json!({ "TYPE": "FORGE" }))).unwrap();
        assert_eq!(r.code, "mods-unsupported");
    }

    /// The failure that motivated the module: a modpack on a linked engine
    /// does not error, it silently starts vanilla.
    #[test]
    fn ios_refuses_plugins_too() {
        for kind in ["PAPER", "SPIGOT", "FABRIC", "forge"] {
            let r = refuse(IOS, &server("native", json!({ "TYPE": kind })));
            assert_eq!(
                r.map(|r| r.code),
                Some("mods-unsupported"),
                "{kind} should be refused on a linked engine",
            );
        }
    }

    /// Crossplay is vanilla by `TYPE` and still impossible: the plugin that
    /// makes it work is implied by the game type, not declared in the env.
    #[test]
    fn ios_refuses_crossplay_despite_vanilla_type() {
        assert_eq!(loader_of(&json!({})), "vanilla");
        let r = refuse(IOS, &vanilla("native-crossplay")).unwrap();
        assert_eq!(r.code, "crossplay-unsupported");
    }

    #[test]
    fn android_runs_mods_but_not_bedrock() {
        assert_eq!(
            refuse(ANDROID, &server("native", json!({ "TYPE": "FORGE" }))),
            None
        );
        assert_eq!(refuse(ANDROID, &vanilla("native-crossplay")), None);

        let r = refuse(ANDROID, &vanilla("native-bedrock")).unwrap();
        assert_eq!(r.code, "bedrock-unsupported");
    }

    /// Same engine as Android, opposite answer — which is why `bedrock` is an
    /// input rather than something read off `Engine`.
    #[test]
    fn desktop_runs_bedrock_on_the_same_engine() {
        assert_eq!(DESKTOP.engine, ANDROID.engine);
        assert_eq!(refuse(DESKTOP, &vanilla("native-bedrock")), None);
        assert_eq!(
            refuse(ANDROID, &vanilla("native-bedrock")).unwrap().code,
            "bedrock-unsupported"
        );
    }

    #[test]
    fn both_bedrock_spellings_are_caught() {
        assert_eq!(
            refuse(IOS, &vanilla("bedrock")).unwrap().code,
            "bedrock-unsupported"
        );
        assert_eq!(
            refuse(IOS, &vanilla("native-bedrock")).unwrap().code,
            "bedrock-unsupported"
        );
    }

    /// Bedrock is checked before mods: a Bedrock server has no Java loader, so
    /// reporting mods for it would be a lie the player cannot act on.
    #[test]
    fn bedrock_wins_over_mods() {
        let r = refuse(IOS, &server("native-bedrock", json!({ "TYPE": "FORGE" }))).unwrap();
        assert_eq!(r.code, "bedrock-unsupported");
    }

    /// The default is the host that can do least. A caller who forgets a field
    /// gets a refusal, never a launch the device cannot honour.
    #[test]
    fn default_host_is_the_conservative_one() {
        let h = Host::default();
        assert_eq!(h.engine, Engine::Linked);
        assert!(!h.bedrock);
        assert!(refuse(h, &server("native", json!({ "TYPE": "FABRIC" }))).is_some());
    }

    #[test]
    fn messages_are_written_for_a_player() {
        // No platform names, no jargon, no codes leaking into the text.
        for r in [
            refuse(IOS, &vanilla("native-bedrock")).unwrap(),
            refuse(IOS, &server("native", json!({ "TYPE": "FORGE" }))).unwrap(),
            refuse(IOS, &vanilla("native-crossplay")).unwrap(),
        ] {
            assert!(
                r.message.ends_with('.'),
                "{:?} should be a sentence",
                r.message
            );
            for banned in ["iOS", "Android", "Pumpkin", "engine", "null", "_"] {
                assert!(
                    !r.message.contains(banned),
                    "{:?} should not mention {banned:?}",
                    r.message,
                );
            }
        }
    }
}

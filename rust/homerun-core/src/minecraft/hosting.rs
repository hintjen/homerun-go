//! Which servers a host can actually run, and which of its engines runs them.
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
//! # A host has engines, it is not one
//!
//! This module used to ask whether a host *was* linked or spawned, and read
//! every limit off that one answer. That worked while each platform had a
//! single engine, and stopped working the moment Android had two: it runs a
//! real JVM for a modded server **and** Pumpkin for a Pumpkin one, and the
//! question "which engine is this device" has no answer there.
//!
//! So [`Host`] is a list of what a device can run, one flag per engine, and
//! [`serves`] answers which of them a given server lands on. Whether Pumpkin is
//! compiled into the app or spawned beside it is deliberately *not* here —
//! iOS links it because it cannot spawn anything, Android spawns it so a crash
//! cannot take the app down, and no rule in this module changes between those.
//!
//! # The limits, and why each is its own input
//!
//! **Pumpkin runs vanilla only.** It is a server written from scratch, and its
//! plugins are WASM or native. It cannot load a Bukkit plugin or a Fabric mod,
//! and no configuration makes it. This follows from the engine, so [`serves`]
//! reads it off the engine it picked rather than off the host.
//!
//! **Bedrock does not follow from any of it.** The desktop hosts Bedrock
//! Dedicated Server perfectly well and both phones ship no BDS at all, so the
//! host has to say, and [`Host::bedrock`] is where it says it.
//!
//! **Nor does Pumpkin.** The desktop could spawn a process like Android does
//! and still has no Pumpkin binary to spawn, so [`Host::pumpkin`] is a
//! declaration and not an inference. Keying it off "can this device spawn
//! things" would have quietly offered the desktop a server it cannot start.
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

/// What a host can run. One flag per engine, because a device may have more
/// than one and Android does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
// Every field is optional and one of them is spelled the old way, so the
// incoming shape is [`HostWire`] and this struct is what it resolves to.
#[serde(from = "HostWire")]
pub struct Host {
    /// This device can run a Java server — it has a JVM and can start one.
    /// False on iOS, which cannot spawn a process at all.
    pub jvm: bool,
    /// This device can run Pumpkin, linked into the app or spawned beside it.
    /// True on both phones, false on the desktop, which ships no Pumpkin.
    pub pumpkin: bool,
    /// This device can run a Bedrock server. False on both phones; true on the
    /// desktop, which has BDS.
    pub bedrock: bool,
}

impl Default for Host {
    fn default() -> Self {
        // The conservative host: Pumpkin and nothing else, which is iOS. A
        // caller that forgets a field gets refusals, not a launch it cannot
        // honour.
        Self {
            jvm: false,
            pumpkin: true,
            bedrock: false,
        }
    }
}

/// The wire shape, which still accepts the field this struct used to have.
///
/// Hosts sent `{"engine":"spawned"|"linked"}` and meant "this is the one engine
/// I have". Both hosts in this repo rebuild together so nothing in-tree needs
/// the shim — but `host` is absent-means-conservative on purpose, and a struct
/// that silently defaulted every field because it did not recognise the key it
/// was sent is precisely the failure this module was written about.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostWire {
    jvm: Option<bool>,
    pumpkin: Option<bool>,
    #[serde(default)]
    engine: Option<Engine>,
    #[serde(default)]
    bedrock: bool,
}

impl From<HostWire> for Host {
    fn from(wire: HostWire) -> Self {
        // A caller naming either new field is speaking the new language, and
        // its `engine` — if it sent one at all — is a deployment detail this
        // struct no longer holds.
        if wire.jvm.is_some() || wire.pumpkin.is_some() {
            return Host {
                jvm: wire.jvm.unwrap_or(false),
                pumpkin: wire.pumpkin.unwrap_or(false),
                bedrock: wire.bedrock,
            };
        }
        match wire.engine {
            // "I spawn my engine" meant a JVM, every time it was sent.
            Some(Engine::Spawned) => Host {
                jvm: true,
                pumpkin: false,
                bedrock: wire.bedrock,
            },
            // "I link my engine" meant Pumpkin, every time it was sent.
            Some(Engine::Linked) => Host {
                jvm: false,
                pumpkin: true,
                bedrock: wire.bedrock,
            },
            None => Host {
                bedrock: wire.bedrock,
                ..Host::default()
            },
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

/// Which engine a server runs on.
///
/// The host's routing answer: one of these maps to one `ServerBackend`. It is
/// deliberately not [`Engine`] — that says *how* an engine is deployed, which
/// differs per platform for the same value here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Served {
    /// A real server jar on a real JVM.
    Jvm,
    /// Pumpkin — linked on iOS, a child process on Android.
    Pumpkin,
    /// Bedrock Dedicated Server.
    Bedrock,
}

impl Served {
    pub fn as_str(self) -> &'static str {
        match self {
            Served::Jvm => "jvm",
            Served::Pumpkin => "pumpkin",
            Served::Bedrock => "bedrock",
        }
    }
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

/// Which of `host`'s engines runs `server`, or why none of them can.
///
/// Checked in order of how badly the launch would fail without it: a Bedrock
/// server on a Java host cannot start at all, while a modded server on Pumpkin
/// starts *successfully* and is wrong.
pub fn serves(host: Host, server: &Server) -> Result<Served, Refusal> {
    let game_type = server.game_type.as_str();

    if is_bedrock(game_type) {
        return if host.bedrock {
            Ok(Served::Bedrock)
        } else {
            Err(Refusal::new(
                "bedrock-unsupported",
                "This is a Bedrock server, and this device can only host Java Edition.",
            ))
        };
    }

    let engine = if is_pumpkin(game_type) {
        // Asked for by name, so there is no substituting a JVM for it: the
        // player picked this server software and a different one is a
        // different server.
        if !host.pumpkin {
            return Err(Refusal::new(
                "engine-unavailable",
                "This device can't host this kind of server.",
            ));
        }
        Served::Pumpkin
    } else if host.jvm {
        // A Java server, and a real JVM is always the better answer for one:
        // it is the only engine that can run the mods and plugins.
        Served::Jvm
    } else if host.pumpkin {
        // No JVM, so a plain Java server falls to Pumpkin. **This is iOS**,
        // where every server ever created is `native` with no `TYPE` and has
        // always been served this way. Removing this arm stops every existing
        // iOS server from launching.
        Served::Pumpkin
    } else {
        return Err(Refusal::new(
            "engine-unavailable",
            "This device can't host this kind of server.",
        ));
    };

    if engine == Served::Pumpkin {
        // Crossplay is Java plus Geyser — a plugin — so it fails the same way
        // a modpack does, and for the same reason. `loader_of` cannot see it:
        // the pack is not in `TYPE`, it is in the game type.
        if is_crossplay(game_type) {
            return Err(Refusal::new(
                "crossplay-unsupported",
                "Crossplay needs a plugin, and this device can only host vanilla Minecraft.",
            ));
        }
        if loader_of(&server.env) != "vanilla" {
            return Err(Refusal::new(
                "mods-unsupported",
                "This server uses mods or plugins, and this device can only host vanilla Minecraft.",
            ));
        }
    }

    Ok(engine)
}

/// Whether `host` can run `server`, and if not, what to tell the player.
///
/// `None` means go ahead. The other half of [`serves`], kept because most
/// callers only want the veto.
pub fn refuse(host: Host, server: &Server) -> Option<Refusal> {
    serves(host, server).err()
}

/// Whether this kind of server runs on a JVM, and so needs a runtime, a jar
/// and a main class fetched before it can start.
///
/// Pumpkin is the server, so there is nothing to fetch; Bedrock is its own
/// binary. Used by [`crate::launch::plan`], which until now inferred this from
/// whether the engine was linked — true of Pumpkin-on-iOS by accident and
/// wrong the moment Pumpkin is spawned instead.
pub fn needs_jvm(game_type: &str) -> bool {
    !is_pumpkin(game_type) && !is_bedrock(game_type)
}

/// Both spellings the API uses. `bedrock` is the reduced form some hosts pass
/// on; `native-bedrock` is the verbatim one.
fn is_bedrock(game_type: &str) -> bool {
    matches!(game_type, "bedrock" | "native-bedrock")
}

/// Both spellings, for the same reason as [`is_bedrock`].
///
/// Pumpkin is named by the game type rather than by `TYPE` because it is a
/// different server, not a loader on top of one — the same reason Bedrock is.
/// That also makes it immutable in the places that matter: nothing offers to
/// change a server's game type after it is made, and a world written by one
/// engine must not be opened by the other.
pub fn is_pumpkin(game_type: &str) -> bool {
    matches!(game_type, "pumpkin" | "native-pumpkin")
}

fn is_crossplay(game_type: &str) -> bool {
    game_type == "native-crossplay"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const IOS: Host = Host {
        jvm: false,
        pumpkin: true,
        bedrock: false,
    };
    const ANDROID: Host = Host {
        jvm: true,
        pumpkin: true,
        bedrock: false,
    };
    const DESKTOP: Host = Host {
        jvm: true,
        pumpkin: false,
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

    /// Every server that exists on iOS is `native` with no `TYPE`, and iOS has
    /// no JVM — so the fallback to Pumpkin for a plain Java server is what
    /// keeps them all launching. Delete that arm in [`serves`] and this is the
    /// test that says so, rather than a support ticket.
    #[test]
    fn ios_serves_a_plain_java_server_with_pumpkin() {
        assert_eq!(serves(IOS, &vanilla("native")), Ok(Served::Pumpkin));
        assert_eq!(
            serves(IOS, &vanilla("minecraft/native")),
            Ok(Served::Pumpkin)
        );
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
                "{kind} should be refused where only Pumpkin can serve it",
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

    /// The routing answer, which is the whole point of the reshape: one device,
    /// two engines, and the game type decides.
    #[test]
    fn android_routes_by_game_type() {
        assert_eq!(
            serves(ANDROID, &server("native", json!({ "TYPE": "PAPER" }))),
            Ok(Served::Jvm)
        );
        assert_eq!(serves(ANDROID, &vanilla("native")), Ok(Served::Jvm));
        assert_eq!(
            serves(ANDROID, &vanilla("native-pumpkin")),
            Ok(Served::Pumpkin)
        );
    }

    /// A JVM is the better engine for a Java server and is still not a
    /// substitute for the one that was asked for by name.
    #[test]
    fn a_pumpkin_server_is_never_served_by_the_jvm() {
        for host in [ANDROID, IOS] {
            assert_eq!(
                serves(host, &vanilla("native-pumpkin")),
                Ok(Served::Pumpkin)
            );
        }
    }

    /// Spawning is not the question — having a Pumpkin to spawn is. The
    /// desktop can start child processes all day and ships no Pumpkin binary.
    #[test]
    fn a_desktop_refuses_pumpkin() {
        let r = refuse(DESKTOP, &vanilla("native-pumpkin")).unwrap();
        assert_eq!(r.code, "engine-unavailable");
        assert!(DESKTOP.jvm, "and not because it cannot spawn anything");
    }

    /// `loader_of` collapses every non-empty `TYPE` to "modded", so a Pumpkin
    /// server must not be identified that way — it carries no `TYPE` at all.
    #[test]
    fn a_pumpkin_server_is_not_mistaken_for_a_modded_one() {
        assert_eq!(loader_of(&json!({})), "vanilla");
        assert_eq!(refuse(ANDROID, &vanilla("native-pumpkin")), None);
        assert_eq!(refuse(ANDROID, &vanilla("pumpkin")), None);
    }

    /// Pumpkin cannot load a mod whether it is linked or spawned, so the
    /// refusal has to follow the engine that was picked and not the way the
    /// device happens to deploy it.
    #[test]
    fn a_pumpkin_server_still_refuses_mods_on_a_host_with_a_jvm() {
        let r = refuse(
            ANDROID,
            &server("native-pumpkin", json!({ "TYPE": "FABRIC" })),
        )
        .unwrap();
        assert_eq!(r.code, "mods-unsupported");

        let r = refuse(ANDROID, &vanilla("native-crossplay")).map(|r| r.code);
        assert_eq!(r, None, "crossplay on Android is a JVM server and is fine");
    }

    /// Same JVM as Android, opposite answer — which is why `bedrock` is an
    /// input rather than something inferred.
    #[test]
    fn desktop_runs_bedrock_on_the_same_jvm() {
        assert_eq!(DESKTOP.jvm, ANDROID.jvm);
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
        assert!(!h.jvm);
        assert!(h.pumpkin);
        assert!(!h.bedrock);
        assert!(refuse(h, &server("native", json!({ "TYPE": "FABRIC" }))).is_some());
    }

    /// The field this struct used to have. A host that has not been rebuilt
    /// keeps the answers it always got, rather than defaulting every flag to
    /// false and being told it can host nothing.
    #[test]
    fn the_engine_key_still_means_what_it_meant() {
        let linked: Host = serde_json::from_value(json!({ "engine": "linked" })).unwrap();
        assert_eq!(linked, IOS);

        let spawned: Host = serde_json::from_value(json!({ "engine": "spawned" })).unwrap();
        assert!(spawned.jvm && !spawned.pumpkin);

        let desktop: Host =
            serde_json::from_value(json!({ "engine": "spawned", "bedrock": true })).unwrap();
        assert_eq!(desktop, DESKTOP);

        // And the new spelling wins outright when both are somehow present.
        let both: Host =
            serde_json::from_value(json!({ "engine": "linked", "jvm": true, "pumpkin": true }))
                .unwrap();
        assert_eq!(both, ANDROID);
    }

    #[test]
    fn an_absent_host_is_the_conservative_one() {
        let empty: Host = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, Host::default());
    }

    /// Pumpkin and Bedrock are the server; there is no jar to fetch for either.
    #[test]
    fn only_a_java_server_needs_a_jvm() {
        assert!(needs_jvm("native"));
        assert!(needs_jvm("minecraft/native"));
        assert!(needs_jvm("native-crossplay"));
        assert!(!needs_jvm("native-pumpkin"));
        assert!(!needs_jvm("pumpkin"));
        assert!(!needs_jvm("native-bedrock"));
    }

    #[test]
    fn messages_are_written_for_a_player() {
        // No platform names, no jargon, no codes leaking into the text.
        for r in [
            refuse(IOS, &vanilla("native-bedrock")).unwrap(),
            refuse(IOS, &server("native", json!({ "TYPE": "FORGE" }))).unwrap(),
            refuse(IOS, &vanilla("native-crossplay")).unwrap(),
            refuse(DESKTOP, &vanilla("native-pumpkin")).unwrap(),
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

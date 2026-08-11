//! What the API's settings mean to a **linked** engine.
//!
//! # Why this is not the engine's business
//!
//! A host that spawns a server writes files; a host that links one assigns
//! struct fields. Those are different mechanics for the same decisions, and
//! the decisions belong to [`homerun_core::minecraft::settings`] either way —
//! which env key wins, what a missing value defaults to, whether a server is
//! online, who is an operator.
//!
//! What is left over is genuinely engine-shaped: an engine accepts a narrower
//! range than the API does. A view distance of 200 is a number the API will
//! happily carry and Pumpkin asserts on. Clamping that is not a decision about
//! what a player asked for, it is a fact about what the engine will take.
//!
//! # Why it is a plain struct rather than `pumpkin-config` types
//!
//! This crate's tests run in seconds with no Pumpkin, on any machine, and that
//! is deliberate — see [`crate::engine`]. Resolve into an ordinary struct and
//! every clamp, every fallback and every UUID rule is testable in the fast
//! suite; resolve straight into the engine's types and none of them are.
//!
//! What is left for the gated half is assignment, and assignment is the part
//! that cannot be got subtly wrong without a test noticing.

use homerun_core::game::Identity;
use homerun_core::minecraft::{self, settings};
use serde::Serialize;
use serde_json::Value;

/// The narrowest and widest a Minecraft engine will accept.
///
/// Pumpkin asserts this range in `PumpkinConfig::validate`, and we bypass that
/// by overriding after `load()`, so the clamp is ours to apply.
const DISTANCE_RANGE: (u32, u32) = (2, 64);

/// Every setting a player can choose that a linked engine cannot honour,
/// keyed by the environment variable that carries it.
///
/// Reported per launch rather than silently dropped: "my server ignored the
/// difficulty I picked" is a support conversation, and one console line ends
/// it. Presence is what matters, not value — `from_env` defaults everything,
/// so by the time it has run there is no way to tell "unset" from "the
/// default".
const UNSUPPORTED: &[(&str, &str)] = &[
    ("DIFFICULTY", "difficulty"),
    ("LEVEL_TYPE", "world type"),
    ("GENERATE_STRUCTURES", "generate structures"),
    ("SPAWN_NPCS", "spawn villagers"),
    ("SPAWN_ANIMALS", "spawn animals"),
    ("SPAWN_MONSTERS", "spawn monsters"),
    ("ENABLE_COMMAND_BLOCK", "command blocks"),
    ("ALLOW_CHEATS", "cheats"),
];

/// The four a Minecraft server understands.
const GAME_MODES: &[&str] = &["survival", "creative", "adventure", "spectator"];

/// What a linked engine should be told, with every value already inside the
/// range it will accept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSettings {
    pub motd: String,
    pub max_players: u32,
    /// Guaranteed within [`DISTANCE_RANGE`].
    pub view_distance: u8,
    /// Guaranteed within [`DISTANCE_RANGE`].
    pub simulation_distance: u8,
    pub online_mode: bool,
    pub pvp: bool,
    pub hardcore: bool,
    /// One of [`GAME_MODES`], lowercase.
    pub game_mode: String,
    pub white_list: bool,
    /// `None` leaves whatever the engine already recorded.
    ///
    /// Set only when the player actually chose a seed: an engine that mints a
    /// random one for an empty string would give a regenerated world a
    /// different seed on every launch.
    pub seed: Option<String>,
    pub ops: Vec<settings::Player>,
    pub whitelisted: Vec<settings::Player>,
    pub banned: Vec<settings::Player>,
    /// Names the host could not resolve, so they were left out.
    pub unresolved: Vec<String>,
    /// Settings this engine cannot honour, phrased for a player.
    pub unsupported: Vec<String>,
}

/// Resolve the API's environment into what a linked engine should be told.
///
/// `resolved` is what the host got back from the player directory it asked —
/// Mojang, for Minecraft. Empty is normal: an offline server needs no lookups
/// at all, and the core derives those ids itself.
pub fn resolve(env: &Value, game_type: &str, resolved: &[Identity]) -> EngineSettings {
    let loader = minecraft::loader_of(env);
    let chosen = settings::from_env(env, game_type, loader, None);

    let (low, high) = DISTANCE_RANGE;
    let mut unsupported: Vec<String> = UNSUPPORTED
        .iter()
        .filter(|(key, _)| present(env, key))
        .map(|(_, label)| (*label).to_string())
        .collect();

    // A game mode this engine does not know is worth saying out loud rather
    // than quietly playing survival — some panels send "hardcore" here, which
    // is a separate flag rather than a mode.
    let game_mode = chosen.game_mode.to_ascii_lowercase();
    let game_mode = if GAME_MODES.contains(&game_mode.as_str()) {
        game_mode
    } else {
        unsupported.push(format!("game mode \"{}\"", chosen.game_mode));
        "survival".to_string()
    };

    let players = |names: &[String]| settings::resolve_players(names, resolved, chosen.online_mode);
    let ops = players(&chosen.op_users);
    let whitelisted = players(&chosen.whitelisted_users);
    let banned = players(&chosen.banned_users);

    EngineSettings {
        motd: chosen.motd.clone(),
        max_players: chosen.max_players,
        view_distance: chosen.view_distance.clamp(low, high) as u8,
        simulation_distance: chosen.simulation_distance.clamp(low, high) as u8,
        online_mode: chosen.online_mode,
        pvp: chosen.pvp,
        hardcore: chosen.hardcore,
        game_mode,
        white_list: chosen.whitelist_enabled,
        seed: Some(chosen.seed.clone()).filter(|s| !s.trim().is_empty()),
        unresolved: dropped(&chosen, &ops, &whitelisted, &banned),
        ops,
        whitelisted,
        banned,
        unsupported,
    }
}

impl EngineSettings {
    /// One line for the console, so a wrong mapping is a two-second diagnosis
    /// rather than a two-hour one. Every bug report carries the console.
    pub fn summary(&self) -> String {
        format!(
            "[Homerun] Settings applied: {}, {} players, view {}, {} mode, {} operator{}.",
            self.game_mode,
            self.max_players,
            self.view_distance,
            if self.online_mode { "online" } else { "offline" },
            self.ops.len(),
            if self.ops.len() == 1 { "" } else { "s" },
        )
    }

    /// The advisories worth telling a player, if any.
    pub fn advisories(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.unresolved.is_empty() {
            lines.push(format!(
                "[Homerun] Could not look up {} — left out of this server's lists.",
                self.unresolved.join(", ")
            ));
        }
        if !self.unsupported.is_empty() {
            lines.push(format!(
                "[Homerun] Not supported by this server yet, so ignored: {}.",
                self.unsupported.join(", ")
            ));
        }
        lines
    }
}

/// A key the API actually sent, as opposed to one `from_env` defaulted.
fn present(env: &Value, key: &str) -> bool {
    env.get(key)
        .map(|v| !matches!(v, Value::Null) && v.as_str() != Some(""))
        .unwrap_or(false)
}

/// Names asked for that no player came back for.
fn dropped(
    chosen: &settings::Settings,
    ops: &[settings::Player],
    whitelisted: &[settings::Player],
    banned: &[settings::Player],
) -> Vec<String> {
    let kept: Vec<&str> = ops
        .iter()
        .chain(whitelisted)
        .chain(banned)
        .map(|p| p.name.as_str())
        .collect();

    let mut missing: Vec<String> = chosen
        .op_users
        .iter()
        .chain(&chosen.whitelisted_users)
        .chain(&chosen.banned_users)
        .filter(|name| !kept.contains(&name.as_str()))
        .cloned()
        .collect();
    missing.dedup();
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolve_env(pairs: Value) -> EngineSettings {
        resolve(&pairs, "java", &[])
    }

    #[test]
    fn a_view_distance_the_engine_would_reject_is_clamped() {
        // Pumpkin asserts 2..=64 and we bypass its validate(), so an
        // unclamped value is a panic on a device rather than an error.
        assert_eq!(resolve_env(json!({ "VIEW_DISTANCE": "0" })).view_distance, 2);
        assert_eq!(resolve_env(json!({ "VIEW_DISTANCE": "1" })).view_distance, 2);
        assert_eq!(
            resolve_env(json!({ "VIEW_DISTANCE": "200" })).view_distance,
            64
        );
        assert_eq!(
            resolve_env(json!({ "SIMULATION_DISTANCE": "999" })).simulation_distance,
            64
        );
    }

    /// The core caps at 15 for `server.properties`, guarding 1.2.5-era servers
    /// that hard-crash above it. That is a file-format concern, and applying
    /// it here would silently give a player 15 when they asked for 24.
    #[test]
    fn a_reasonable_view_distance_is_left_alone() {
        assert_eq!(
            resolve_env(json!({ "VIEW_DISTANCE": "24" })).view_distance,
            24
        );
    }

    #[test]
    fn an_unknown_game_mode_falls_back_and_says_so() {
        let resolved = resolve_env(json!({ "GAMEMODE": "hardcore" }));
        assert_eq!(resolved.game_mode, "survival");
        assert!(
            resolved.unsupported.iter().any(|u| u.contains("hardcore")),
            "{:?}",
            resolved.unsupported
        );
    }

    #[test]
    fn the_four_real_modes_pass_through() {
        for mode in GAME_MODES {
            let resolved = resolve_env(json!({ "GAMEMODE": mode }));
            assert_eq!(&resolved.game_mode, mode);
            assert!(resolved.unsupported.is_empty(), "{mode}");
        }
    }

    /// A crossplay vanilla server authenticates against nobody, because Geyser
    /// clients have no Mojang account. The core decides this; we must not
    /// re-derive it.
    #[test]
    fn crossplay_vanilla_is_offline_without_being_told() {
        assert!(!resolve(&json!({}), "native-crossplay", &[]).online_mode);
        assert!(resolve(&json!({}), "java", &[]).online_mode);
    }

    #[test]
    fn online_mode_false_is_honoured() {
        assert!(!resolve_env(json!({ "ONLINE_MODE": "false" })).online_mode);
    }

    #[test]
    fn an_offline_server_ops_everyone_without_a_lookup() {
        let resolved = resolve_env(json!({ "ONLINE_MODE": "false", "OPS": "Notch,jeb_" }));
        assert_eq!(resolved.ops.len(), 2);
        assert!(resolved.unresolved.is_empty());
    }

    #[test]
    fn an_online_server_drops_a_name_it_could_not_resolve() {
        let resolved = resolve(&json!({ "OPS": "Notch,Ghost" }), "java", &[Identity {
            name: "Notch".into(),
            id: "069a79f4-44e9-4726-a5be-fca90e38aaf5".into(),
        }]);

        assert_eq!(resolved.ops.len(), 1);
        assert_eq!(resolved.ops[0].name, "Notch");
        assert_eq!(resolved.unresolved, vec!["Ghost".to_string()]);
    }

    #[test]
    fn an_empty_seed_is_none_rather_than_a_new_random_one() {
        // Pumpkin's `Seed::from("")` mints a fresh seed on every call, so
        // assigning unconditionally gives a regenerated world a different
        // world every launch.
        assert_eq!(resolve_env(json!({})).seed, None);
        assert_eq!(resolve_env(json!({ "LEVEL_SEED": "   " })).seed, None);
        assert_eq!(
            resolve_env(json!({ "LEVEL_SEED": "12345" })).seed,
            Some("12345".to_string())
        );
    }

    #[test]
    fn only_settings_the_api_actually_sent_are_called_unsupported() {
        assert!(resolve_env(json!({})).unsupported.is_empty());

        let resolved = resolve_env(json!({ "DIFFICULTY": "hard", "LEVEL_TYPE": "flat" }));
        assert_eq!(resolved.unsupported, vec!["difficulty", "world type"]);
    }

    #[test]
    fn the_summary_names_what_was_applied() {
        let line = resolve_env(json!({ "GAMEMODE": "creative", "MAX_PLAYERS": "8" })).summary();
        assert!(line.contains("creative"), "{line}");
        assert!(line.contains("8 players"), "{line}");
        assert!(line.contains("online mode"), "{line}");
    }

    #[test]
    fn one_operator_is_not_pluralised() {
        let line = resolve_env(json!({ "ONLINE_MODE": "false", "OPS": "Notch" })).summary();
        assert!(line.contains("1 operator."), "{line}");
    }
}

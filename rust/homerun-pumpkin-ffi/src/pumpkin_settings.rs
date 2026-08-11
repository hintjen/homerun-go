//! Assigning a launch's settings onto Pumpkin's own types.
//!
//! The decisions are already made by the time anything here runs — see
//! [`crate::engine_settings`], which is where clamps, fallbacks and UUID rules
//! live precisely so they can be tested without a Pumpkin build. What is left
//! is assignment, and the invariants that come with bypassing the engine's own
//! validation.
//!
//! # Why override a loaded config rather than build one
//!
//! `PumpkinConfig::load` is what writes `pumpkin.toml` on first run, and that
//! file carries roughly two hundred settings this app does not manage. Building
//! a config from scratch would put every one of them out of a player's reach
//! for good. It also matters that a **restore** can bring another device's
//! `pumpkin.toml` into this server's directory: loading it and then overriding
//! the managed set is what makes that harmless.
//!
//! The same policy `homerun-core`'s `properties::merge` already gives the
//! desktop and Android — the API owns the settings it knows about, the file
//! owns the rest.
//!
//! # The invariants we inherit
//!
//! Overriding after `load()` means `PumpkinConfig::validate` has already run,
//! over the file's values rather than ours. Its assertions are `assert!`, so
//! breaking one is an abort on a phone. They are therefore ours to hold:
//!
//! - a distance outside `2..=64` (held in `engine_settings`, asserted here),
//! - `online_mode` without `encryption`,
//! - `allow_chat_reports` without `online_mode`.
//!
//! The gated tests call `validate()` after `apply` over a hostile matrix, so
//! Pumpkin's own assertions are the oracle — an assertion added upstream
//! breaks a test here rather than a server on someone's phone.

use std::num::NonZeroU8;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::str::FromStr;

use pumpkin::data::banlist_serializer::BannedPlayerEntry;
use pumpkin::data::VanillaData;
use pumpkin_config::op::Op;
use pumpkin_config::whitelist::WhitelistEntry;
use pumpkin_config::{LoadConfiguration, PumpkinConfig};
use pumpkin_util::world_seed::Seed;
use pumpkin_util::GameMode;
use uuid::Uuid;

use crate::engine_settings::EngineSettings;
use homerun_core::minecraft::settings::Player;

/// Load Pumpkin's config, surviving a file it cannot parse.
///
/// `load()` panics on malformed TOML and on a `validate()` assertion, which
/// without this is an unstartable server explained by a file the player cannot
/// see. Falling back to defaults costs them their unmanaged settings for one
/// launch; panicking costs them the app.
pub fn load_config(exec_dir: &Path, warn: &dyn Fn(String)) -> PumpkinConfig {
    catch_unwind(AssertUnwindSafe(|| PumpkinConfig::load(exec_dir))).unwrap_or_else(|_| {
        warn(
            "[Homerun] This server's configuration file could not be read, so default settings \
             are being used. Your own settings still apply."
                .to_string(),
        );
        PumpkinConfig::default()
    })
}

/// Load the operator, whitelist and ban lists, surviving a file it cannot
/// parse. Same reasoning as [`load_config`]; these are JSON and panic the same
/// way.
pub fn load_data(warn: &dyn Fn(String)) -> VanillaData {
    catch_unwind(AssertUnwindSafe(VanillaData::load)).unwrap_or_else(|_| {
        warn(
            "[Homerun] This server's player lists could not be read, so they are being rebuilt \
             from your settings."
                .to_string(),
        );
        VanillaData {
            banned_ip_list: Default::default(),
            banned_player_list: Default::default(),
            operator_config: Default::default(),
            user_cache: Default::default(),
            whitelist_config: Default::default(),
        }
    })
}

/// Override the settings this app manages, leaving the rest of the file alone.
pub fn apply(settings: &EngineSettings, config: &mut PumpkinConfig) {
    let basic = &mut config.basic;
    basic.hardcore = settings.hardcore;
    basic.white_list = settings.white_list;
    basic.default_gamemode =
        GameMode::from_str(&settings.game_mode).unwrap_or(basic.default_gamemode);

    // Only when the player actually chose one: `Seed::from("")` mints a fresh
    // random seed, so assigning unconditionally would give a regenerated world
    // a different world on every launch. It is read only when a level is
    // created; an existing world keeps the seed in its own `level.dat`.
    if let Some(seed) = &settings.seed {
        basic.seed = Seed::from(seed.as_str());
    }

    config.advanced.pvp.enabled = settings.pvp;

    // The two listeners are one server. Leaving Bedrock on its own defaults
    // would give a Bedrock client a different player cap and a different MOTD
    // from the Java client next to it on the same Wi-Fi.
    for (online, encryption, max_players, view, simulation, motd) in [
        (
            &mut config.advanced.networking.java.online_mode,
            &mut config.advanced.networking.java.encryption,
            &mut config.advanced.networking.java.max_players,
            &mut config.advanced.networking.java.view_distance,
            &mut config.advanced.networking.java.simulation_distance,
            &mut config.advanced.networking.java.motd,
        ),
        (
            &mut config.advanced.networking.bedrock.online_mode,
            &mut config.advanced.networking.bedrock.encryption,
            &mut config.advanced.networking.bedrock.max_players,
            &mut config.advanced.networking.bedrock.view_distance,
            &mut config.advanced.networking.bedrock.simulation_distance,
            &mut config.advanced.networking.bedrock.motd,
        ),
    ] {
        *online = settings.online_mode;
        // Pumpkin asserts this pairing, and we bypass the check that would
        // have caught it.
        if settings.online_mode {
            *encryption = true;
        }
        *max_players = settings.max_players;
        *view = distance(settings.view_distance);
        *simulation = distance(settings.simulation_distance);
        motd.clone_from(&settings.motd);
    }

    // Also asserted, and reachable the moment a player turns off online mode
    // on a server whose file has reports enabled — which is a config Pumpkin
    // itself would have refused to load.
    if !settings.online_mode {
        basic.allow_chat_reports = false;
    }
}

/// Seed the player lists a launch is responsible for.
///
/// Operators and the whitelist are replaced **wholesale**: a de-opped player
/// has to stop being an operator on the next start, and merging cannot express
/// a removal.
///
/// Bans are **appended**. `/ban` in game writes the same file, and rewriting it
/// from the API would quietly un-ban everyone a moderator banned on the device.
pub fn apply_lists(settings: &EngineSettings, config: &PumpkinConfig, data: &mut VanillaData) {
    let level = config.basic.op_permission_level;

    data.operator_config.get_mut().ops = settings
        .ops
        .iter()
        .filter_map(|player| {
            Some(Op::new(
                uuid_of(player)?,
                player.name.clone(),
                level,
                // Vanilla's default, and the safer one: an operator who
                // bypasses the cap can fill a phone's server past what it was
                // told it could hold.
                false,
            ))
        })
        .collect();

    data.whitelist_config.get_mut().whitelist = settings
        .whitelisted
        .iter()
        .filter_map(|player| Some(WhitelistEntry::new(uuid_of(player)?, player.name.clone())))
        .collect();

    let bans = data.banned_player_list.get_mut();
    for player in &settings.banned {
        let Some(uuid) = uuid_of(player) else { continue };
        if bans.banned_players.iter().any(|entry| entry.uuid == uuid) {
            continue;
        }
        // Built here rather than through `BannedPlayerEntry::new`, which wants
        // a connected player's `GameProfile`, and rather than through core's
        // `merge_banned`, which writes a `+0000` offset where Pumpkin's
        // deserializer requires `+00:00` — that mismatch would not fail this
        // launch, it would panic the *next* one.
        bans.banned_players.push(BannedPlayerEntry {
            uuid,
            name: player.name.clone(),
            created: time::OffsetDateTime::now_utc(),
            source: "Homerun".to_string(),
            expires: None,
            reason: "Banned by an operator.".to_string(),
        });
    }
}

/// A distance Pumpkin will accept. `engine_settings` clamps to `2..=64`; this
/// is the assertion that the clamp is still there, in the one place where
/// being wrong is an abort rather than a test failure.
fn distance(value: u8) -> NonZeroU8 {
    NonZeroU8::new(value.clamp(2, 64)).expect("clamped to at least 2")
}

/// A player's UUID, or nothing.
///
/// Nothing is a real outcome rather than an error: an online-mode launch whose
/// directory lookup failed has a name and no id, and dropping that entry is
/// what `engine_settings` already decided. Failing the launch over an operator
/// who could not be resolved would trade a missing permission for no server.
fn uuid_of(player: &Player) -> Option<Uuid> {
    Uuid::parse_str(&player.uuid).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(env: serde_json::Value) -> EngineSettings {
        crate::engine_settings::resolve(&env, "java", &[])
    }

    fn data() -> VanillaData {
        VanillaData {
            banned_ip_list: Default::default(),
            banned_player_list: Default::default(),
            operator_config: Default::default(),
            user_cache: Default::default(),
            whitelist_config: Default::default(),
        }
    }

    /// Distinct values per field on purpose: equal ones are exactly what lets
    /// two fields be swapped without a test noticing.
    #[test]
    fn every_managed_field_lands_where_it_was_meant_to() {
        let mut config = PumpkinConfig::default();
        apply(
            &settings(json!({
                "MOTD": "a homerun server",
                "MAX_PLAYERS": "33",
                "VIEW_DISTANCE": "7",
                "SIMULATION_DISTANCE": "11",
                "GAMEMODE": "adventure",
                "HARDCORE": "true",
                "PVP": "false",
                "ENABLE_WHITELIST": "true",
                "ONLINE_MODE": "false",
            })),
            &mut config,
        );

        let java = &config.advanced.networking.java;
        assert_eq!(java.motd, "a homerun server");
        assert_eq!(java.max_players, 33);
        assert_eq!(java.view_distance.get(), 7);
        assert_eq!(java.simulation_distance.get(), 11);
        assert!(!java.online_mode);

        assert_eq!(config.basic.default_gamemode, GameMode::Adventure);
        assert!(config.basic.hardcore);
        assert!(config.basic.white_list);
        assert!(!config.advanced.pvp.enabled);

        config.validate();
    }

    /// The whole reason to override a loaded config rather than build one.
    #[test]
    fn a_setting_this_app_does_not_manage_survives() {
        let mut config = PumpkinConfig::default();
        config.basic.allow_nether = false;
        config.advanced.networking.java.compression.info.threshold = 512;

        apply(&settings(json!({ "MOTD": "hi" })), &mut config);

        assert!(!config.basic.allow_nether);
        assert_eq!(config.advanced.networking.java.compression.info.threshold, 512);
    }

    /// Pumpkin asserts these, and `validate()` has already run by the time we
    /// assign — so a violation is an abort on a phone rather than a refusal.
    #[test]
    fn pumpkins_own_assertions_hold_over_a_hostile_matrix() {
        for (env, game_type) in [
            (json!({ "ONLINE_MODE": "true" }), "java"),
            (json!({ "ONLINE_MODE": "false" }), "java"),
            (json!({ "VIEW_DISTANCE": "0" }), "java"),
            (json!({ "VIEW_DISTANCE": "500" }), "java"),
            (json!({ "SIMULATION_DISTANCE": "1" }), "java"),
            (json!({}), "native-crossplay"),
        ] {
            for encryption in [true, false] {
                for reports in [true, false] {
                    let mut config = PumpkinConfig::default();
                    config.advanced.networking.java.encryption = encryption;
                    config.advanced.networking.bedrock.encryption = encryption;
                    config.basic.allow_chat_reports = reports;

                    apply(
                        &crate::engine_settings::resolve(&env, game_type, &[]),
                        &mut config,
                    );

                    // Panics on failure, which is the point: Pumpkin's own
                    // assertions are the oracle.
                    config.validate();
                }
            }
        }
    }

    #[test]
    fn an_empty_seed_leaves_the_worlds_own_alone() {
        let mut config = PumpkinConfig::default();
        let before = config.basic.seed.0;
        apply(&settings(json!({})), &mut config);
        assert_eq!(config.basic.seed.0, before);

        apply(&settings(json!({ "LEVEL_SEED": "12345" })), &mut config);
        assert_eq!(config.basic.seed.0, 12345);
    }

    #[test]
    fn operators_and_the_whitelist_are_replaced_not_merged() {
        let mut data = data();
        data.operator_config.get_mut().ops = vec![Op::new(
            Uuid::from_u128(1),
            "WasAnOp".into(),
            Default::default(),
            false,
        )];
        data.whitelist_config.get_mut().whitelist =
            vec![WhitelistEntry::new(Uuid::from_u128(2), "WasAllowed".into())];

        let config = PumpkinConfig::default();
        let settings = settings(json!({
            "ONLINE_MODE": "false",
            "OPS": "Notch",
            "WHITELIST": "jeb_",
            "ENABLE_WHITELIST": "true",
        }));
        apply_lists(&settings, &config, &mut data);

        let ops = &data.operator_config.get_mut().ops;
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].name, "Notch");
        // A de-opped player must actually lose it on the next start.
        assert!(!ops.iter().any(|op| op.name == "WasAnOp"));

        let list = &data.whitelist_config.get_mut().whitelist;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "jeb_");
    }

    #[test]
    fn a_ban_made_on_the_device_survives_a_launch() {
        let mut data = data();
        let local = Uuid::from_u128(7);
        data.banned_player_list
            .get_mut()
            .banned_players
            .push(BannedPlayerEntry {
                uuid: local,
                name: "BannedInGame".into(),
                created: time::OffsetDateTime::now_utc(),
                source: "Console".into(),
                expires: None,
                reason: "griefing".into(),
            });

        let config = PumpkinConfig::default();
        let settings = settings(json!({ "ONLINE_MODE": "false", "BANNED": "Notch" }));
        apply_lists(&settings, &config, &mut data);

        let bans = &data.banned_player_list.get_mut().banned_players;
        assert_eq!(bans.len(), 2);
        assert!(bans.iter().any(|entry| entry.uuid == local));
        assert!(bans.iter().any(|entry| entry.name == "Notch"));

        // And applying the same settings twice does not duplicate anyone.
        apply_lists(&settings, &config, &mut data);
        assert_eq!(data.banned_player_list.get_mut().banned_players.len(), 2);
    }

    /// Pumpkin's deserializer requires `+00:00`; core's ban writer emits
    /// `+0000`. If these entries are ever built from that side instead, this
    /// is the test that fails rather than the next launch.
    #[test]
    fn a_written_ban_can_be_read_back() {
        let mut data = data();
        let config = PumpkinConfig::default();
        apply_lists(
            &settings(json!({ "ONLINE_MODE": "false", "BANNED": "Notch" })),
            &config,
            &mut data,
        );

        let json = serde_json::to_string(&data.banned_player_list.get_mut().banned_players).unwrap();
        let round_tripped: Vec<BannedPlayerEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.len(), 1);
    }

    /// An online-mode name nobody could resolve has no UUID to write, and that
    /// is not a reason to fail a launch.
    #[test]
    fn an_unresolvable_operator_is_dropped_rather_than_fatal() {
        let mut data = data();
        let config = PumpkinConfig::default();
        apply_lists(&settings(json!({ "OPS": "Ghost" })), &config, &mut data);
        assert!(data.operator_config.get_mut().ops.is_empty());
    }
}

//! What the API says a world should be, turned into the files a server reads.
//!
//! Reference: `nativeServerManager.ts` — `fetchServerConfig:3755` for the
//! environment mapping, `writeServerProperties:4022` and
//! `mergeServerProperties:3981` for `server.properties`,
//! `writeOpsAndWhitelistFiles:2408` and `mergeBannedPlayersFile:2459` for the
//! player lists, `offlineUuid:4351` for the UUID derivation.
//!
//! # Why this matters more than it looks
//!
//! Every choice a player makes in the creation wizard — game mode, difficulty,
//! seed, max players, PVP, hardcore, ops, whitelist — arrives as a string in
//! the server's `environment_variables` and reaches the world *only* by being
//! written into `server.properties`, `ops.json` and `whitelist.json` before the
//! JVM starts. A host that skips this step produces a server that boots
//! perfectly and is wrong in every respect, with no error anywhere. Android did
//! exactly that until this module existed: a server created with
//! `MOTD=Android tunnel test` answered a server-list ping with
//! `"description": "A Minecraft Server"`.
//!
//! # Two divergences from the desktop, both deliberate
//!
//! **Online mode is derived once.** The desktop computes it twice and the two
//! disagree. `server.properties` gets `crossplay && loader == "vanilla" ? false
//! : true` (`writeServerProperties:4057`), ignoring `ONLINE_MODE` entirely,
//! while UUID resolution uses `ONLINE_MODE !== "false"`
//! (`writeOpsAndWhitelistFiles:2413`). Both mismatches are reachable and both
//! silently break op-ing:
//!
//!  - a crossplay vanilla server runs offline but ops are written with Mojang
//!    UUIDs, which the server will never match
//!  - a server with `ONLINE_MODE=false` runs online but ops are written with
//!    derived offline UUIDs, likewise unmatched
//!
//! [`Settings::online_mode`] is the single answer both uses read, and
//! [`properties`] writes that same value. See the tests named for each case.
//!
//! **Numbers never become `NaN`.** `parseInt(env.MAX_PLAYERS || "20")` yields
//! `NaN` for a non-numeric value, and `String(NaN)` writes the literal text
//! `NaN` into `server.properties`. Here a value that cannot be read falls back
//! to the documented default.

use crate::md5;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The desktop's fallback MOTD, used when the server sets none.
///
/// Hosts may override it via [`from_env`]; passing `None` keeps every platform
/// identical, which is the default worth preferring.
pub const DEFAULT_MOTD: &str = "Next generation Minecraft server powered by §9Homerun Desktop";

/// Old servers (confirmed on 1.2.5) hard-crash with "Too big view radius!"
/// above this. Clamped unconditionally rather than tracking which release
/// lifted the cap.
pub const MAX_VIEW_DISTANCE: u32 = 15;

/// A world's settings, resolved from `environment_variables`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub motd: String,
    pub game_mode: String,
    pub difficulty: String,
    pub seed: String,
    pub world_type: String,
    pub max_players: u32,
    pub view_distance: u32,
    pub simulation_distance: u32,
    pub local_port: u16,
    pub enable_command_blocks: bool,
    pub hardcore: bool,
    pub spawn_npcs: bool,
    pub spawn_animals: bool,
    pub spawn_monsters: bool,
    pub pvp: bool,
    pub generate_structures: bool,
    pub whitelist_enabled: bool,
    /// The one answer both `server.properties` and UUID derivation use.
    pub online_mode: bool,
    pub op_users: Vec<String>,
    pub whitelisted_users: Vec<String>,
    pub banned_users: Vec<String>,
}

/// What only the host knows at launch: the ports it actually secured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    /// The port the JVM will bind, after any conflict was resolved.
    pub port: u16,
    /// `127.0.0.1`, or `0.0.0.0` when the user opted into LAN exposure.
    pub bind_address: String,
    /// Omitted by hosts that drive the console over stdin rather than RCON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rcon: Option<Rcon>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rcon {
    pub port: u16,
    pub password: String,
}

/// A player, resolved to the UUID the server will match them by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Player {
    pub name: String,
    pub uuid: String,
}

/// Read an env value as a string, whatever JSON type it arrived as.
///
/// The desktop compares against `"true"` and `"false"` directly, so a backend
/// that sent a JSON boolean instead of a string would silently take the
/// wrong branch there. Coercing first makes both spellings behave the same.
fn text(env: &Value, key: &str) -> Option<String> {
    match env.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// `true` unless the value is exactly `"false"` — the desktop's `!== "false"`.
fn on_unless_false(env: &Value, key: &str) -> bool {
    text(env, key).as_deref() != Some("false")
}

/// `true` only when the value is exactly `"true"` — the desktop's `=== "true"`.
fn only_if_true(env: &Value, key: &str) -> bool {
    text(env, key).as_deref() == Some("true")
}

/// Leading-digits parse, mirroring `parseInt` without its `NaN`.
fn number<T: std::str::FromStr>(env: &Value, keys: &[&str], default: T) -> T {
    for key in keys {
        let Some(raw) = text(env, key) else { continue };
        let trimmed = raw.trim();
        let digits: String = trimmed
            .char_indices()
            .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+')))
            .map(|(_, c)| c)
            .collect();
        if let Ok(parsed) = digits.parse::<T>() {
            return parsed;
        }
    }
    default
}

/// First non-empty of `keys`, else `default`.
fn text_or(env: &Value, keys: &[&str], default: &str) -> String {
    for key in keys {
        if let Some(value) = text(env, key) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    default.to_string()
}

/// Resolve a player list from the canonical string, falling back to legacy
/// arrays **only when the canonical key is absent**.
///
/// Key presence, not truthiness. An empty `WHITELIST` means "nobody", and
/// treating it as falsy is the bug that let a stale `whitelisted_users` array
/// re-add a player who had just been removed (`serverEnv.ts` records this).
pub fn parse_user_list(env: &Value, primary: &str, fallbacks: &[&str]) -> Vec<String> {
    if let Some(Value::String(raw)) = env.get(primary) {
        return raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    for key in fallbacks {
        if let Some(Value::Array(items)) = env.get(key) {
            return items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
    }
    Vec::new()
}

/// Resolve `environment_variables` into settings.
///
/// `game_type` is the API's (`native-crossplay` forces offline mode on vanilla,
/// because Geyser clients have no Mojang account to verify). `loader` is the
/// already-resolved loader, not the raw `TYPE`. `fallback_motd` overrides
/// [`DEFAULT_MOTD`].
pub fn from_env(
    env: &Value,
    game_type: &str,
    loader: &str,
    fallback_motd: Option<&str>,
) -> Settings {
    let crossplay = game_type == "native-crossplay";

    Settings {
        motd: text_or(env, &["MOTD"], fallback_motd.unwrap_or(DEFAULT_MOTD)),
        game_mode: text_or(env, &["GAMEMODE", "MODE"], "survival"),
        difficulty: text_or(env, &["DIFFICULTY"], "normal"),
        seed: text_or(env, &["LEVEL_SEED", "SEED"], ""),
        world_type: text_or(env, &["LEVEL_TYPE"], "default"),
        max_players: number(env, &["MAX_PLAYERS"], 20),
        view_distance: number(env, &["VIEW_DISTANCE"], 10),
        simulation_distance: number(env, &["SIMULATION_DISTANCE", "TICK_DISTANCE"], 10),
        local_port: number(env, &["SERVER_PORT"], 25565),
        enable_command_blocks: only_if_true(env, "ENABLE_COMMAND_BLOCK")
            || only_if_true(env, "ALLOW_CHEATS"),
        hardcore: only_if_true(env, "HARDCORE"),
        spawn_npcs: on_unless_false(env, "SPAWN_NPCS"),
        spawn_animals: on_unless_false(env, "SPAWN_ANIMALS"),
        spawn_monsters: on_unless_false(env, "SPAWN_MONSTERS"),
        pvp: on_unless_false(env, "PVP"),
        generate_structures: on_unless_false(env, "GENERATE_STRUCTURES"),
        whitelist_enabled: only_if_true(env, "ENABLE_WHITELIST") || only_if_true(env, "whitelist"),
        // The single derivation. See the module's divergence note.
        online_mode: on_unless_false(env, "ONLINE_MODE") && !(crossplay && loader == "vanilla"),
        op_users: parse_user_list(env, "OPS", &["op_users"]),
        whitelisted_users: parse_user_list(
            env,
            "WHITELIST",
            &["whitelisted_users", "whitelistedUsers"],
        ),
        banned_users: parse_user_list(env, "BANNED", &["banned_users"]),
    }
}

/// The `server.properties` keys this host manages, in the desktop's order.
///
/// Everything else in the file — keys the server wrote, comments, settings a
/// player edited by hand — is preserved by [`crate::properties::merge`]. Only
/// these are
/// ours to overwrite on every launch.
pub fn properties(settings: &Settings, runtime: &Runtime) -> Vec<(String, String)> {
    let flag = |on: bool| if on { "true" } else { "false" }.to_string();
    let mut out: Vec<(String, String)> = vec![
        ("motd".into(), settings.motd.clone()),
        ("gamemode".into(), settings.game_mode.clone()),
        ("difficulty".into(), settings.difficulty.clone()),
        ("max-players".into(), settings.max_players.to_string()),
        (
            "view-distance".into(),
            settings.view_distance.min(MAX_VIEW_DISTANCE).to_string(),
        ),
        (
            "simulation-distance".into(),
            settings.simulation_distance.to_string(),
        ),
        ("level-type".into(), settings.world_type.clone()),
        (
            "enable-command-block".into(),
            flag(settings.enable_command_blocks),
        ),
        ("level-seed".into(), settings.seed.clone()),
        ("hardcore".into(), flag(settings.hardcore)),
        ("spawn-npcs".into(), flag(settings.spawn_npcs)),
        ("spawn-animals".into(), flag(settings.spawn_animals)),
        ("spawn-monsters".into(), flag(settings.spawn_monsters)),
        ("pvp".into(), flag(settings.pvp)),
        (
            "generate-structures".into(),
            flag(settings.generate_structures),
        ),
        ("white-list".into(), flag(settings.whitelist_enabled)),
        ("server-ip".into(), runtime.bind_address.clone()),
        ("server-port".into(), runtime.port.to_string()),
        ("query.port".into(), runtime.port.to_string()),
        ("online-mode".into(), flag(settings.online_mode)),
        ("spawn-protection".into(), "0".into()),
    ];

    // Hosts that drive the console over the process's stdin have no use for
    // RCON, and writing a password for a port nothing listens on is noise.
    if let Some(rcon) = &runtime.rcon {
        out.push(("enable-rcon".into(), "true".into()));
        out.push(("rcon.port".into(), rcon.port.to_string()));
        out.push(("rcon.password".into(), rcon.password.clone()));
        out.push(("broadcast-rcon-to-ops".into(), "false".into()));
    }

    out
}

/// The UUID an offline server derives for a name.
///
/// Java's `UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes())` — an
/// MD5 with the version-3 and RFC-4122 variant bits forced. Deterministic, so
/// it needs no network, which is why offline servers never fail to op.
pub fn offline_uuid(name: &str) -> String {
    let mut hash = md5::digest(format!("OfflinePlayer:{name}").as_bytes());
    hash[6] = (hash[6] & 0x0f) | 0x30; // version 3
    hash[8] = (hash[8] & 0x3f) | 0x80; // variant RFC 4122
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    dash(&hex)
}

/// Insert dashes into a 32-character hex UUID, as Mojang's API returns it.
pub fn dash_uuid(undashed: &str) -> Result<String> {
    let clean: String = undashed.replace('-', "").to_lowercase();
    if clean.len() != 32 || !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Malformed(format!(
            "\"{undashed}\" is not a 32-character hexadecimal UUID"
        )));
    }
    Ok(dash(&clean))
}

fn dash(hex: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `ops.json`, rewritten wholesale on every launch.
///
/// Wholesale is the point: the file is the source of truth the server reads
/// before it accepts anyone, so a player removed from the list stops being an
/// operator on the next start. An add-only pass would leave them op forever.
pub fn ops_json(players: &[Player]) -> Value {
    Value::Array(
        players
            .iter()
            .map(|p| {
                json!({
                    "uuid": p.uuid,
                    "name": p.name,
                    "level": 4,
                    "bypassesPlayerLimit": false,
                })
            })
            .collect(),
    )
}

/// `whitelist.json`, rewritten wholesale for the same reason as [`ops_json`].
pub fn whitelist_json(players: &[Player]) -> Value {
    Value::Array(
        players
            .iter()
            .map(|p| json!({ "uuid": p.uuid, "name": p.name }))
            .collect(),
    )
}

/// Which of `banned` are not already in an existing `banned-players.json`.
///
/// Returned so the caller resolves UUIDs only for names it will actually
/// write — on an online-mode server each resolution is a Mojang request.
/// Unparseable or missing content is treated as empty, exactly as the server
/// treats it.
pub fn banned_missing(existing: &str, banned: &[String]) -> Vec<String> {
    let known: Vec<String> = serde_json::from_str::<Value>(existing)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
        .map(str::to_lowercase)
        .collect();

    banned
        .iter()
        .filter(|name| !known.contains(&name.to_lowercase()))
        .cloned()
        .collect()
}

/// Append bans to `banned-players.json`, preserving what is already there.
///
/// Append-only, unlike ops and whitelist. The server maintains this file
/// itself — an in-game `/ban` lands here without any sync seeing it — so
/// rewriting it from the API would erase legitimate local bans. The API's list
/// only carries bans made from the UI or console across devices; `/pardon`
/// edits the file directly and the matching env removal stops another device
/// re-adding the name.
///
/// `created` is the server's timestamp format, e.g.
/// `2026-08-10 14:03:22 +0000`; it is passed in because this crate has no
/// clock. Returns `None` when there is nothing to add.
pub fn merge_banned(existing: &str, additions: &[Player], created: &str) -> Option<String> {
    if additions.is_empty() {
        return None;
    }
    let mut all: Vec<Value> = serde_json::from_str::<Value>(existing)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    for player in additions {
        all.push(json!({
            "uuid": player.uuid,
            "name": player.name,
            "created": created,
            "source": "Homerun",
            "expires": "forever",
            "reason": "Banned by an operator.",
        }));
    }

    serde_json::to_string_pretty(&Value::Array(all)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, Value)]) -> Value {
        let mut map = serde_json::Map::new();
        for (k, v) in pairs {
            map.insert((*k).to_string(), v.clone());
        }
        Value::Object(map)
    }

    fn defaults() -> Settings {
        from_env(&env(&[]), "java", "vanilla", None)
    }

    fn runtime() -> Runtime {
        Runtime {
            port: 25565,
            bind_address: "127.0.0.1".into(),
            rcon: None,
        }
    }

    fn value_of(props: &[(String, String)], key: &str) -> String {
        props
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("no managed key {key}"))
    }

    // ─── the regression this module exists for ──────────────────────────────

    /// The exact failure that motivated all of this: a server created with a
    /// custom MOTD answered pings with the vanilla default.
    #[test]
    fn a_custom_motd_reaches_server_properties() {
        let settings = from_env(
            &env(&[("MOTD", json!("Android tunnel test"))]),
            "java",
            "vanilla",
            None,
        );
        let props = properties(&settings, &runtime());
        assert_eq!(value_of(&props, "motd"), "Android tunnel test");

        let file = crate::properties::merge("", &props);
        assert!(
            file.contains("motd=Android tunnel test"),
            "the MOTD must survive into the file:\n{file}"
        );
    }

    #[test]
    fn an_absent_motd_falls_back_to_the_desktops() {
        assert_eq!(defaults().motd, DEFAULT_MOTD);
        assert!(
            DEFAULT_MOTD.contains('§'),
            "the fallback carries a colour code"
        );
    }

    #[test]
    fn a_host_may_override_the_fallback_motd() {
        let settings = from_env(&env(&[]), "java", "vanilla", Some("Hosted on a phone"));
        assert_eq!(settings.motd, "Hosted on a phone");
    }

    // ─── the environment mapping ────────────────────────────────────────────

    #[test]
    fn every_default_matches_the_desktop() {
        let s = defaults();
        assert_eq!(s.game_mode, "survival");
        assert_eq!(s.difficulty, "normal");
        assert_eq!(s.seed, "");
        assert_eq!(s.world_type, "default");
        assert_eq!(s.max_players, 20);
        assert_eq!(s.view_distance, 10);
        assert_eq!(s.simulation_distance, 10);
        assert_eq!(s.local_port, 25565);
        assert!(!s.enable_command_blocks);
        assert!(!s.hardcore);
        assert!(!s.whitelist_enabled);
        assert!(s.spawn_npcs && s.spawn_animals && s.spawn_monsters);
        assert!(s.pvp && s.generate_structures);
        assert!(s.online_mode);
    }

    #[test]
    fn the_legacy_alias_keys_are_honoured() {
        let s = from_env(
            &env(&[
                ("MODE", json!("creative")),
                ("SEED", json!("12345")),
                ("TICK_DISTANCE", json!("6")),
                ("ALLOW_CHEATS", json!("true")),
            ]),
            "java",
            "vanilla",
            None,
        );
        assert_eq!(s.game_mode, "creative");
        assert_eq!(s.seed, "12345");
        assert_eq!(s.simulation_distance, 6);
        assert!(s.enable_command_blocks);
    }

    /// `GAMEMODE` and `LEVEL_SEED` win over the older spellings.
    #[test]
    fn the_canonical_key_beats_its_alias() {
        let s = from_env(
            &env(&[
                ("GAMEMODE", json!("adventure")),
                ("MODE", json!("creative")),
                ("LEVEL_SEED", json!("canonical")),
                ("SEED", json!("legacy")),
            ]),
            "java",
            "vanilla",
            None,
        );
        assert_eq!(s.game_mode, "adventure");
        assert_eq!(s.seed, "canonical");
    }

    #[test]
    fn switch_flags_are_on_unless_exactly_false() {
        for (raw, expected) in [("false", false), ("true", true), ("no", true), ("", true)] {
            let s = from_env(&env(&[("PVP", json!(raw))]), "java", "vanilla", None);
            assert_eq!(s.pvp, expected, "PVP={raw:?}");
        }
    }

    #[test]
    fn opt_in_flags_are_off_unless_exactly_true() {
        for (raw, expected) in [("true", true), ("false", false), ("yes", false)] {
            let s = from_env(&env(&[("HARDCORE", json!(raw))]), "java", "vanilla", None);
            assert_eq!(s.hardcore, expected, "HARDCORE={raw:?}");
        }
    }

    /// A backend that sends JSON booleans rather than strings must not flip
    /// every flag to its wrong default — the desktop's `=== "true"` would.
    #[test]
    fn json_booleans_behave_like_their_string_spelling() {
        let s = from_env(
            &env(&[("HARDCORE", json!(true)), ("PVP", json!(false))]),
            "java",
            "vanilla",
            None,
        );
        assert!(s.hardcore);
        assert!(!s.pvp);
    }

    #[test]
    fn a_number_arriving_as_json_is_read() {
        let s = from_env(&env(&[("MAX_PLAYERS", json!(64))]), "java", "vanilla", None);
        assert_eq!(s.max_players, 64);
    }

    /// The desktop writes the literal text `NaN` here.
    #[test]
    fn an_unreadable_number_falls_back_rather_than_becoming_nan() {
        let s = from_env(
            &env(&[("MAX_PLAYERS", json!("lots")), ("VIEW_DISTANCE", json!(""))]),
            "java",
            "vanilla",
            None,
        );
        assert_eq!(s.max_players, 20);
        assert_eq!(s.view_distance, 10);

        let props = properties(&s, &runtime());
        assert_eq!(value_of(&props, "max-players"), "20");
        assert!(
            !crate::properties::merge("", &props).contains("NaN"),
            "no property may ever be written as NaN"
        );
    }

    // ─── online mode: the divergence ────────────────────────────────────────

    /// A crossplay vanilla server runs offline, so its ops must be derived
    /// offline too. The desktop writes Mojang UUIDs here and the ops silently
    /// never apply.
    #[test]
    fn crossplay_vanilla_is_offline_for_both_properties_and_uuids() {
        let s = from_env(&env(&[]), "native-crossplay", "vanilla", None);
        assert!(!s.online_mode);
        assert_eq!(
            value_of(&properties(&s, &runtime()), "online-mode"),
            "false",
            "properties and UUID derivation must agree"
        );
    }

    /// The other half of the same divergence: an explicit opt-out has to reach
    /// `server.properties`, which the desktop's derivation ignores entirely.
    #[test]
    fn an_explicit_online_mode_false_reaches_properties() {
        let s = from_env(
            &env(&[("ONLINE_MODE", json!("false"))]),
            "java",
            "vanilla",
            None,
        );
        assert!(!s.online_mode);
        assert_eq!(
            value_of(&properties(&s, &runtime()), "online-mode"),
            "false"
        );
    }

    /// Crossplay only forces offline for vanilla; Paper authenticates through
    /// Geyser's own flow and stays online.
    #[test]
    fn crossplay_on_paper_stays_online() {
        let s = from_env(&env(&[]), "native-crossplay", "paper", None);
        assert!(s.online_mode);
    }

    // ─── player lists ───────────────────────────────────────────────────────

    #[test]
    fn a_user_list_splits_and_trims() {
        let s = from_env(
            &env(&[("OPS", json!("Notch, jeb_ ,,Dinnerbone"))]),
            "java",
            "vanilla",
            None,
        );
        assert_eq!(s.op_users, vec!["Notch", "jeb_", "Dinnerbone"]);
    }

    /// The bug `serverEnv.ts` documents: an emptied canonical list must mean
    /// "nobody", not "fall through to the stale array".
    #[test]
    fn an_empty_canonical_list_beats_a_stale_array() {
        let s = from_env(
            &env(&[
                ("WHITELIST", json!("")),
                ("whitelisted_users", json!(["Removed"])),
            ]),
            "java",
            "vanilla",
            None,
        );
        assert!(
            s.whitelisted_users.is_empty(),
            "a removed player must not be re-added by the legacy array"
        );
    }

    #[test]
    fn the_legacy_array_is_used_only_when_the_canonical_key_is_absent() {
        let s = from_env(
            &env(&[("whitelisted_users", json!(["Steve", "Alex"]))]),
            "java",
            "vanilla",
            None,
        );
        assert_eq!(s.whitelisted_users, vec!["Steve", "Alex"]);
    }

    // ─── UUIDs ──────────────────────────────────────────────────────────────

    /// Pinned against the desktop's `offlineUuid` for the same names. If these
    /// drift, every op on every offline server silently stops applying.
    #[test]
    fn offline_uuids_match_the_desktop() {
        for (name, expected) in [
            ("Notch", "b50ad385-829d-3141-a216-7e7d7539ba7f"),
            ("jeb_", "a762f560-4fce-3236-812a-b80efff0b62b"),
            ("Dinnerbone", "4d258a81-2358-3084-8166-05b9faccad80"),
            ("a", "52428a0e-1e30-3cb1-976c-e728b2614047"),
            ("Steve", "5627dd98-e6be-3c21-b8a8-e92344183641"),
            ("ÄÖÜ_ünïcode", "23c8e2e6-fa3d-3410-96a1-29c96f2407c8"),
        ] {
            assert_eq!(offline_uuid(name), expected, "offline UUID for {name:?}");
        }
    }

    #[test]
    fn an_offline_uuid_carries_version_3_and_the_rfc_variant() {
        let uuid = offline_uuid("Notch");
        assert_eq!(uuid.chars().nth(14), Some('3'), "version nibble");
        assert!(
            matches!(uuid.chars().nth(19), Some('8' | '9' | 'a' | 'b')),
            "variant nibble in {uuid}"
        );
    }

    #[test]
    fn mojangs_undashed_uuid_gets_dashes() {
        assert_eq!(
            dash_uuid("069a79f444e94726a5befca90e38aaf5").unwrap(),
            "069a79f4-44e9-4726-a5be-fca90e38aaf5"
        );
    }

    #[test]
    fn a_malformed_uuid_is_refused_rather_than_sliced() {
        for bad in ["", "abc", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"] {
            assert!(dash_uuid(bad).is_err(), "{bad:?} must be refused");
        }
    }

    // ─── through the generic merge ──────────────────────────────────────────
    //
    // The merge itself is `crate::properties`, tested there against every
    // shape of input. What is worth asserting here is only that Minecraft's
    // managed keys survive it — the seam, not the mechanism.

    #[test]
    fn the_managed_keys_reach_a_merged_file() {
        let file = crate::properties::merge("", &properties(&defaults(), &runtime()));
        assert!(file.starts_with("motd="));
        assert!(file.contains("difficulty=normal"));
        assert!(file.ends_with('\n'));
    }

    #[test]
    fn a_servers_own_keys_survive_a_launch() {
        let existing = "#Minecraft server properties\nrate-limit=0\nmotd=old\n";
        let file = crate::properties::merge(existing, &properties(&defaults(), &runtime()));
        assert!(
            file.contains("rate-limit=0"),
            "an unmanaged key was dropped"
        );
        assert!(file.contains(&format!("motd={DEFAULT_MOTD}")));
        assert!(!file.contains("motd=old"));
    }

    #[test]
    fn the_view_distance_cap_is_enforced() {
        let s = from_env(
            &env(&[("VIEW_DISTANCE", json!("32"))]),
            "java",
            "vanilla",
            None,
        );
        assert_eq!(s.view_distance, 32, "the setting itself is not rewritten");
        assert_eq!(
            value_of(&properties(&s, &runtime()), "view-distance"),
            "15",
            "old servers hard-crash above this"
        );
    }

    #[test]
    fn rcon_keys_appear_only_when_the_host_uses_rcon() {
        let without = properties(&defaults(), &runtime());
        assert!(!without.iter().any(|(k, _)| k == "enable-rcon"));

        let with = properties(
            &defaults(),
            &Runtime {
                port: 25565,
                bind_address: "127.0.0.1".into(),
                rcon: Some(Rcon {
                    port: 25575,
                    password: "hunter2".into(),
                }),
            },
        );
        assert_eq!(value_of(&with, "rcon.port"), "25575");
        assert_eq!(value_of(&with, "rcon.password"), "hunter2");
        assert_eq!(value_of(&with, "broadcast-rcon-to-ops"), "false");
    }

    #[test]
    fn the_runtime_port_wins_over_the_configured_one() {
        let s = from_env(
            &env(&[("SERVER_PORT", json!("25565"))]),
            "java",
            "vanilla",
            None,
        );
        let props = properties(
            &s,
            &Runtime {
                port: 25570, // the host had to move off a busy port
                bind_address: "0.0.0.0".into(),
                rcon: None,
            },
        );
        assert_eq!(value_of(&props, "server-port"), "25570");
        assert_eq!(value_of(&props, "query.port"), "25570");
        assert_eq!(value_of(&props, "server-ip"), "0.0.0.0");
    }

    // ─── ops, whitelist, bans ───────────────────────────────────────────────

    #[test]
    fn ops_carry_level_4() {
        let json = ops_json(&[Player {
            name: "Notch".into(),
            uuid: "b50ad385-829d-3141-a216-7e7d7539ba7f".into(),
        }]);
        let entry = &json.as_array().unwrap()[0];
        assert_eq!(entry["level"], 4);
        assert_eq!(entry["bypassesPlayerLimit"], false);
        assert_eq!(entry["name"], "Notch");
    }

    #[test]
    fn an_empty_op_list_is_an_empty_array_not_null() {
        assert_eq!(ops_json(&[]), json!([]));
        assert_eq!(whitelist_json(&[]), json!([]));
    }

    #[test]
    fn bans_already_present_are_not_re_added() {
        let existing = r#"[{"uuid":"x","name":"Griefer"}]"#;
        let missing = banned_missing(existing, &["Griefer".into(), "Other".into()]);
        assert_eq!(missing, vec!["Other"]);
    }

    #[test]
    fn ban_matching_ignores_case() {
        let existing = r#"[{"uuid":"x","name":"GRIEFER"}]"#;
        assert!(banned_missing(existing, &["griefer".into()]).is_empty());
    }

    #[test]
    fn a_corrupt_ban_file_is_treated_as_empty() {
        for broken in ["", "not json", "{}", "null"] {
            assert_eq!(
                banned_missing(broken, &["Griefer".into()]),
                vec!["Griefer"],
                "input {broken:?}"
            );
        }
    }

    /// Append-only: a ban the server wrote itself must survive our merge.
    #[test]
    fn merging_bans_preserves_local_ones() {
        let existing = r#"[{"uuid":"local","name":"BannedInGame"}]"#;
        let merged = merge_banned(
            existing,
            &[Player {
                name: "FromUi".into(),
                uuid: "remote".into(),
            }],
            "2026-08-10 14:03:22 +0000",
        )
        .expect("something to write");

        let parsed: Value = serde_json::from_str(&merged).unwrap();
        let names: Vec<&str> = parsed
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["BannedInGame", "FromUi"]);
        assert_eq!(parsed[1]["source"], "Homerun");
        assert_eq!(parsed[1]["expires"], "forever");
        assert_eq!(parsed[1]["created"], "2026-08-10 14:03:22 +0000");
    }

    #[test]
    fn merging_no_bans_writes_nothing() {
        assert!(merge_banned("[]", &[], "now").is_none());
    }
}

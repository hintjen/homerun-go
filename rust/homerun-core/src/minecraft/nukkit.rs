//! PowerNukkitX: a Bedrock server that happens to be a jar.
//!
//! # Why this is a branch and not a second [`crate::game::Game`]
//!
//! It is the same shape as Pumpkin. A player picks it by name, it gets its own
//! game type (`native-powernukkitx`), and — like Pumpkin — that game type is
//! immutable once a server exists, because a world written by one engine must
//! never be opened by another. What differs from vanilla is the *file set*, not
//! the model: settings resolve out of `environment_variables` exactly as they
//! do for Java, and only the writing-down changes.
//!
//! So [`crate::minecraft::Minecraft`] branches on the game type and delegates
//! here. Nothing in `game/v1` moves.
//!
//! # Everything below was read out of PowerNukkitX, not guessed
//!
//! Verified against `master` at release 3.0.3. The parts that are easy to get
//! wrong, and were:
//!
//!  - **The config file is `pnx.yml`**, not `server-settings.yml`. The class is
//!    `config/ServerSettings.java`; the filename is in `Server.java` and in
//!    `PowerNukkitX.java`, which decides whether to run the setup wizard by
//!    asking whether that exact file exists.
//!  - **okaeri renders camelCase fields as kebab-case keys** — `maxPlayers`
//!    becomes `max-players` — and saves with `withRemoveOrphans(true)`, so a
//!    key it does not recognise is silently deleted on the next boot. A typo
//!    here does not fail loudly; it stops taking effect.
//!  - **`gamemode` and `difficulty` are integers**, not the words
//!    `server.properties` uses.
//!  - **Online mode is `settings.xbox-auth`.**
//!  - **The seed is not in `pnx.yml` at all.** It lives per-world in
//!    `worlds/<name>/config.json`, and `Server.generateLevel` prefers that file
//!    over anything passed to it — which is the only reason a requested seed
//!    can be honoured. See [`level_config`].
//!
//! # What this deliberately does not do
//!
//! No `eula.txt`: there is no Mojang EULA in play, and PNX takes its own
//! licence on the command line (`--accept-license`, see
//! [`crate::minecraft::jvm::NUKKIT_PROGRAM_ARGS`]).
//!
//! No Mojang identity lookups. A Bedrock player is a gamertag; `ops.txt` and
//! `white-list.txt` hold plain names, one per line, and a UUID resolved against
//! Mojang would match nobody. That is why [`required_lookups`] is empty rather
//! than merely unused — returning names would make the host issue requests
//! whose answers are wrong.

use serde_json::{json, Map, Value};

use super::jar::{Algorithm, Artifact, Checksum};
use super::settings::{self, Settings};
use crate::game::{BuildContext, ConfigInput, Encoding, FileWrite, Lookup};
use crate::{Error, Result};

/// PowerNukkitX reads its settings from here, and `PowerNukkitX.main` decides
/// whether to run the interactive setup wizard by asking whether this file
/// exists. Writing it before the first launch is what keeps a phone from
/// sitting at `starting` forever waiting on an answer typed into stdin.
pub const SETTINGS_FILE: &str = "pnx.yml";

/// Plain names, one per line — `Config.ENUM` in Nukkit terms.
pub const OPS_FILE: &str = "ops.txt";
pub const ALLOWLIST_FILE: &str = "white-list.txt";

/// A JSON *array* of `{name, creationDate, source, expireDate, reason}`, which
/// is not the shape a Java server uses for the same filename.
pub const BANNED_FILE: &str = "banned-players.json";

/// PowerNukkitX targets Java 21, and 25 removes the `sun.misc.Unsafe` memory
/// access that Netty and fastutil still reach for. Not a floor — the number.
pub const REQUIRED_JAVA: u16 = 21;

/// What `settings.default-level-name` is set to, and therefore which directory
/// under `worlds/` a requested seed has to be written into.
pub const DEFAULT_LEVEL_NAME: &str = "world";

/// The ceiling this host puts on `view-distance`.
///
/// Not a PowerNukkitX limit — it has none. The create wizard offers up to 64
/// chunks because that is what a rented Bedrock server can do, and a phone
/// cannot: every chunk in view is heap, and Android kills the whole app under
/// memory pressure rather than just the server. A starting point, and the
/// number M8 measurement is expected to move.
pub const MAX_VIEW_DISTANCE: u32 = 16;

/// The loader key a PowerNukkitX jar is cached under.
///
/// Deliberately not one of [`super::jar::Loader`]'s: that enum is what `TYPE`
/// parses into, and PowerNukkitX is not a loader anyone can put on a Java
/// server. It only has to be a string no Mojang jar shares, so that a
/// `powernukkitx` 3.0.3 and some future Minecraft 3.0.3 cannot collide in the
/// shared jar cache.
pub const LOADER: &str = "powernukkitx";

/// Where a world's generator and seed live, relative to the server directory.
pub fn level_config_path(level_name: &str) -> String {
    format!("worlds/{level_name}/config.json")
}

/// Files this game needs read before its config can be written.
///
/// `pnx.yml` because PowerNukkitX rewrites it on every boot and a player may
/// have edited it in between — writing a fresh one each launch would silently
/// discard both.
///
/// The world's `config.json` because it must never be overwritten: after the
/// first generation it holds the world's real identity, and replacing it with
/// a freshly-derived seed would generate a *different world* into the same
/// directory. Reading it is how [`config_files`] knows to leave it alone.
pub fn config_inputs() -> Vec<ConfigInput> {
    vec![
        ConfigInput {
            path: SETTINGS_FILE.into(),
            encoding: Encoding::Utf8,
        },
        ConfigInput {
            path: BANNED_FILE.into(),
            encoding: Encoding::Utf8,
        },
        ConfigInput {
            path: level_config_path(DEFAULT_LEVEL_NAME),
            encoding: Encoding::Utf8,
        },
    ]
}

/// None, ever. See the module docs.
pub fn required_lookups() -> Vec<Lookup> {
    Vec::new()
}

/// Bedrock has no MOTD field of its own — the create wizard calls it the server
/// name — so `SERVER_NAME` stands in when `MOTD` is absent.
fn motd(env: &Value) -> Option<String> {
    for key in ["MOTD", "SERVER_NAME"] {
        if let Some(text) = env.get(key).and_then(|v| v.as_str()) {
            if !text.trim().is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// `GAMEMODE` as PowerNukkitX numbers them.
///
/// Unknown means survival: a server that quietly starts everyone in creative
/// because a spelling changed is worse than one that ignores the setting.
pub fn gamemode_of(mode: &str) -> u8 {
    match mode.trim().to_ascii_lowercase().as_str() {
        "creative" | "1" => 1,
        "adventure" | "2" => 2,
        "spectator" | "3" => 3,
        _ => 0,
    }
}

/// `DIFFICULTY` as PowerNukkitX numbers them. Unknown means easy, which is
/// PowerNukkitX's own default.
pub fn difficulty_of(difficulty: &str) -> u8 {
    match difficulty.trim().to_ascii_lowercase().as_str() {
        "peaceful" | "0" => 0,
        "normal" | "2" => 2,
        "hard" | "3" => 3,
        _ => 1,
    }
}

/// `LEVEL_TYPE` as a PowerNukkitX generator name.
///
/// The wizard offers Bedrock's three (`DEFAULT`, `FLAT`, `LEGACY`) and
/// PowerNukkitX registers `flat`, `normal`, `nether`, `the_end` and `void`.
/// **`LEGACY` has no equivalent** — it is Bedrock's finite 512×512 world, which
/// PowerNukkitX cannot generate — so it becomes an ordinary infinite world
/// rather than a refusal. A player who picked it gets a world; what they do not
/// get is the world border, and no amount of mapping here would give them one.
pub fn generator_of(level_type: &str) -> &'static str {
    match level_type.trim().to_ascii_uppercase().as_str() {
        "FLAT" => "flat",
        "VOID" => "void",
        _ => "normal",
    }
}

/// The seed a `LEVEL_SEED` means.
///
/// A number is itself. Anything else is Java's `String.hashCode`, which is what
/// Minecraft does with a typed seed and therefore the only answer that makes
/// "the seed I used on my other server" work.
pub fn seed_of(raw: &str) -> Option<i64> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(number) = text.parse::<i64>() {
        return Some(number);
    }
    let mut hash: i32 = 0;
    for ch in text.chars() {
        // Java hashes UTF-16 code units, so a character outside the BMP
        // contributes its surrogate pair — `encode_utf16` is not decoration.
        let mut buffer = [0u16; 2];
        for unit in ch.encode_utf16(&mut buffer) {
            hash = hash.wrapping_mul(31).wrapping_add(*unit as i32);
        }
    }
    Some(hash as i64)
}

/// One dimension, exactly as `DimensionEnum` constructs it.
///
/// `height` and `chunkSectionCount` are derived in the Java constructor and
/// then serialised as plain fields, so they have to be written out rather than
/// left to a default — Gson populates fields directly and computes nothing.
fn dimension(id: u8) -> Value {
    let (name, min_height, max_height, sections) = match id {
        1 => ("minecraft:nether", 0, 127, 8),
        2 => ("minecraft:the_end", 0, 255, 16),
        _ => ("minecraft:overworld", -64, 319, 24),
    };
    // `if (minHeight <= 0 && maxHeight > 0) height += 1` — the y=0 layer counts.
    let mut height = max_height - min_height;
    if min_height <= 0 && max_height > 0 {
        height += 1;
    }
    json!({
        "dimensionName": name,
        "dimensionId": id,
        "minHeight": min_height,
        "maxHeight": max_height,
        "height": height,
        "chunkSectionCount": sections,
    })
}

/// A world's `config.json`, which is the only place a requested seed can go.
///
/// `Server.generateLevel` reads this file if it exists and ignores the config
/// it was passed; if it does not exist it invents a random seed. So this is
/// written **before the first launch, and never again** — after generation the
/// file describes a world that exists, and rewriting it with a different seed
/// would generate a second, different world into the same directory while the
/// first one is still on disk.
pub fn level_config(generator: &str, seed: i64) -> String {
    let entry = |index: u8, name: &str| {
        json!({
            "name": name,
            "seed": seed,
            "enableAntiXray": false,
            "antiXrayMode": "LOW",
            "preDeobfuscate": true,
            "dimensionData": dimension(index),
            "preset": {},
        })
    };
    let value = json!({
        "format": "leveldb",
        "enable": true,
        // The nether and the end are generated by their own generators
        // whatever the overworld is: `FLAT` means a flat overworld, not a flat
        // universe.
        "generators": {
            "0": entry(0, generator),
            "1": entry(1, "nether"),
            "2": entry(2, "the_end"),
        },
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

/// A YAML scalar, quoted when it has to be.
///
/// Only strings ever need it, and MOTDs are the reason: a server name that
/// starts with `#`, contains a `:` or is the word `no` all parse as something
/// other than themselves. Quoting unconditionally is simpler than deciding, and
/// okaeri reads a quoted scalar back identically.
fn yaml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn yaml_bool(value: bool) -> String {
    if value { "true" } else { "false" }.to_string()
}

/// One setting this host owns: which category, which key, and the rendered
/// YAML scalar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub category: &'static str,
    pub key: &'static str,
    pub value: String,
}

fn setting(category: &'static str, key: &'static str, value: String) -> Setting {
    Setting {
        category,
        key,
        value,
    }
}

/// The `pnx.yml` keys this host manages, in the order a fresh file gets them.
///
/// Everything not named here — and PowerNukkitX has well over a hundred keys —
/// is preserved verbatim by [`merge_settings`]. These are the ones the API can
/// express and the player expects to take effect.
pub fn managed(resolved: &Settings, port: u16, bind_address: &str) -> Vec<Setting> {
    vec![
        setting("settings", "ip", yaml_string(bind_address)),
        setting("settings", "port", port.to_string()),
        setting("settings", "max-players", resolved.max_players.to_string()),
        setting("settings", "motd", yaml_string(&resolved.motd)),
        setting(
            "settings",
            "allow-list",
            yaml_bool(resolved.whitelist_enabled),
        ),
        // Bedrock's online mode. Off means anyone can claim any gamertag, which
        // is what the API means by `ONLINE_MODE=false`.
        setting("settings", "xbox-auth", yaml_bool(resolved.online_mode)),
        setting(
            "settings",
            "default-level-name",
            yaml_string(DEFAULT_LEVEL_NAME),
        ),
        setting("settings", "language", yaml_string("eng")),
        setting(
            "gameplay-settings",
            "gamemode",
            gamemode_of(&resolved.game_mode).to_string(),
        ),
        setting(
            "gameplay-settings",
            "difficulty",
            difficulty_of(&resolved.difficulty).to_string(),
        ),
        setting(
            "gameplay-settings",
            "view-distance",
            resolved.view_distance.min(MAX_VIEW_DISTANCE).to_string(),
        ),
        setting("gameplay-settings", "pvp", yaml_bool(resolved.pvp)),
        setting(
            "gameplay-settings",
            "hardcore",
            yaml_bool(resolved.hardcore),
        ),
        setting(
            "gameplay-settings",
            "enable-command-blocks",
            yaml_bool(resolved.enable_command_blocks),
        ),
        // Bedrock's "tick distance" is the simulation radius, which Nukkit
        // calls the tick radius.
        setting(
            "chunk-settings",
            "tick-radius",
            resolved.simulation_distance.to_string(),
        ),
        // Forced, not defaulted. PowerNukkitX ships bStats-style metrics on,
        // and a player's phone does not report to a third party. The launch
        // also passes `-DdisableSentry=true`, because a config file a world
        // restore can overwrite is not a privacy guarantee.
        setting("misc-settings", "enable-metrics", yaml_bool(false)),
    ]
}

/// Apply [`Setting`]s to an existing `pnx.yml`, preserving everything else.
///
/// The same contract as [`crate::properties::merge`], one nesting level down:
/// comments, ordering, unknown keys and PowerNukkitX's own nested blocks
/// (`network-settings.rate-limit` and friends) all survive.
///
/// # How it avoids corrupting the file
///
/// A key is replaced only when it is at **exactly two spaces of indent** inside
/// the category it belongs to and already carries a scalar. That is what keeps
/// a nested block's children — four spaces in — from being mistaken for the
/// category's own keys, and what keeps a `rate-limit:` sub-block header from
/// being overwritten with a scalar.
///
/// A managed key the file does not have is inserted at the end of its
/// category's block rather than appended to the document, because a repeated
/// top-level key is a duplicate mapping key and SnakeYAML rejects the file
/// outright.
pub fn merge_settings(existing: &str, settings: &[Setting]) -> String {
    if existing.trim().is_empty() {
        return render_settings(settings);
    }

    let mut out: Vec<String> = Vec::new();
    let mut applied = vec![false; settings.len()];
    let mut category: Option<String> = None;
    // Where the current category's block ends, so a missing key can be put
    // inside it rather than after the whole document.
    let mut block_end = 0usize;

    let flush = |out: &mut Vec<String>,
                 applied: &mut Vec<bool>,
                 category: &Option<String>,
                 at: usize| {
        let Some(name) = category else { return };
        let mut insert: Vec<String> = Vec::new();
        for (index, setting) in settings.iter().enumerate() {
            if !applied[index] && setting.category == name {
                insert.push(format!("  {}: {}", setting.key, setting.value));
                applied[index] = true;
            }
        }
        for (offset, line) in insert.into_iter().enumerate() {
            out.insert(at + offset, line);
        }
    };

    for line in existing.lines() {
        if let Some(name) = top_level_key(line) {
            // Leaving a category: anything of ours it was missing goes in
            // before the next one starts.
            flush(&mut out, &mut applied, &category, block_end);
            category = Some(name.to_string());
            out.push(line.to_string());
            block_end = out.len();
            continue;
        }

        let mut replaced = false;
        if let (Some(name), Some((key, has_value))) = (category.as_deref(), nested_key(line)) {
            if has_value {
                for (index, setting) in settings.iter().enumerate() {
                    if !applied[index] && setting.category == name && setting.key == key {
                        out.push(format!("  {key}: {}", setting.value));
                        applied[index] = true;
                        replaced = true;
                        break;
                    }
                }
            }
        }
        if !replaced {
            out.push(line.to_string());
        }
        // A blank line or a trailing comment belongs after the block, not
        // inside it, so the insertion point stops at the last real key.
        if !line.trim().is_empty() && !line.trim_start().starts_with('#') {
            block_end = out.len();
        }
    }
    flush(&mut out, &mut applied, &category, block_end);

    // Categories the file has never heard of.
    let missing: Vec<&Setting> = settings
        .iter()
        .zip(&applied)
        .filter(|(_, done)| !**done)
        .map(|(setting, _)| setting)
        .collect();
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if !missing.is_empty() {
        let owned: Vec<Setting> = missing.into_iter().cloned().collect();
        text.push_str(&render_settings(&owned));
    }
    text
}

/// `name:` with nothing after it — a category header at the left margin.
fn top_level_key(line: &str) -> Option<&str> {
    if line.starts_with([' ', '\t', '#']) || line.trim().is_empty() {
        return None;
    }
    let (name, rest) = line.split_once(':')?;
    if !rest.trim().is_empty() {
        return None;
    }
    let name = name.trim_end();
    (!name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
    .then_some(name)
}

/// `  name: value` at exactly one level of indent. The bool is whether there is
/// a scalar after the colon; a sub-block header has none.
fn nested_key(line: &str) -> Option<(&str, bool)> {
    let rest = line.strip_prefix("  ")?;
    if rest.starts_with([' ', '\t']) || rest.starts_with('#') {
        return None;
    }
    let (name, value) = rest.split_once(':')?;
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        || name.is_empty()
    {
        return None;
    }
    Some((name, !value.trim().is_empty()))
}

/// A fresh file, or the tail of one: categories in first-seen order.
fn render_settings(settings: &[Setting]) -> String {
    let mut out = String::new();
    let mut seen: Vec<&str> = Vec::new();
    for setting in settings {
        if !seen.contains(&setting.category) {
            seen.push(setting.category);
        }
    }
    for category in seen {
        out.push_str(category);
        out.push_str(":\n");
        for setting in settings.iter().filter(|s| s.category == category) {
            out.push_str(&format!("  {}: {}\n", setting.key, setting.value));
        }
    }
    out
}

/// One name per line, which is what `Config.ENUM` reads.
fn name_list(names: &[String]) -> String {
    let mut out = String::new();
    for name in names {
        out.push_str(name);
        out.push('\n');
    }
    out
}

/// Nukkit's ban list: a JSON array, and not the shape a Java server writes into
/// a file of the same name.
///
/// Append-only for the same reason the Java one is — an in-game `/ban` lands
/// here and no sync ever sees it, so a rewrite would quietly unban whoever the
/// operator banned last night.
pub fn merge_banned(existing: &str, additions: &[String], created: &str) -> Option<String> {
    let mut entries: Vec<Value> = serde_json::from_str::<Vec<Value>>(existing.trim())
        .ok()
        .unwrap_or_default();

    let present: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry.get("name").and_then(|v| v.as_str()))
        .map(|name| name.to_ascii_lowercase())
        .collect();

    let mut added = false;
    for name in additions {
        if present.contains(&name.to_ascii_lowercase()) {
            continue;
        }
        let mut entry = Map::new();
        entry.insert("name".into(), json!(name));
        // `BanEntry.format` is `yyyy-MM-dd HH:mm:ss Z`, which is the same
        // format the Java list uses — so the host's clock string serves both.
        entry.insert("creationDate".into(), json!(created));
        entry.insert("source".into(), json!("Homerun"));
        entry.insert("expireDate".into(), json!("Forever"));
        entry.insert("reason".into(), json!("Banned by an operator."));
        entries.push(Value::Object(entry));
        added = true;
    }

    added.then(|| {
        serde_json::to_string_pretty(&Value::Array(entries)).unwrap_or_else(|_| "[]".to_string())
    })
}

/// Everything a PowerNukkitX launch writes.
pub fn config_files(ctx: &BuildContext) -> Result<Vec<FileWrite>> {
    // The loader is always vanilla here: nothing offers a `TYPE` for this game
    // type, and `serves` refuses one that arrived anyway.
    let mut resolved = settings::from_env(&ctx.env, &ctx.game_type, "vanilla", None);
    if let Some(name) = motd(&ctx.env) {
        resolved.motd = name;
    }

    let mut files = vec![FileWrite {
        path: SETTINGS_FILE.into(),
        contents: merge_settings(
            ctx.existing(SETTINGS_FILE),
            &managed(&resolved, ctx.port, &ctx.bind_address),
        ),
        encoding: Encoding::Utf8,
    }];

    // Plain names — a Bedrock identity is a gamertag and there is no UUID to
    // resolve. Nukkit matches these case-insensitively.
    files.push(FileWrite {
        path: OPS_FILE.into(),
        contents: name_list(&resolved.op_users),
        encoding: Encoding::Utf8,
    });
    files.push(FileWrite {
        path: ALLOWLIST_FILE.into(),
        contents: name_list(&resolved.whitelisted_users),
        encoding: Encoding::Utf8,
    });

    if let Some(contents) = merge_banned(
        ctx.existing(BANNED_FILE),
        &resolved.banned_users,
        &ctx.now,
    ) {
        files.push(FileWrite {
            path: BANNED_FILE.into(),
            contents,
            encoding: Encoding::Utf8,
        });
    }

    // Only when the player actually asked for something, and only when the
    // world does not exist yet. A `config.json` already on disk describes a
    // world that has been generated; replacing it generates a different one
    // into the same directory.
    let path = level_config_path(DEFAULT_LEVEL_NAME);
    let generator = generator_of(&resolved.world_type);
    let seed = seed_of(&resolved.seed);
    if ctx.existing(&path).trim().is_empty() && (seed.is_some() || generator != "normal") {
        files.push(FileWrite {
            path,
            // No seed asked for, but a generator was: a stable arbitrary value
            // beats a zero, which is a legitimate seed a player might have
            // wanted and would then never be able to ask for again.
            contents: level_config(generator, seed.unwrap_or(0)),
            encoding: Encoding::Utf8,
        });
    }

    Ok(files)
}

// ─── which release to run ───────────────────────────────────────────────────

/// Where the releases come from. The host makes the request; this reads it.
pub const RELEASES_URL: &str = "https://api.github.com/repos/PowerNukkitX/PowerNukkitX/releases";

/// The one asset a release publishes.
pub const ASSET: &str = "powernukkitx.jar";

/// Pick the release to run out of GitHub's `/releases` array.
///
/// # Why `blessed` exists
///
/// The jar is data, so a new PowerNukkitX release reaches players without a
/// store update — which is the point, and also the risk: nothing sits between
/// PowerNukkitX publishing and every phone running it. `blessed` is that
/// something. It is the tag the API says to use, so a release that eats worlds
/// is stopped by a field change on the server rather than by shipping a build
/// through store review.
///
/// `None` means the newest stable one, which is what a host with no answer from
/// the API falls back to.
///
/// # Ordering
///
/// By `published_at`, not by array position and not by reading the tag as a
/// version. GitHub happens to return newest-first, republishing a release
/// changes that, ISO-8601 sorts lexicographically, and a tag scheme belongs to
/// somebody else.
pub fn release(releases: &Value, blessed: Option<&str>) -> Result<Artifact> {
    let list = releases
        .as_array()
        .ok_or_else(|| Error::Malformed("the release list is not a list".into()))?;

    let wanted = blessed.map(normalise_tag);
    let mut best: Option<(&str, &Value)> = None;
    for entry in list {
        let tag = entry.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
        if tag.is_empty() {
            continue;
        }
        match &wanted {
            Some(pin) => {
                if normalise_tag(tag) != *pin {
                    continue;
                }
            }
            None => {
                // A draft has no downloadable asset, and a prerelease is not
                // something a world should land on by accident.
                if entry.get("draft").and_then(|v| v.as_bool()) == Some(true)
                    || entry.get("prerelease").and_then(|v| v.as_bool()) == Some(true)
                {
                    continue;
                }
            }
        }
        let published = entry
            .get("published_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if best.is_none() || best.is_some_and(|(seen, _)| published > seen) {
            best = Some((published, entry));
        }
    }

    let Some((_, entry)) = best else {
        return Err(Error::Malformed(match blessed {
            Some(tag) => format!("PowerNukkitX has no release {tag}"),
            None => "PowerNukkitX has published no releases".into(),
        }));
    };

    let tag = entry
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let asset = entry
        .get("assets")
        .and_then(|v| v.as_array())
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a.get("name").and_then(|v| v.as_str()) == Some(ASSET))
        })
        .ok_or_else(|| Error::Malformed(format!("PowerNukkitX {tag} publishes no {ASSET}")))?;

    let url = asset
        .get("browser_download_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Malformed(format!("PowerNukkitX {tag} has no download link")))?;

    Ok(Artifact {
        url: url.to_string(),
        loader: LOADER.to_string(),
        version: normalise_tag(tag),
        // GitHub publishes `sha256:<hex>` on an asset once it has computed one.
        // Absent is not an error: `jar::cache_decision` refetches what it cannot
        // prove rather than trusting a marker, which is the right answer.
        checksum: asset
            .get("digest")
            .and_then(|v| v.as_str())
            .and_then(|d| d.strip_prefix("sha256:"))
            .filter(|hex| !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
            .map(|hex| Checksum {
                algorithm: Algorithm::Sha256,
                hex: hex.to_ascii_lowercase(),
            }),
        required_java: REQUIRED_JAVA,
        size_bytes: asset.get("size").and_then(|v| v.as_u64()),
    })
}

/// `v3.0.3` and `3.0.3` are the same release. The stored form drops the `v`, so
/// a tag scheme that gains or loses one does not orphan every cached jar.
fn normalise_tag(tag: &str) -> String {
    let trimmed = tag.trim();
    trimmed.strip_prefix('v').unwrap_or(trimmed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(env: Value) -> BuildContext {
        BuildContext {
            env,
            game_type: "native-powernukkitx".into(),
            port: 19140,
            bind_address: "127.0.0.1".into(),
            existing: Default::default(),
            resolved: vec![],
            now: "2026-08-21 09:15:00 +0000".into(),
        }
    }

    fn written<'a>(files: &'a [FileWrite], path: &str) -> Option<&'a FileWrite> {
        files.iter().find(|f| f.path == path)
    }

    // ─── the file the wizard checks for ─────────────────────────────────────

    /// If this ever becomes `server-settings.yml` again, the setup wizard runs,
    /// reads a licence answer off stdin, and a phone sits at `starting`
    /// forever.
    #[test]
    fn the_settings_file_is_the_one_the_wizard_looks_for() {
        assert_eq!(SETTINGS_FILE, "pnx.yml");
    }

    #[test]
    fn a_launch_writes_settings_ops_and_the_allowlist() {
        let files = config_files(&ctx(json!({}))).unwrap();
        assert!(written(&files, SETTINGS_FILE).is_some());
        assert!(written(&files, OPS_FILE).is_some());
        assert!(written(&files, ALLOWLIST_FILE).is_some());
    }

    /// There is no Mojang EULA in this game type, and writing one would be a
    /// file PowerNukkitX ignores.
    #[test]
    fn no_eula_and_no_server_properties() {
        let files = config_files(&ctx(json!({}))).unwrap();
        assert!(written(&files, "eula.txt").is_none());
        assert!(written(&files, "server.properties").is_none());
    }

    // ─── settings mapping ───────────────────────────────────────────────────

    #[test]
    fn the_port_and_bind_address_come_from_the_host() {
        let files = config_files(&ctx(json!({}))).unwrap();
        let yaml = &written(&files, SETTINGS_FILE).unwrap().contents;
        assert!(yaml.contains("  port: 19140"), "{yaml}");
        assert!(yaml.contains("  ip: \"127.0.0.1\""), "{yaml}");
    }

    #[test]
    fn the_server_name_becomes_the_motd() {
        let files = config_files(&ctx(json!({ "SERVER_NAME": "Ada's world" }))).unwrap();
        let yaml = &written(&files, SETTINGS_FILE).unwrap().contents;
        assert!(yaml.contains("  motd: \"Ada's world\""), "{yaml}");
    }

    /// A name that would parse as something else if it were not quoted.
    #[test]
    fn a_motd_that_looks_like_yaml_survives() {
        let files = config_files(&ctx(json!({ "SERVER_NAME": "no: #1 \"best\"" }))).unwrap();
        let yaml = &written(&files, SETTINGS_FILE).unwrap().contents;
        assert!(yaml.contains(r#"  motd: "no: #1 \"best\"""#), "{yaml}");
    }

    #[test]
    fn gamemode_and_difficulty_are_numbers() {
        let files =
            config_files(&ctx(json!({ "GAMEMODE": "creative", "DIFFICULTY": "hard" }))).unwrap();
        let yaml = &written(&files, SETTINGS_FILE).unwrap().contents;
        assert!(yaml.contains("  gamemode: 1"), "{yaml}");
        assert!(yaml.contains("  difficulty: 3"), "{yaml}");
    }

    #[test]
    fn every_wizard_gamemode_maps() {
        assert_eq!(gamemode_of("survival"), 0);
        assert_eq!(gamemode_of("creative"), 1);
        assert_eq!(gamemode_of("adventure"), 2);
        // Not offered by the wizard, but a server made elsewhere can carry it.
        assert_eq!(gamemode_of("spectator"), 3);
    }

    #[test]
    fn every_wizard_difficulty_maps() {
        assert_eq!(difficulty_of("peaceful"), 0);
        assert_eq!(difficulty_of("easy"), 1);
        assert_eq!(difficulty_of("normal"), 2);
        assert_eq!(difficulty_of("hard"), 3);
    }

    /// A spelling nobody recognises must not silently put everyone in creative.
    #[test]
    fn an_unknown_gamemode_is_survival() {
        assert_eq!(gamemode_of("sUrVivAl"), 0);
        assert_eq!(gamemode_of("kitchen sink"), 0);
    }

    #[test]
    fn online_mode_is_xbox_auth() {
        let on = config_files(&ctx(json!({}))).unwrap();
        assert!(written(&on, SETTINGS_FILE)
            .unwrap()
            .contents
            .contains("  xbox-auth: true"));

        let off = config_files(&ctx(json!({ "ONLINE_MODE": "false" }))).unwrap();
        assert!(written(&off, SETTINGS_FILE)
            .unwrap()
            .contents
            .contains("  xbox-auth: false"));
    }

    /// The wizard offers up to 64 chunks. A phone cannot afford it.
    #[test]
    fn view_distance_is_clamped_for_a_phone() {
        let files = config_files(&ctx(json!({ "VIEW_DISTANCE": "64" }))).unwrap();
        let yaml = &written(&files, SETTINGS_FILE).unwrap().contents;
        assert!(
            yaml.contains(&format!("  view-distance: {MAX_VIEW_DISTANCE}")),
            "{yaml}"
        );
    }

    #[test]
    fn tick_distance_becomes_the_tick_radius() {
        let files = config_files(&ctx(json!({ "TICK_DISTANCE": "8" }))).unwrap();
        let yaml = &written(&files, SETTINGS_FILE).unwrap().contents;
        assert!(yaml.contains("  tick-radius: 8"), "{yaml}");
    }

    /// Not a default we happen to agree with — a promise. See `managed`.
    #[test]
    fn metrics_are_forced_off() {
        let files = config_files(&ctx(json!({}))).unwrap();
        let yaml = &written(&files, SETTINGS_FILE).unwrap().contents;
        assert!(yaml.contains("  enable-metrics: false"), "{yaml}");
    }

    // ─── the merge ──────────────────────────────────────────────────────────

    #[test]
    fn an_existing_value_is_replaced_in_place() {
        let existing = "settings:\n  motd: \"old\"\n  port: 19132\n";
        let merged = merge_settings(
            existing,
            &[setting("settings", "motd", yaml_string("new"))],
        );
        assert_eq!(merged, "settings:\n  motd: \"new\"\n  port: 19132\n");
    }

    /// PowerNukkitX writes comments into this file on every save, and a player
    /// may have added their own.
    #[test]
    fn comments_and_unknown_keys_survive() {
        let existing = "# PowerNukkitX\nsettings:\n  # what players see\n  motd: \"old\"\n  \
                        force-server-translate: false\n";
        let merged = merge_settings(
            existing,
            &[setting("settings", "motd", yaml_string("new"))],
        );
        assert!(merged.contains("# PowerNukkitX"));
        assert!(merged.contains("  # what players see"));
        assert!(merged.contains("  force-server-translate: false"));
        assert!(merged.contains("  motd: \"new\""));
    }

    /// `network-settings.rate-limit` is a block, and its children are two
    /// levels in. Mistaking one for a key of ours would rewrite it as a scalar
    /// and take the whole file down with it.
    #[test]
    fn a_nested_block_is_left_alone() {
        let existing = "network-settings:\n  snappy: false\n  rate-limit:\n    enabled: true\n    \
                        snappy: true\n";
        let merged = merge_settings(
            existing,
            &[setting("network-settings", "snappy", "true".into())],
        );
        assert_eq!(
            merged,
            "network-settings:\n  snappy: true\n  rate-limit:\n    enabled: true\n    \
             snappy: true\n"
        );
    }

    /// A repeated top-level key is a duplicate mapping key, which SnakeYAML
    /// rejects outright — so a missing key goes inside the block it belongs to.
    #[test]
    fn a_missing_key_is_inserted_into_its_own_category() {
        let existing = "settings:\n  motd: \"hi\"\n\ngameplay-settings:\n  pvp: true\n";
        let merged = merge_settings(
            existing,
            &[setting("settings", "port", "19140".into())],
        );
        assert_eq!(
            merged,
            "settings:\n  motd: \"hi\"\n  port: 19140\n\ngameplay-settings:\n  pvp: true\n"
        );
        assert_eq!(merged.matches("settings:").count(), 2, "{merged}");
    }

    #[test]
    fn a_category_the_file_lacks_is_appended() {
        let existing = "settings:\n  motd: \"hi\"\n";
        let merged = merge_settings(
            existing,
            &[setting("misc-settings", "enable-metrics", "false".into())],
        );
        assert_eq!(
            merged,
            "settings:\n  motd: \"hi\"\nmisc-settings:\n  enable-metrics: false\n"
        );
    }

    #[test]
    fn an_empty_file_gets_the_whole_document() {
        let rendered = merge_settings("", &managed(&blank(), 19132, "0.0.0.0"));
        assert!(rendered.starts_with("settings:\n"));
        assert!(rendered.contains("gameplay-settings:\n"));
        assert!(rendered.contains("misc-settings:\n"));
    }

    fn blank() -> Settings {
        settings::from_env(&json!({}), "native-powernukkitx", "vanilla", None)
    }

    // ─── identities ─────────────────────────────────────────────────────────

    /// The whole reason this is not the Java path: a gamertag has no Mojang
    /// UUID, so asking for one wastes a request and writes an id that matches
    /// nobody.
    #[test]
    fn a_bedrock_server_looks_nobody_up() {
        assert!(required_lookups().is_empty());
    }

    #[test]
    fn ops_and_the_allowlist_are_plain_names() {
        let files = config_files(&ctx(json!({
            "OPS": "Ada,Grace",
            "WHITELIST": "Ada",
        })))
        .unwrap();
        assert_eq!(written(&files, OPS_FILE).unwrap().contents, "Ada\nGrace\n");
        assert_eq!(written(&files, ALLOWLIST_FILE).unwrap().contents, "Ada\n");
    }

    #[test]
    fn bans_are_appended_not_replaced() {
        let existing = r#"[{"name":"Mallory","creationDate":"2026-01-01 00:00:00 +0000",
            "source":"console","expireDate":"Forever","reason":"griefing"}]"#;
        let merged = merge_banned(
            existing,
            &["Eve".to_string()],
            "2026-08-21 09:15:00 +0000",
        )
        .unwrap();
        assert!(merged.contains("Mallory"), "{merged}");
        assert!(merged.contains("griefing"), "{merged}");
        assert!(merged.contains("Eve"), "{merged}");
    }

    #[test]
    fn a_ban_that_is_already_there_writes_nothing() {
        let existing = r#"[{"name":"Eve","creationDate":"2026-01-01 00:00:00 +0000",
            "source":"console","expireDate":"Forever","reason":"x"}]"#;
        assert!(merge_banned(existing, &["eve".to_string()], "now").is_none());
    }

    // ─── the world ──────────────────────────────────────────────────────────

    #[test]
    fn a_plain_launch_writes_no_level_config() {
        let files = config_files(&ctx(json!({}))).unwrap();
        assert!(written(&files, &level_config_path(DEFAULT_LEVEL_NAME)).is_none());
    }

    #[test]
    fn a_seed_is_written_into_the_worlds_config() {
        let files = config_files(&ctx(json!({ "LEVEL_SEED": "12345" }))).unwrap();
        let config = written(&files, &level_config_path(DEFAULT_LEVEL_NAME)).unwrap();
        let value: Value = serde_json::from_str(&config.contents).unwrap();
        assert_eq!(value["generators"]["0"]["seed"], 12345);
        assert_eq!(value["generators"]["0"]["name"], "normal");
        assert_eq!(value["format"], "leveldb");
    }

    /// A flat overworld, not a flat universe.
    #[test]
    fn flat_only_changes_the_overworld() {
        let files = config_files(&ctx(json!({ "LEVEL_TYPE": "FLAT" }))).unwrap();
        let config = written(&files, &level_config_path(DEFAULT_LEVEL_NAME)).unwrap();
        let value: Value = serde_json::from_str(&config.contents).unwrap();
        assert_eq!(value["generators"]["0"]["name"], "flat");
        assert_eq!(value["generators"]["1"]["name"], "nether");
        assert_eq!(value["generators"]["2"]["name"], "the_end");
    }

    #[test]
    fn every_wizard_level_type_maps() {
        assert_eq!(generator_of("DEFAULT"), "normal");
        assert_eq!(generator_of("FLAT"), "flat");
        // No PowerNukkitX equivalent: an ordinary world, documented as such.
        assert_eq!(generator_of("LEGACY"), "normal");
    }

    /// The world already exists. Rewriting this file with a freshly-derived
    /// seed would generate a second, different world into the same directory.
    #[test]
    fn an_existing_world_config_is_never_overwritten() {
        let mut context = ctx(json!({ "LEVEL_SEED": "999" }));
        context.existing.insert(
            level_config_path(DEFAULT_LEVEL_NAME),
            r#"{"format":"leveldb"}"#.into(),
        );
        let files = config_files(&context).unwrap();
        assert!(written(&files, &level_config_path(DEFAULT_LEVEL_NAME)).is_none());
    }

    #[test]
    fn a_worded_seed_hashes_the_way_java_does() {
        // `"hello".hashCode()` in Java is 99162322 — the canonical worked
        // example, so this checks the algorithm and not just itself.
        assert_eq!(seed_of("hello"), Some(99_162_322));
        assert_eq!(seed_of("homerun"), Some(1_092_724_044));
        assert_eq!(seed_of("-42"), Some(-42));
        assert_eq!(seed_of("  "), None);
    }

    #[test]
    fn the_overworld_is_the_dimension_powernukkitx_expects() {
        let value = dimension(0);
        assert_eq!(value["dimensionName"], "minecraft:overworld");
        assert_eq!(value["minHeight"], -64);
        assert_eq!(value["maxHeight"], 319);
        // 319 - (-64) + 1, because y=0 counts.
        assert_eq!(value["height"], 384);
        assert_eq!(value["chunkSectionCount"], 24);
    }

    #[test]
    fn the_config_asks_for_what_it_must_not_clobber() {
        let paths: Vec<String> = config_inputs().into_iter().map(|i| i.path).collect();
        assert!(paths.contains(&SETTINGS_FILE.to_string()));
        assert!(paths.contains(&level_config_path(DEFAULT_LEVEL_NAME)));
        assert!(paths.contains(&BANNED_FILE.to_string()));
    }

    // ─── which release to run ───────────────────────────────────────────────

    fn releases() -> Value {
        json!([
            {
                "tag_name": "v3.0.4-beta",
                "prerelease": true,
                "published_at": "2026-08-20T10:00:00Z",
                "assets": [{ "name": ASSET, "browser_download_url": "https://x/beta.jar" }],
            },
            {
                "tag_name": "v3.0.3",
                "prerelease": false,
                "draft": false,
                "published_at": "2026-08-14T10:00:00Z",
                "assets": [{
                    "name": ASSET,
                    "browser_download_url": "https://x/3.0.3.jar",
                    "size": 60139230,
                    "digest": "sha256:AABBCC",
                }],
            },
            {
                "tag_name": "v3.0.2",
                "prerelease": false,
                "draft": false,
                "published_at": "2026-07-01T10:00:00Z",
                "assets": [{ "name": ASSET, "browser_download_url": "https://x/3.0.2.jar" }],
            },
        ])
    }

    #[test]
    fn the_newest_stable_release_wins() {
        let artifact = release(&releases(), None).unwrap();
        assert_eq!(artifact.version, "3.0.3");
        assert_eq!(artifact.url, "https://x/3.0.3.jar");
        assert_eq!(artifact.loader, LOADER);
        assert_eq!(artifact.required_java, REQUIRED_JAVA);
        assert_eq!(artifact.size_bytes, Some(60139230));
    }

    /// A prerelease is newer, and must not be what a world lands on.
    #[test]
    fn a_prerelease_is_not_picked_by_accident() {
        assert_eq!(release(&releases(), None).unwrap().version, "3.0.3");
    }

    /// The safety valve: the API names the release, every phone picks it up on
    /// its next launch, and no build goes through store review.
    #[test]
    fn the_api_can_pin_an_older_release() {
        let artifact = release(&releases(), Some("3.0.2")).unwrap();
        assert_eq!(artifact.version, "3.0.2");
    }

    /// Deliberately, so a pin can name a prerelease when that is the fix.
    #[test]
    fn a_pin_may_name_a_prerelease() {
        assert_eq!(
            release(&releases(), Some("v3.0.4-beta")).unwrap().version,
            "3.0.4-beta"
        );
    }

    #[test]
    fn a_pin_that_does_not_exist_says_so() {
        let error = release(&releases(), Some("9.9.9")).unwrap_err().to_string();
        assert!(error.contains("9.9.9"), "{error}");
    }

    #[test]
    fn the_leading_v_is_not_part_of_the_version() {
        assert_eq!(
            release(&releases(), Some("v3.0.3")).unwrap().version,
            "3.0.3"
        );
    }

    #[test]
    fn a_digest_becomes_a_checksum_the_cache_can_key_on() {
        let artifact = release(&releases(), None).unwrap();
        assert_eq!(artifact.checksum.as_ref().unwrap().hex, "aabbcc");
        assert!(super::super::jar::cache_key(&artifact).is_some());
    }

    /// Not an error: the cache refetches what it cannot prove.
    #[test]
    fn a_release_with_no_digest_still_resolves() {
        assert!(release(&releases(), Some("3.0.2"))
            .unwrap()
            .checksum
            .is_none());
    }

    #[test]
    fn a_release_with_no_jar_is_named_in_the_error() {
        let empty = json!([{
            "tag_name": "3.1.0",
            "published_at": "2026-09-01T10:00:00Z",
            "assets": [{ "name": "source.zip", "browser_download_url": "https://x/s.zip" }],
        }]);
        let error = release(&empty, None).unwrap_err().to_string();
        assert!(error.contains("3.1.0"), "{error}");
    }

}

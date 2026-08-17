//! Minigame servers: our own plugin jars, the env they read, and what makes
//! one of these servers different from a world somebody lives in.
//!
//! # What a minigame server is
//!
//! A public Paper server created from a template in the Games browser. Three
//! things arrive in its `environment_variables` and nothing else marks it:
//!
//! - `MINIGAME` — the template id, and the flag every decision here keys on.
//! - `CUSTOM_PLUGINS` — the jars that make it a game. They are ours, they are
//!   not on Modrinth, and [`crate::minecraft::mods`] will never fetch them.
//! - `MINIGAME_*` / `BEDWARS_*` — settings the plugin reads at runtime.
//!
//! # Why the host cannot work any of this out for itself
//!
//! It did, on the desktop, in three places — and the phone would have had to
//! be a fourth. The filename rule in particular is the kind of thing that
//! looks local and is not: see [`custom_plugins`] for why two hosts naming the
//! same jar differently breaks a server on the *second* device a player uses.
//!
//! # Ephemeral
//!
//! A lobby is not a world. It is generated for one session, nobody's building
//! is in it, and the API soft-deletes the server record when it stops. So a
//! minigame server is not backed up, not restored, and its directory is
//! deleted once it exits — [`is_minigame`] is what a host asks before doing
//! any of those three. On a phone that last point is not a tidiness
//! preference: a Paper server with a generated world is a gigabyte or two, and
//! a player who hosts a few games in an evening would otherwise fill a device
//! that never offered them a way to notice.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The jars that make a server a game. Newline- or comma-separated URLs.
pub const ENV_CUSTOM_PLUGINS: &str = "CUSTOM_PLUGINS";
/// The template id. Present iff this is a minigame server.
pub const ENV_MINIGAME: &str = "MINIGAME";

/// Prefixes of the env keys our own plugins read at runtime.
///
/// A prefix rather than a list because a template's mode injects keys this
/// crate has never heard of (`BEDWARS_TEAM_SIZE` and friends), and a list would
/// have to be edited every time the catalog gained one — silently dropping the
/// new key until somebody noticed. The namespace is ours, so forwarding all of
/// it is both safe and complete.
const FORWARDED_PREFIXES: [&str; 2] = ["MINIGAME", "BEDWARS"];

/// Loaders that load a jar dropped in `plugins/`.
///
/// All three, not the two a phone can host: this crate answers for the desktop
/// as well, and [`crate::minecraft::jar::Loader::hostable`] is the separate
/// question of what *this build* will start. A host that cannot run Spigot
/// never reaches here with `"spigot"`.
const PLUGIN_LOADERS: [&str; 3] = ["paper", "spigot", "bukkit"];

/// One jar to fetch, and the name to give it on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomPlugin {
    /// Fetched verbatim, every launch. It is a *stable resolve endpoint* that
    /// redirects to the newest jar on the server's release channel, so
    /// re-fetching is what makes a plugin update arrive without the player
    /// recreating anything.
    pub url: String,
    /// Where it goes inside `plugins/`. Never a path — see [`custom_plugins`].
    pub filename: String,
}

/// Is this a minigame server?
///
/// JavaScript truthiness on `MINIGAME`, because the desktop's test is `!!
/// env.MINIGAME` and a server that disagrees about this between platforms
/// would be backed up on one and deleted on the other.
pub fn is_minigame(env: &Value) -> bool {
    env.get(ENV_MINIGAME).is_some_and(truthy)
}

/// The env our own plugins read, curated out of the server's settings.
///
/// # Why any of this is needed
///
/// A server's `environment_variables` reach the world by being written into
/// `server.properties` and friends; **they are not the JVM's environment**.
/// The supervisor spawns Java with its own, so `System.getenv("MINIGAME_MIN_PLAYERS")`
/// inside the plugin sees nothing at all — the host chose a player count in
/// the UI and the match started with the plugin's built-in default of two.
///
/// # Why it is curated rather than forwarded wholesale
///
/// The server's env is not a safe thing to hand a subprocess. It is whatever
/// the dashboard holds for this server, it is fetched with the user's token,
/// and the rule the rest of this host is built around — nothing secret reaches
/// the server process's environment — would be exactly reversed by passing it
/// all through. Two prefixes we own is the whole of what the plugins read.
///
/// Sorted, so a launch is reproducible and a diff of two launches means
/// something.
pub fn forwarded_env(env: &Value) -> BTreeMap<String, String> {
    let Some(map) = env.as_object() else {
        return BTreeMap::new();
    };

    map.iter()
        .filter(|(key, _)| FORWARDED_PREFIXES.iter().any(|p| key.starts_with(p)))
        .filter_map(|(key, value)| Some((key.clone(), as_env_string(value)?)))
        .collect()
}

/// The jars to fetch into `plugins/`, in the order the catalog listed them.
///
/// Empty for a loader that would never load them — a jar dropped in `plugins/`
/// on a Fabric server is simply ignored, and downloading it would be a phone's
/// data spent on nothing.
///
/// # The filename, and why it is not a local decision
///
/// The rule is the desktop's, reproduced exactly:
///
/// 1. the last segment of the URL path, percent-decoded and trimmed;
/// 2. `.jar` appended if it does not already end that way;
/// 3. and if that segment is empty or the URL will not parse,
///    `homerun-plugin-<first 12 hex of sha1(url)>.jar`.
///
/// Step 3 reads like a fallback and is in fact the normal path: every URL the
/// API hands out is `…/api/minigame/plugins/<slug>/download/?channel=…`, whose
/// last path segment is empty because of the trailing slash. So our jars are
/// all hash-named on every platform today. That is worth knowing before
/// anybody "fixes" it — the slug is right there in the path and would make a
/// far better filename, but changing the rule is a change both platforms have
/// to make in the same release. See [`crate::sha1`] for what goes wrong when
/// they disagree.
///
/// # One deliberate divergence, and it is a hole
///
/// A URL is attacker-influenced: `CUSTOM_PLUGINS` is read from the server's
/// `environment_variables`, which anyone who can edit the server on the web
/// dashboard can set. The desktop hands the decoded segment straight to
/// `path.join`, so a URL ending `..%2F..%2Fplugins.jar` decodes to
/// `../../plugins.jar` and writes outside the plugins directory — a file write
/// at a path of the sender's choosing, which on desktop is a file write
/// anywhere the app can reach.
///
/// So a name that is not a plain filename is refused here and falls to the
/// hash. This changes the answer only for inputs that are attacks: a name
/// containing `/`, `\`, a NUL, or that is `.`/`..` is not a filename anybody
/// meant. Reported upstream rather than only fixed here.
pub fn custom_plugins(loader: &str, env: &Value) -> Vec<CustomPlugin> {
    if !PLUGIN_LOADERS.contains(&loader) {
        return Vec::new();
    }

    let Some(raw) = env.get(ENV_CUSTOM_PLUGINS).and_then(Value::as_str) else {
        return Vec::new();
    };

    let mut plugins: Vec<CustomPlugin> = Vec::new();
    for url in raw
        .split(['\n', ','])
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let filename = plugin_filename(url);
        // The catalog listing one plugin twice would otherwise be a jar
        // fetched twice and written over itself. Same directory either way, so
        // this is unobservable except in what it costs a phone to do.
        if plugins.iter().any(|p| p.filename == filename) {
            continue;
        }
        plugins.push(CustomPlugin {
            url: url.to_string(),
            filename,
        });
    }
    plugins
}

/// `spigot.yml`, which a host reads before a launch so this can decide.
pub const SPIGOT_YML: &str = "spigot.yml";

/// `spigot.yml` disabling every advancement, or `None` to leave it alone.
///
/// # Why a minigame turns them off
///
/// A player dropped into a BedWars lobby gets "Advancement Made! Stone Age"
/// popping over the game. The `announceAdvancements` gamerule only silences
/// the chat line; disabling them at the server level stops the toast too,
/// because a disabled advancement is never granted. It is boot-time config, so
/// it has to be on disk before the JVM starts — the plugin cannot do it later.
///
/// # Why only when the file is absent
///
/// The desktop parses the YAML and merges one key into it. This does not,
/// because a YAML parser is a dependency this crate will not take for one
/// key, and writing the file wholesale would discard everything Paper had
/// generated in it.
///
/// Absent is not a rare case, though — it is the normal one. A minigame
/// server's directory is deleted when it stops, so every launch is a fresh
/// directory with no `spigot.yml` in it; we write ours, Paper reads it and
/// rewrites the file with its own defaults merged *around* our block. The one
/// case this skips is a directory that survived — a delete that failed, or a
/// server that became a minigame after running as something else — where
/// leaving Paper's file intact is the right answer anyway.
pub fn disable_advancements(env: &Value, existing: &str) -> Option<String> {
    if !is_minigame(env) || !existing.trim().is_empty() {
        return None;
    }
    // `'*'` disables every advancement at once — no "and its children"
    // caveat, and no console error for an id that does not exist.
    Some("advancements:\n  disable-saving: true\n  disabled:\n  - '*'\n".to_string())
}

// ---------------------------------------------------------------------------
// The filename rule
// ---------------------------------------------------------------------------

fn plugin_filename(url: &str) -> String {
    match url_path_basename(url) {
        Some(base) if is_plain_filename(&base) => {
            if base.to_ascii_lowercase().ends_with(".jar") {
                base
            } else {
                format!("{base}.jar")
            }
        }
        _ => format!(
            "homerun-plugin-{}.jar",
            &crate::sha1::hex(url.as_bytes())[..12]
        ),
    }
}

/// A name that names a file in this directory and nothing else.
fn is_plain_filename(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// The last path segment of `url`, percent-decoded and trimmed.
///
/// `None` where `new URL(url)` would throw or `decodeURIComponent` would: no
/// scheme, no authority, or a broken percent-escape. Deliberately not a WHATWG
/// URL parser — it is asked one question, about URLs this codebase generates,
/// and every shape it declines to parse ends at the same hashed name the
/// desktop would produce for it.
fn url_path_basename(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || !scheme.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }

    // Authority runs to the path, the query, or the fragment — whichever
    // comes first. An empty one is not a URL any host would fetch.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if authority_end == 0 {
        return None;
    }

    let path = match rest[authority_end..].chars().next() {
        Some('/') => {
            let after = &rest[authority_end..];
            &after[..after.find(['?', '#']).unwrap_or(after.len())]
        }
        // No path at all: `https://host?q`. The basename is empty, which is
        // the hashed name, so say so rather than pretending it parsed.
        _ => return Some(String::new()),
    };

    Some(
        percent_decode(path.rsplit('/').next().unwrap_or(""))?
            .trim()
            .to_string(),
    )
}

/// `decodeURIComponent`, minus the parts nothing here can reach. `None` for a
/// malformed escape or bytes that are not UTF-8 — both of which throw in
/// JavaScript, and both of which end at the hashed name.
fn percent_decode(segment: &str) -> Option<String> {
    if !segment.contains('%') {
        return Some(segment.to_string());
    }

    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(text, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---------------------------------------------------------------------------
// JavaScript semantics, reproduced on purpose
// ---------------------------------------------------------------------------

/// `!!value`, for the JSON shapes an env can hold.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        // `!!{}` and `!![]` are both true in JavaScript. Neither is a shape an
        // env var arrives in, but agreeing costs nothing.
        _ => true,
    }
}

/// `String(value)` for an env var, or `None` for a value that is not one.
///
/// The desktop forwards any non-null value through `String()`, which turns an
/// object into the literal text `[object Object]`. That is not a value worth
/// reproducing, and an env var is never an object or an array — so those are
/// dropped here instead, and a plugin sees the key absent rather than
/// nonsense.
fn as_env_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        // `String(5)` is `"5"`, not `"5.0"`.
        Value::Number(n) => Some(match n.as_i64() {
            Some(i) => i.to_string(),
            None => n.to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    /// The URL the API actually hands out. Its trailing slash is why the hash
    /// path is the normal path, and this is the test that says so out loud.
    const RESOLVE_URL: &str =
        "https://api.gethomerun.app/api/minigame/plugins/homerun-minigames/download/?channel=release";

    #[test]
    fn a_real_resolve_url_is_named_by_its_hash_because_its_path_ends_in_a_slash() {
        let plugins = custom_plugins("paper", &env(&[("CUSTOM_PLUGINS", json!(RESOLVE_URL))]));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].url, RESOLVE_URL);
        // Pinned against the desktop's own digest — see `sha1`'s tests.
        assert_eq!(plugins[0].filename, "homerun-plugin-dafc06fcd9c7.jar");
    }

    #[test]
    fn a_url_naming_a_jar_keeps_that_name() {
        let plugins = custom_plugins(
            "paper",
            &env(&[(
                "CUSTOM_PLUGINS",
                json!("https://cdn.example/BedWars-1.2.3.jar"),
            )]),
        );
        assert_eq!(plugins[0].filename, "BedWars-1.2.3.jar");
    }

    #[test]
    fn a_last_segment_that_is_not_a_jar_gains_the_extension() {
        let plugins = custom_plugins(
            "paper",
            &env(&[(
                "CUSTOM_PLUGINS",
                json!("https://cdn.example/downloads/lobby"),
            )]),
        );
        assert_eq!(plugins[0].filename, "lobby.jar");
    }

    #[test]
    fn an_encoded_space_is_decoded_the_way_the_desktop_decodes_it() {
        let plugins = custom_plugins(
            "paper",
            &env(&[(
                "CUSTOM_PLUGINS",
                json!("https://cdn.example/My%20Plugin.jar"),
            )]),
        );
        assert_eq!(plugins[0].filename, "My Plugin.jar");
    }

    /// The whole reason the plain-filename check exists. Without it this
    /// decodes to `../../owned.jar` and the host writes outside `plugins/`.
    #[test]
    fn a_url_encoding_a_path_traversal_falls_back_to_the_hash() {
        for hostile in [
            "https://cdn.example/a/..%2F..%2Fowned.jar",
            "https://cdn.example/a/%2E%2E%2Fowned.jar",
            "https://cdn.example/a/..%5Cowned.jar",
            "https://cdn.example/a/%2E%2E",
        ] {
            let plugins = custom_plugins("paper", &env(&[("CUSTOM_PLUGINS", json!(hostile))]));
            let name = &plugins[0].filename;
            assert!(
                name.starts_with("homerun-plugin-") && name.ends_with(".jar"),
                "{hostile} produced {name}, which is not a plain filename",
            );
            assert!(!name.contains(['/', '\\']), "{name} escapes plugins/");
        }
    }

    #[test]
    fn several_urls_split_on_newlines_and_commas_alike() {
        let plugins = custom_plugins(
            "paper",
            &env(&[(
                "CUSTOM_PLUGINS",
                json!("https://cdn.example/a.jar\n  https://cdn.example/b.jar , https://cdn.example/c.jar\n\n"),
            )]),
        );
        assert_eq!(
            plugins
                .iter()
                .map(|p| p.filename.as_str())
                .collect::<Vec<_>>(),
            ["a.jar", "b.jar", "c.jar"],
        );
    }

    #[test]
    fn the_same_jar_listed_twice_is_fetched_once() {
        let plugins = custom_plugins(
            "paper",
            &env(&[(
                "CUSTOM_PLUGINS",
                json!("https://cdn.example/a.jar\nhttps://cdn.example/a.jar"),
            )]),
        );
        assert_eq!(plugins.len(), 1);
    }

    #[test]
    fn something_that_is_not_a_url_still_gets_a_stable_name() {
        let plugins = custom_plugins(
            "paper",
            &env(&[("CUSTOM_PLUGINS", json!("not a url at all"))]),
        );
        // The desktop's digest of the same token — `sha1`'s tests pin it.
        assert_eq!(plugins[0].filename, "homerun-plugin-1edc88d0873a.jar");
    }

    #[test]
    fn only_loaders_that_read_the_plugins_directory_are_given_any() {
        let e = env(&[("CUSTOM_PLUGINS", json!("https://cdn.example/a.jar"))]);
        for loader in PLUGIN_LOADERS {
            assert_eq!(custom_plugins(loader, &e).len(), 1, "{loader}");
        }
        for loader in ["vanilla", "fabric", "quilt", "forge", "neoforge"] {
            assert!(custom_plugins(loader, &e).is_empty(), "{loader}");
        }
    }

    #[test]
    fn a_server_with_no_custom_plugins_asks_for_nothing() {
        assert!(custom_plugins("paper", &env(&[])).is_empty());
        assert!(custom_plugins("paper", &env(&[("CUSTOM_PLUGINS", json!(""))])).is_empty());
        assert!(custom_plugins("paper", &Value::Null).is_empty());
    }

    #[test]
    fn a_minigame_is_marked_by_minigame_and_javascript_decides_what_that_means() {
        assert!(is_minigame(&env(&[("MINIGAME", json!("bedwars"))])));
        assert!(!is_minigame(&env(&[])));
        // `!!""` is false, and the desktop reads it that way too.
        assert!(!is_minigame(&env(&[("MINIGAME", json!(""))])));
        assert!(!is_minigame(&env(&[("MINIGAME", Value::Null)])));
    }

    #[test]
    fn our_own_namespace_is_forwarded_and_nothing_else_is() {
        let forwarded = forwarded_env(&env(&[
            ("MINIGAME", json!("bedwars")),
            ("MINIGAME_MIN_PLAYERS", json!(4)),
            ("MINIGAME_MODE", json!("doubles")),
            ("BEDWARS_TEAM_SIZE", json!(2)),
            ("VERSION", json!("1.21.4")),
            ("TYPE", json!("PAPER")),
            ("RCON_PASSWORD", json!("hunter2")),
        ]));

        assert_eq!(
            forwarded.keys().collect::<Vec<_>>(),
            [
                "BEDWARS_TEAM_SIZE",
                "MINIGAME",
                "MINIGAME_MIN_PLAYERS",
                "MINIGAME_MODE"
            ],
        );
        // A number becomes the text a shell would carry, not `4.0`.
        assert_eq!(forwarded["MINIGAME_MIN_PLAYERS"], "4");
    }

    /// The rule this exists to enforce: the server's settings are fetched with
    /// the user's token and are full of things a subprocess has no business
    /// seeing.
    #[test]
    fn nothing_outside_our_namespace_can_reach_the_server_process() {
        let forwarded = forwarded_env(&env(&[
            ("ACCESS_TOKEN", json!("secret")),
            ("RESTIC_PASSWORD", json!("secret")),
            ("MINIGAME", json!("bedwars")),
        ]));
        assert_eq!(forwarded.len(), 1);
        assert!(forwarded.contains_key("MINIGAME"));
    }

    #[test]
    fn advancements_are_disabled_for_a_fresh_minigame_directory_only() {
        let game = env(&[("MINIGAME", json!("bedwars"))]);

        let written = disable_advancements(&game, "").expect("a fresh directory gets the file");
        assert!(written.contains("disable-saving: true"));
        assert!(written.contains("- '*'"));

        // Paper has already written its own; leave it alone.
        assert_eq!(
            disable_advancements(&game, "settings:\n  bungeecord: false\n"),
            None
        );
        // Not a minigame — never touched.
        assert_eq!(disable_advancements(&env(&[]), ""), None);
    }
}

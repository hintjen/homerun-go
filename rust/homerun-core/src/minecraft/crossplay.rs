//! Crossplay: a Java server that Bedrock clients can also join.
//!
//! # What crossplay actually is
//!
//! A perfectly ordinary Java server, plus two plugins. **Geyser** speaks the
//! Bedrock protocol on a UDP port and translates it into a Java client session;
//! **Floodgate** lets those translated sessions in without a Mojang account, so
//! the server keeps `online-mode=true` and Java players still authenticate.
//!
//! That is the whole feature. There is no separate server, no second world, and
//! nothing about the launch that differs — which is why crossplay is a
//! `game_type` and not an engine, and why [`super::hosting`] routes it to the
//! JVM like any other Java server.
//!
//! # Why the decisions are here and not in a host
//!
//! Everything below is a decision — which jars, from where, what Geyser is
//! told. None of it is I/O. A host merges [`projects`] into what it was already
//! going to resolve from Modrinth, fetches [`floodgate_build`]'s answer, writes
//! [`config`]'s file, and asks the tunnel for `crossplay` exposure. Two hosts
//! cannot then disagree about what a crossplay server is.
//!
//! # Plugin mode, not Geyser Standalone
//!
//! The desktop runs Geyser as a **standalone** second JVM, which needs its own
//! config naming the Java server, its own lifecycle, and a Floodgate key poll.
//! Geyser also ships as a plugin, and in plugin mode it runs inside the server
//! JVM: it finds the server it fronts by definition, dies with it, and needs no
//! supervision. On a phone that is the only sensible shape — a second JVM is
//! memory that is not there and another process to keep alive against the OOM
//! killer.
//!
//! It also arrives through the mod pipeline that already exists, rather than a
//! second download mechanism this app would have to justify.
//!
//! # Where the jars come from, and why they differ
//!
//! Modrinth publishes Geyser for the Bukkit family *and* Fabric, but publishes
//! **Floodgate for Fabric and NeoForge only** — there is no Paper build listed.
//! So on Paper, Geyser resolves through Modrinth like every other plugin and
//! Floodgate comes from GeyserMC's own download API. [`projects`] and
//! [`floodgate`] are the two halves of that split, and each returns nothing for
//! the case the other covers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::mods;
use super::LISTEN_BEDROCK;
use crate::game::{Encoding, FileWrite};

/// GeyserMC's download API, which is where a Paper Floodgate build lives.
///
/// `latest/builds/latest` is an alias the API resolves server-side, so this is
/// a constant rather than something assembled from a version.
pub const FLOODGATE_META: &str =
    "https://download.geysermc.org/v2/projects/floodgate/versions/latest/builds/latest";

/// Is this the game type that wants a Bedrock bridge?
///
/// **`native-crossplay` only.** The API's unprefixed `crossplay` is the hosted
/// container type, which a device never launches — the same reason
/// [`super::settings::from_env`] tests for exactly this string.
pub fn is_crossplay(game_type: &str) -> bool {
    game_type == "native-crossplay"
}

/// Does this loader take Bukkit plugins?
///
/// Asked through [`mods::sub_dir`] rather than by listing the family again,
/// because that function is already the one place that knows.
fn bukkit_family(loader: &str) -> bool {
    mods::sub_dir(loader) == "plugins"
}

/// Modrinth slugs this game type implies, on top of what the player configured.
///
/// Empty for anything that is not crossplay, so a caller can merge the result
/// unconditionally and a non-crossplay server is untouched.
///
/// Floodgate is absent on the Bukkit family **on purpose** — Modrinth has no
/// Paper build of it, and asking for one resolves to nothing and installs
/// nothing, silently. [`floodgate`] covers that case instead.
pub fn projects(game_type: &str, loader: &str) -> Vec<&'static str> {
    if !is_crossplay(game_type) {
        return Vec::new();
    }
    if bukkit_family(loader) {
        vec!["geyser"]
    } else {
        vec!["geyser", "floodgate"]
    }
}

/// Merge [`projects`] into a server's configured `MODRINTH_PROJECTS`.
///
/// Returns the configured string untouched for anything that is not crossplay,
/// so a caller can pipe every server through this.
///
/// **A slug already listed is left exactly as it is.** A player whose server
/// came from the desktop may already carry a pinned `geyser:<versionId>`, and
/// appending a bare `geyser` beside it would put two entries for one project
/// into the resolver — which installs the plugin twice under two filenames, and
/// Bukkit refuses to load the second. The pin wins because it is the more
/// specific instruction, and because silently overriding what a player chose is
/// not this function's business.
pub fn merge_projects(game_type: &str, loader: &str, configured: &str) -> String {
    let wanted = projects(game_type, loader);
    if wanted.is_empty() {
        return configured.to_string();
    }

    let listed: Vec<String> = mods::split_list(configured);
    let already: Vec<String> = listed
        .iter()
        .map(|entry| {
            entry
                .split(':')
                .next()
                .unwrap_or(entry)
                .trim()
                .to_ascii_lowercase()
        })
        .collect();

    let mut merged = listed.clone();
    for slug in wanted {
        if !already.iter().any(|s| s == slug) {
            merged.push(slug.to_string());
        }
    }
    merged.join(
        "
",
    )
}

/// A jar to fetch from outside Modrinth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fetch {
    pub url: String,
    /// The name to save it under, **stable across builds**.
    ///
    /// Load-bearing: [`mods::sweep`] only ever deletes files it installed
    /// itself, so a jar fetched here is never cleaned up. Under a versioned
    /// name every update would leave the previous build beside the new one and
    /// Bukkit would refuse to load two copies of one plugin.
    pub file_name: String,
    /// Hex SHA-256 from the build metadata, for the host to verify against.
    pub sha256: Option<String>,
    /// Relative to the server directory.
    pub sub_dir: String,
}

/// Which GeyserMC download flavour this server needs, or `None` when Modrinth
/// already supplies Floodgate for this loader.
///
/// Returns the flavour name rather than a bool so the caller has something to
/// pass straight to [`floodgate_build`].
pub fn floodgate(game_type: &str, loader: &str) -> Option<&'static str> {
    (is_crossplay(game_type) && bukkit_family(loader)).then_some("spigot")
}

/// Read GeyserMC's build metadata into the one download `flavour` names.
///
/// The metadata carries a SHA-256 per flavour and the canonical filename, so
/// the download is checksum-verifiable and the destination name comes from
/// GeyserMC rather than from us.
pub fn floodgate_build(meta: &Value, flavour: &str) -> Option<Fetch> {
    let version = meta.get("version")?.as_str()?;
    let build = meta.get("build").and_then(Value::as_u64)?;
    let download = meta.get("downloads")?.get(flavour)?;

    Some(Fetch {
        url: format!(
            "https://download.geysermc.org/v2/projects/floodgate/versions/{version}/builds/{build}/downloads/{flavour}"
        ),
        file_name: download
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("floodgate-spigot.jar")
            .to_string(),
        sha256: download
            .get("sha256")
            .and_then(Value::as_str)
            .map(str::to_string),
        sub_dir: "plugins".to_string(),
    })
}

/// Where this loader's Geyser keeps its configuration.
///
/// `None` for a loader whose plugin directory name nobody here has read. That
/// is deliberate: the two below were taken from the jar's own `plugin.yml` and
/// from the API's Fabric path, and a guessed third would produce a file Geyser
/// never opens — which looks exactly like a config that did not work.
fn config_path(loader: &str) -> Option<&'static str> {
    if bukkit_family(loader) {
        // `plugin.yml` inside the Modrinth Paper jar declares
        // `name: Geyser-Spigot`, and Bukkit derives the data directory from it.
        return Some("plugins/Geyser-Spigot/config.yml");
    }
    match loader {
        "fabric" => Some("config/Geyser-Fabric/config.yml"),
        _ => None,
    }
}

/// What to seed Geyser's configuration with, or `None` when there is nothing
/// to configure.
///
/// # Partial on purpose
///
/// Geyser reads its config through Configurate, which fills every key it is not
/// given from the defaults — so this names the two that must not be left to a
/// default and nothing else.
///
/// - **`bedrock.port`** is written even though [`LISTEN_BEDROCK`] is also
///   Geyser's own default. The gateway DNATs to this port and the wireproxy
///   `ListenPort` is fixed to it, so the coupling is real; writing it makes the
///   coupling greppable instead of a coincidence that would break silently if
///   Geyser ever changed its default.
/// - **`auth-type`** is what lets a Bedrock player in without a Mojang account.
///   Geyser is documented to detect a Floodgate on the same server and it
///   `softdepend`s on it, so this may well be redundant — but the failure mode
///   of being wrong is `online`, which rejects every Bedrock join with an
///   authentication error that reads like a network problem. One line removes
///   the doubt.
///
/// `bedrock.address` is **not** written: the default binds every interface,
/// which is what the tunnel's `127.0.0.1` target needs and what a future
/// local-network mode would need too. Neither is anything under `java:` beyond
/// the auth type — a plugin-mode Geyser is inside the server it fronts and
/// finds `plugins/floodgate/key.pem` itself.
///
/// # Seed, not sync
///
/// A host must write this **only when the file is absent**. Geyser rewrites the
/// file with every default expanded the first time it starts, and dropping a
/// two-key partial back over that on the next launch invites a config-version
/// migration nobody has tested. Nothing on a phone can edit the file in
/// between, so there is no drift to correct.
pub fn config(game_type: &str, loader: &str) -> Option<FileWrite> {
    if !is_crossplay(game_type) {
        return None;
    }
    let path = config_path(loader)?;

    Some(FileWrite {
        path: path.to_string(),
        contents: format!("bedrock:\n  port: {LISTEN_BEDROCK}\njava:\n  auth-type: floodgate\n"),
        encoding: Encoding::Utf8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The whole module is a no-op for the servers that are not crossplay, and
    /// that is most of them. Every entry point has to agree about it — a
    /// function that quietly returned something here would put Geyser on a
    /// vanilla server.
    #[test]
    fn a_plain_java_server_asks_for_nothing() {
        for game_type in ["native", "native-pumpkin", "java", ""] {
            assert!(projects(game_type, "paper").is_empty(), "{game_type}");
            assert_eq!(floodgate(game_type, "paper"), None, "{game_type}");
            assert_eq!(config(game_type, "paper"), None, "{game_type}");
        }
    }

    /// The hosted `crossplay` type runs in a container and is never launched by
    /// a device, so it must not be mistaken for the native one.
    #[test]
    fn only_the_native_spelling_counts() {
        assert!(is_crossplay("native-crossplay"));
        assert!(!is_crossplay("crossplay"));
    }

    /// Paper gets Geyser from Modrinth and Floodgate from GeyserMC. Asking
    /// Modrinth for a Paper Floodgate resolves to nothing and installs nothing
    /// without complaining, which is the bug this split exists to avoid.
    #[test]
    fn paper_splits_the_two_plugins_across_two_sources() {
        assert_eq!(projects("native-crossplay", "paper"), vec!["geyser"]);
        assert_eq!(floodgate("native-crossplay", "paper"), Some("spigot"));
    }

    /// Spigot and Bukkit are the same family and must not answer differently.
    #[test]
    fn the_whole_bukkit_family_answers_together() {
        for loader in ["paper", "spigot", "bukkit"] {
            assert_eq!(
                projects("native-crossplay", loader),
                vec!["geyser"],
                "{loader}"
            );
            assert!(floodgate("native-crossplay", loader).is_some(), "{loader}");
        }
    }

    /// Fabric has both on Modrinth, so nothing is fetched out of band.
    #[test]
    fn fabric_takes_both_from_modrinth() {
        assert_eq!(
            projects("native-crossplay", "fabric"),
            vec!["geyser", "floodgate"]
        );
        assert_eq!(floodgate("native-crossplay", "fabric"), None);
    }

    /// The ordinary server's list comes back byte-identical, because this runs
    /// on every launch of every Java server.
    #[test]
    fn a_non_crossplay_list_is_returned_untouched() {
        let configured = "sodium
lithium:abc123";
        assert_eq!(
            merge_projects("native", "paper", configured),
            configured.to_string()
        );
    }

    #[test]
    fn crossplay_adds_geyser_to_what_the_player_chose() {
        let merged = merge_projects(
            "native-crossplay",
            "paper",
            "worldedit
vault",
        );
        assert_eq!(
            merged,
            "worldedit
vault
geyser"
        );
    }

    /// The desktop pins a version id when it enables Geyser. Appending a bare
    /// slug beside that pin resolves the same project twice and Bukkit refuses
    /// the duplicate plugin — so the pin has to be recognised and left alone.
    #[test]
    fn an_existing_pin_is_not_joined_by_a_second_entry() {
        let merged = merge_projects("native-crossplay", "paper", "geyser:AbC12345");
        assert_eq!(merged, "geyser:AbC12345");
    }

    /// The API writes these comma-separated and the dashboard writes them
    /// newline-separated. Both are one list.
    #[test]
    fn either_separator_is_understood() {
        let merged = merge_projects("native-crossplay", "paper", "geyser,floodgate");
        assert_eq!(
            merged,
            "geyser
floodgate"
        );
    }

    /// Case is not part of a slug's identity anywhere else in the resolver, so
    /// it must not be here either.
    #[test]
    fn a_pin_in_the_wrong_case_still_counts() {
        let merged = merge_projects("native-crossplay", "paper", "GeYsEr:AbC12345");
        assert_eq!(merged, "GeYsEr:AbC12345");
    }

    /// Fabric wants both, and a server that already lists one gets only the
    /// other.
    #[test]
    fn fabric_merges_only_what_is_missing() {
        let merged = merge_projects("native-crossplay", "fabric", "floodgate");
        assert_eq!(
            merged,
            "floodgate
geyser"
        );
    }

    /// An empty configuration is the common case for a crossplay server made on
    /// a phone — creation writes no projects at all.
    #[test]
    fn an_empty_list_becomes_just_the_plugins() {
        assert_eq!(merge_projects("native-crossplay", "paper", ""), "geyser");
    }

    #[test]
    fn a_build_becomes_a_checksummed_download() {
        let meta = json!({
            "version": "2.2.5",
            "build": 140,
            "downloads": {
                "spigot": { "name": "floodgate-spigot.jar", "sha256": "9f436c42ff" },
                "bungee": { "name": "floodgate-bungee.jar", "sha256": "0e09f6a629" }
            }
        });

        let fetch = floodgate_build(&meta, "spigot").unwrap();
        assert_eq!(
            fetch.url,
            "https://download.geysermc.org/v2/projects/floodgate/versions/2.2.5/builds/140/downloads/spigot"
        );
        assert_eq!(fetch.file_name, "floodgate-spigot.jar");
        assert_eq!(fetch.sha256.as_deref(), Some("9f436c42ff"));
        assert_eq!(fetch.sub_dir, "plugins");
    }

    /// Metadata that does not describe the flavour asked for is not a jar with
    /// a guessed URL — it is nothing, and the host reports a failed install.
    #[test]
    fn a_missing_flavour_is_not_invented() {
        let meta = json!({ "version": "2.2.5", "build": 140, "downloads": {} });
        assert_eq!(floodgate_build(&meta, "spigot"), None);
        assert_eq!(floodgate_build(&json!({}), "spigot"), None);
    }

    /// The port in the file is the port the gateway forwards to. If these ever
    /// drift the server starts, logs nothing wrong, and no Bedrock player can
    /// join — so the constant is asserted, not the literal.
    #[test]
    fn the_config_names_the_port_the_gateway_uses() {
        let file = config("native-crossplay", "paper").unwrap();
        assert_eq!(file.path, "plugins/Geyser-Spigot/config.yml");
        assert!(
            file.contents.contains(&format!("port: {LISTEN_BEDROCK}")),
            "{}",
            file.contents
        );
        assert!(file.contents.contains("auth-type: floodgate"));
        assert_eq!(file.encoding, Encoding::Utf8);
    }

    #[test]
    fn fabric_has_its_own_config_directory() {
        let file = config("native-crossplay", "fabric").unwrap();
        assert_eq!(file.path, "config/Geyser-Fabric/config.yml");
    }

    /// A loader whose Geyser directory nobody has read gets no file rather than
    /// a guessed one. The host logs that it happened; a file in the wrong place
    /// would look like a config that simply did not work.
    #[test]
    fn an_unread_loader_gets_no_guess() {
        assert_eq!(config("native-crossplay", "neoforge"), None);
        assert_eq!(config("native-crossplay", "vanilla"), None);
    }
}

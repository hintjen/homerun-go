//! What a mod jar says about itself.
//!
//! # Why this is needed at all
//!
//! Modrinth's project-level `server_side` is authoritative and settles most
//! mods. It cannot settle the ones that are not on Modrinth — modpacks
//! routinely ship some mods directly in `overrides/mods/` as **CurseForge**
//! builds, whose bytes match no Modrinth file — and it cannot settle the ones
//! whose Modrinth metadata has drifted from the jar the loader actually reads.
//!
//! Both cases end the same way: a client-only mod on a dedicated server, and
//! `NoClassDefFoundError: net/minecraft/client/...` at boot.
//!
//! # What is parsed, and what it is worth
//!
//! Ported from `classifyModJarSide` (`mod-installer.ts:184`). The host pulls
//! the entries out of the zip; this reads them.
//!
//! - **`fabric.mod.json` / `quilt.mod.json`** — `environment` is a real answer.
//!   `"client"` means client-only, and the loader itself enforces it.
//! - **`META-INF/neoforge.mods.toml` and `META-INF/mods.toml`** — read **both**
//!   and union their dependencies. A jar often ships a minimal legacy
//!   `mods.toml` for loader compatibility alongside a `neoforge.mods.toml`
//!   carrying the real data; NeoForge reads the latter, so we must too.
//!
//! A Forge `side = "BOTH"` is a **weak, often-wrong** signal — authors leave it
//! on genuinely client-only mods — which is why [`Side::Serverable`] means
//! "not self-declared client", not "safe on a server". The caller is expected
//! to keep asking.

use serde::{Deserialize, Serialize};

/// What a jar claims about where it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Declared client-only. Trustworthy: excluding it is safe.
    Client,
    /// Not declared client-only. **Not** the same as "runs on a server".
    Serverable,
    /// The jar said nothing readable.
    Unknown,
}

/// What one jar declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub side: Side,
    #[serde(rename = "modId")]
    pub mod_id: Option<String>,
    /// Mandatory dependencies, with the loader's own ids filtered out.
    pub deps: Vec<String>,
}

impl Default for Facts {
    fn default() -> Self {
        Facts {
            side: Side::Unknown,
            mod_id: None,
            deps: Vec::new(),
        }
    }
}

/// Ids that name the loader or the game rather than another mod.
///
/// A dependency on `minecraft` is not something to go looking for on Modrinth.
const LOADER_IDS: [&str; 8] = [
    "minecraft",
    "forge",
    "neoforge",
    "fabricloader",
    "fabric-loader",
    "quilt_loader",
    "java",
    "fabric",
];

fn is_loader_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    LOADER_IDS.contains(&lower.as_str())
}

/// Read a jar's own metadata.
///
/// [`fabric`] is the text of `fabric.mod.json` or `quilt.mod.json`, whichever
/// the jar has. [`tomls`] is the text of `META-INF/neoforge.mods.toml` and
/// `META-INF/mods.toml`, in that order of preference — both, when both exist.
///
/// A jar with Fabric metadata is judged by it alone; the toml arms are for
/// Forge-family jars, which have no `environment` field of their own.
pub fn read(fabric: Option<&str>, tomls: &[String]) -> Facts {
    if let Some(text) = fabric {
        return from_fabric(text);
    }
    if !tomls.is_empty() {
        return from_toml(tomls);
    }
    Facts::default()
}

fn from_fabric(text: &str) -> Facts {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
        // Malformed metadata is tolerated rather than fatal: the loader may
        // well accept it, and refusing to install a mod because we could not
        // read its manifest is a worse outcome than installing it.
        return Facts::default();
    };

    let env = doc
        .get("environment")
        .or_else(|| doc.get("minecraft").and_then(|m| m.get("environment")))
        .and_then(|v| v.as_str());

    let mod_id = doc
        .get("id")
        .or_else(|| doc.get("quilt_loader").and_then(|q| q.get("id")))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let depends = doc
        .get("depends")
        .or_else(|| doc.get("quilt_loader").and_then(|q| q.get("depends")));

    let deps: Vec<String> = match depends {
        // Fabric writes an object keyed by mod id; Quilt writes a list of
        // objects with an `id`. Both spellings appear in the wild.
        Some(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|d| {
                d.get("id")
                    .and_then(|v| v.as_str())
                    .or_else(|| d.as_str())
                    .map(str::to_string)
            })
            .collect(),
        _ => Vec::new(),
    };

    Facts {
        side: match env {
            Some("client") => Side::Client,
            Some("server") | Some("*") => Side::Serverable,
            _ => Side::Unknown,
        },
        mod_id,
        deps: deps.into_iter().filter(|d| !is_loader_id(d)).collect(),
    }
}

/// Read the Forge-family tomls.
///
/// Hand-scanned rather than parsed with a TOML crate, for the same reason the
/// maven metadata is: this crate depends on serde and nothing else, and what
/// is needed is three fields. The desktop reaches the same values with
/// regexes over the same text.
fn from_toml(tomls: &[String]) -> Facts {
    let mut mod_id: Option<String> = None;
    let mut sides: Vec<String> = Vec::new();
    let mut deps: Vec<String> = Vec::new();

    for text in tomls {
        if mod_id.is_none() {
            mod_id = first_mod_id(text);
        }

        for block in dependency_blocks(text) {
            // A dependency *on the loader or the game* is where `side` lives:
            // `[[dependencies.x]] modId = "minecraft" ... side = "CLIENT"` is
            // how a Forge mod declares itself client-only.
            let Some(id) = quoted_after(block, "modId") else {
                continue;
            };
            if is_loader_id(&id) {
                if let Some(side) = quoted_after(block, "side") {
                    sides.push(side.to_ascii_uppercase());
                }
                continue;
            }
            let mandatory = block.contains("mandatory = true")
                || block.contains("mandatory=true")
                || block.contains("type = \"required\"")
                || block.contains("type=\"required\"");
            if mandatory && !deps.contains(&id) {
                deps.push(id);
            }
        }
    }

    Facts {
        // Every declared side must be CLIENT for the jar to be client-only.
        // One SERVER or BOTH among them means it claims to run here — weakly,
        // which is the caller's problem and not this function's.
        side: if sides.is_empty() {
            Side::Unknown
        } else if sides.iter().all(|s| s == "CLIENT") {
            Side::Client
        } else {
            Side::Serverable
        },
        mod_id,
        deps,
    }
}

/// The first `modId = "..."` in a document — the jar's own id.
fn first_mod_id(text: &str) -> Option<String> {
    text.lines()
        .find(|line| line.trim_start().starts_with("modId"))
        .and_then(|line| quoted_after(line, "modId"))
}

/// Each `[[dependencies.*]]` block, up to the next `[[` or the end.
fn dependency_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[dependencies.") {
        let after = &rest[start..];
        // Skip the opening `[[` so the search for the next one does not find
        // this block's own header.
        let end = after[2..].find("[[").map(|i| i + 2).unwrap_or(after.len());
        blocks.push(&after[..end]);
        rest = &after[end..];
    }
    blocks
}

/// The value of `key = "value"` within [`text`], if there is one.
fn quoted_after(text: &str, key: &str) -> Option<String> {
    let mut rest = text;
    while let Some(at) = rest.find(key) {
        let after = &rest[at + key.len()..];
        let trimmed = after.trim_start();
        if let Some(tail) = trimmed.strip_prefix('=') {
            let tail = tail.trim_start();
            if let Some(quoted) = tail.strip_prefix('"') {
                if let Some(end) = quoted.find('"') {
                    return Some(quoted[..end].to_string());
                }
            }
        }
        rest = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fabric_jar_declaring_client_is_believed() {
        let facts = read(Some(r#"{"id":"sodium","environment":"client"}"#), &[]);
        assert_eq!(facts.side, Side::Client);
        assert_eq!(facts.mod_id.as_deref(), Some("sodium"));
    }

    #[test]
    fn fabric_dependencies_come_from_an_object_and_the_loader_is_not_one() {
        let facts = read(
            Some(
                r#"{"id":"chipped","depends":{"fabricloader":">=0.15","athena":"*","minecraft":"1.21.4"}}"#,
            ),
            &[],
        );
        assert_eq!(facts.deps, vec!["athena"]);
    }

    /// Quilt writes the same information as a list.
    #[test]
    fn quilt_metadata_reads_the_same_way() {
        let facts = read(
            Some(r#"{"quilt_loader":{"id":"example","depends":[{"id":"athena"},{"id":"java"}]}}"#),
            &[],
        );
        assert_eq!(facts.mod_id.as_deref(), Some("example"));
        assert_eq!(facts.deps, vec!["athena"]);
    }

    #[test]
    fn malformed_metadata_is_tolerated_rather_than_fatal() {
        assert_eq!(read(Some("{not json"), &[]), Facts::default());
        assert_eq!(read(None, &[]), Facts::default());
    }

    const CLIENT_ONLY_TOML: &str = r#"
modLoader = "javafml"
[[mods]]
modId = "citresewn"
[[dependencies.citresewn]]
    modId = "minecraft"
    mandatory = true
    side = "CLIENT"
[[dependencies.citresewn]]
    modId = "forge"
    mandatory = true
    side = "CLIENT"
"#;

    #[test]
    fn a_forge_jar_whose_every_side_is_client_is_client_only() {
        let facts = read(None, &[CLIENT_ONLY_TOML.to_string()]);
        assert_eq!(facts.side, Side::Client);
        assert_eq!(facts.mod_id.as_deref(), Some("citresewn"));
    }

    /// `side = "BOTH"` is what most mods declare, including client-only ones.
    /// It means "not self-declared client", and the caller keeps asking.
    #[test]
    fn a_forge_jar_declaring_both_is_only_serverable_not_safe() {
        let toml = CLIENT_ONLY_TOML.replace("CLIENT", "BOTH");
        assert_eq!(read(None, &[toml]).side, Side::Serverable);
    }

    /// A jar ships a minimal legacy `mods.toml` beside a `neoforge.mods.toml`
    /// carrying the real dependencies. NeoForge reads the latter, so the union
    /// is what the loader will enforce — sodiumoptionsapi declares
    /// reeses_sodium_options in only one of the two.
    #[test]
    fn both_toml_manifests_are_read_and_their_dependencies_unioned() {
        let neoforge = r#"
[[mods]]
modId = "sodiumoptionsapi"
[[dependencies.sodiumoptionsapi]]
    modId = "reeses_sodium_options"
    type = "required"
"#;
        let legacy = r#"
[[mods]]
modId = "sodiumoptionsapi"
[[dependencies.sodiumoptionsapi]]
    modId = "sodium"
    mandatory = true
"#;
        let facts = read(None, &[neoforge.to_string(), legacy.to_string()]);
        assert!(
            facts.deps.contains(&"reeses_sodium_options".to_string()),
            "{facts:?}"
        );
        assert!(facts.deps.contains(&"sodium".to_string()), "{facts:?}");
    }

    #[test]
    fn optional_forge_dependencies_are_not_pulled_in() {
        let toml = r#"
[[mods]]
modId = "example"
[[dependencies.example]]
    modId = "jei"
    mandatory = false
"#;
        assert!(read(None, &[toml.to_string()]).deps.is_empty());
    }

    #[test]
    fn a_jar_with_no_readable_side_says_unknown_rather_than_guessing() {
        let toml = "[[mods]]\nmodId = \"example\"\n";
        let facts = read(None, &[toml.to_string()]);
        assert_eq!(facts.side, Side::Unknown);
        assert_eq!(facts.mod_id.as_deref(), Some("example"));
    }
}

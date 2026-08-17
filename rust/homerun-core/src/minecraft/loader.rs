//! Mod loaders that install by running an installer.
//!
//! # Why this is separate from [`super::jar`]
//!
//! Vanilla and Paper publish a **server jar**: resolve a URL, download it,
//! check a digest, run it. Fabric publishes an **installer** — a jar that is
//! run once, fetches what it needs, and leaves a launchable server behind. The
//! two share a version resolver and nothing else, so `jar` keeps the artifact
//! and this keeps the install.
//!
//! # Provenance
//!
//! `mod-installer.ts` in the `homerun` repo is the spec — `setupServerLoader`
//! and the helpers around it. Everything here that looks arbitrary was learned
//! there, and the module doc names the file each behaviour came from.
//!
//! # What the host still does
//!
//! Every HTTP request, every file write, and running the installer itself.
//! This module answers: which installer, whether the one already installed can
//! be kept, what to delete when it cannot, and what to launch afterwards.

use super::jar::Loader;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// Fabric's installer index. The host fetches it and passes the JSON back.
pub const FABRIC_INSTALLER_META: &str = "https://meta.fabricmc.net/v2/versions/installer";

/// Quilt's installer index. Same idea as Fabric's, different shape — see
/// [`quilt_installer_url`].
pub const QUILT_INSTALLER_META: &str = "https://meta.quiltmc.org/v3/versions/installer";

/// What a server directory records about the loader installed into it.
///
/// Written as `.homerun-loader.json`, the same name and shape the desktop
/// uses, so a server directory restored from a desktop backup is understood
/// rather than reinstalled. `mods` and `modpackFiles` are the desktop's too
/// and arrive with M4; a host that does not write them must **preserve** any
/// it finds, which is why they are not modelled here yet — round-tripping a
/// field this module does not understand would silently drop it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub loader: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    #[serde(rename = "loaderVersion", skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
}

/// Which installer to download, from Fabric's installer index.
///
/// The desktop takes the first entry marked `stable`, falling back to the
/// first entry at all (`mod-installer.ts:781`). Order is not relied upon for
/// the stable pick — the index is newest-first today, and picking *a* stable
/// installer is what matters, since the installer's own version has no bearing
/// on what it installs.
pub fn fabric_installer_url(meta: &serde_json::Value) -> Result<String> {
    let entries = meta
        .as_array()
        .ok_or_else(|| Error::Malformed("the Fabric installer index is not a list".into()))?;

    let chosen = entries
        .iter()
        .find(|e| e.get("stable").and_then(serde_json::Value::as_bool) == Some(true))
        .or_else(|| entries.first())
        .ok_or_else(|| Error::Malformed("the Fabric installer index is empty".into()))?;

    chosen
        .get("url")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .ok_or_else(|| Error::Malformed("the chosen Fabric installer has no URL".into()))
}

/// Which installer to download, from Quilt's installer index.
///
/// Quilt's index has **no `stable` field**, so there is nothing to prefer and
/// the first entry wins. That is not the same rule as
/// [`fabric_installer_url`], and the difference is the reason this is a
/// separate function rather than a shared one with a flag: reading Fabric's
/// rule onto Quilt's data would silently fall through to `entries.first()`
/// every time and look like it was choosing.
///
/// Which loader gets installed is not decided here. The installer resolves
/// that itself, and it picks the latest *stable* loader — verified on a device,
/// where an index whose newest entry was `0.30.1-beta.2` installed `0.30.0`.
pub fn quilt_installer_url(meta: &serde_json::Value) -> Result<String> {
    let entries = meta
        .as_array()
        .ok_or_else(|| Error::Malformed("the Quilt installer index is not a list".into()))?;

    entries
        .first()
        .ok_or_else(|| Error::Malformed("the Quilt installer index is empty".into()))?
        .get("url")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .ok_or_else(|| Error::Malformed("the chosen Quilt installer has no URL".into()))
}

/// Where Quilt says whether it has mapped a Minecraft version at all.
///
/// Quilt lags Minecraft releases by weeks, sometimes longer, and its installer
/// does not fail helpfully when asked for a version it cannot map — so this is
/// checked first. The desktop does the same
/// (`mod-installer.ts`, the `case "quilt"` block), and it is the reason the UI
/// can offer Quilt on 1.21.11 while Fabric is already on 26.2.
pub fn quilt_intermediary_url(mc_version: &str) -> String {
    format!(
        "https://meta.quiltmc.org/v3/versions/intermediary/{}",
        super::mods::encode(mc_version)
    )
}

/// Refuse a Minecraft version Quilt has not published mappings for.
///
/// A non-empty list means mapped. Anything else — an empty list, an object, a
/// request that failed and produced `null` — means no, which is the safe
/// direction: running the installer anyway leaves a half-installed directory
/// and an error naming none of this.
///
/// The message names the two things a player can actually do, because "Quilt
/// does not support this" alone leaves them stuck on a screen that offered it.
pub fn ensure_quilt_supports(mc_version: &str, intermediary: &serde_json::Value) -> Result<()> {
    let mapped = intermediary
        .as_array()
        .is_some_and(|versions| !versions.is_empty());

    if mapped {
        Ok(())
    } else {
        Err(Error::Unsupported(format!(
            "Quilt does not yet support Minecraft {mc_version} (no mappings published). \
             Use the Fabric loader instead, or pick an older Minecraft version."
        )))
    }
}

/// The jar to launch once [`Loader`] has been installed.
///
/// Fabric's launch jar carries `Class-Path` in its manifest naming every
/// library the installer put in `libraries/`, and the JVM's application class
/// loader honours that — which is the whole reason Fabric needs no argfile
/// handling and Forge does. Quilt does exactly the same thing, under its own
/// name.
pub fn launch_jar(loader: Loader) -> Option<&'static str> {
    match loader {
        Loader::Fabric => Some("fabric-server-launch.jar"),
        Loader::Quilt => Some("quilt-server-launch.jar"),
        // Forge and NeoForge have no launch jar at all: their argfile carries
        // the module path, the main class and the program arguments, and
        // `server.jar` in their directory is a placeholder nothing runs. See
        // [`crate::minecraft::argfile`].
        Loader::NeoForge | Loader::Forge => None,
        Loader::Vanilla | Loader::Paper => None,
    }
}

/// NeoForge's maven metadata. Its versions are `<mc-minor>.<mc-patch>.<build>`.
pub const NEOFORGE_METADATA: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";

/// Forge's maven metadata. Its versions are `<mc>-<build>`.
pub const FORGE_METADATA: &str =
    "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml";

/// The installer URL for a resolved loader build.
pub fn installer_url(loader: Loader, version: &str) -> Result<String> {
    match loader {
        Loader::NeoForge => Ok(format!(
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/{version}/neoforge-{version}-installer.jar"
        )),
        Loader::Forge => Ok(format!(
            "https://maven.minecraftforge.net/net/minecraftforge/forge/{version}/forge-{version}-installer.jar"
        )),
        other => Err(Error::Unsupported(format!(
            "{} does not have a versioned installer",
            other.as_str()
        ))),
    }
}

/// NeoForge's version prefix for a Minecraft version: `1.21.4` -> `21.4.`.
///
/// A missing patch is `0`, which is how `1.21` becomes `21.0.` — NeoForge
/// numbers its first build for a minor release that way.
fn neoforge_prefix(mc_version: &str) -> String {
    let parts: Vec<&str> = mc_version.split('.').collect();
    if parts.first() == Some(&"1") {
        format!(
            "{}.{}.",
            parts.get(1).unwrap_or(&"0"),
            parts.get(2).unwrap_or(&"0")
        )
    } else {
        format!(
            "{}.{}.",
            parts.first().unwrap_or(&"0"),
            parts.get(1).unwrap_or(&"0")
        )
    }
}

/// Every `<version>` in a maven-metadata document, in document order.
///
/// Hand-scanned rather than parsed: this crate depends on serde and nothing
/// else, and the shape here is a flat list of one tag. The desktop uses a
/// regex over the same text and finds the same strings.
fn maven_versions(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<version>") {
        let after = &rest[start + "<version>".len()..];
        match after.find("</version>") {
            Some(end) => {
                out.push(after[..end].trim());
                rest = &after[end..];
            }
            None => break,
        }
    }
    out
}

/// Compare maven versions by their numeric components rather than by position.
///
/// Ported from `resolveForgeVersion` (`mod-installer.ts:562`), where it fixes
/// a real bug: `maven-metadata.xml` is **not** reliably newest-last for old
/// Minecraft. 1.7.10's last entry is the ancient `10.13.0.1150`, whose bundled
/// LaunchWrapper 1.9 crashes at boot.
///
/// That version cannot arise here — it wants Java 8 and is refused long before
/// this runs — but the algorithm is the correct one either way, and position
/// is not something to start trusting because today's inputs happen to be
/// ordered.
fn newer(a: &str, b: &str) -> bool {
    let key = |v: &str| -> Vec<u64> {
        v.split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    };
    let (ka, kb) = (key(a), key(b));
    for i in 0..ka.len().max(kb.len()) {
        let (x, y) = (
            ka.get(i).copied().unwrap_or(0),
            kb.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// The NeoForge build to install for a Minecraft version.
///
/// A [`pinned`] build is honoured when the metadata has it, and otherwise the
/// newest is used — the desktop warns and falls back the same way, because a
/// pin that no longer exists should not stop a server starting.
pub fn resolve_neoforge_version(
    xml: &str,
    mc_version: &str,
    pinned: Option<&str>,
) -> Result<String> {
    let prefix = neoforge_prefix(mc_version);
    let matching: Vec<&str> = maven_versions(xml)
        .into_iter()
        .filter(|v| v.starts_with(&prefix))
        .collect();

    if matching.is_empty() {
        return Err(Error::Unsupported(format!(
            "NeoForge has no build for Minecraft {mc_version} yet."
        )));
    }
    if let Some(pin) = pinned {
        if matching.contains(&pin) {
            return Ok(pin.to_string());
        }
    }
    Ok(matching
        .iter()
        .copied()
        .fold(matching[0], |best, v| if newer(v, best) { v } else { best })
        .to_string())
}

/// The Forge build to install for a Minecraft version.
///
/// The pin format varies and all three spellings must resolve — packs express
/// `dependencies.forge` as a bare build (`47.2.17`), as the full artifact
/// (`1.20.1-47.2.17`), or with old Minecraft's doubled suffix
/// (`1.7.10-10.13.4.1614-1.7.10`).
pub fn resolve_forge_version(xml: &str, mc_version: &str, pinned: Option<&str>) -> Result<String> {
    let prefix = format!("{mc_version}-");
    let matching: Vec<&str> = maven_versions(xml)
        .into_iter()
        .filter(|v| v.starts_with(&prefix))
        .collect();

    if matching.is_empty() {
        return Err(Error::Unsupported(format!(
            "Forge has no build for Minecraft {mc_version}."
        )));
    }

    if let Some(pin) = pinned {
        let candidates = [pin.to_string(), format!("{mc_version}-{pin}")];
        let found = matching.iter().find(|v| {
            candidates.iter().any(|c| c == *v)
                || candidates.iter().any(|c| v.starts_with(&format!("{c}-")))
        });
        if let Some(found) = found {
            return Ok(found.to_string());
        }
    }

    Ok(matching
        .iter()
        .copied()
        .fold(matching[0], |best, v| if newer(v, best) { v } else { best })
        .to_string())
}

/// Whether what is installed has to be torn down and installed again.
///
/// A **loader change**, a **Minecraft version change**, or a **loader version
/// change** all force it (`mod-installer.ts:735`). The last one matters more
/// than it sounds: a modpack is built against one loader build, and
/// version-sensitive mixins target the exact patched classes of that build, so
/// installing a different one breaks injection at boot rather than at install.
///
/// A pinned version of `None` never forces a reinstall on its own — an
/// unpinned server keeps whatever it has rather than chasing the newest loader
/// on every start.
pub fn needs_reinstall(
    installed: Option<&Installed>,
    loader: Loader,
    mc_version: &str,
    loader_version: Option<&str>,
) -> bool {
    let Some(installed) = installed else {
        // No marker at all. The host still checks the launch jar is there
        // before believing this, because a marker can go missing while a
        // perfectly good install sits beside it — the same failure `jar`
        // handles with its `adopt` verdict.
        return true;
    };

    if installed.loader != loader.as_str() || installed.mc_version != mc_version {
        return true;
    }

    match (loader_version, installed.loader_version.as_deref()) {
        (Some(wanted), have) => have != Some(wanted),
        (None, _) => false,
    }
}

/// Everything a loader install leaves behind, given a directory listing.
///
/// Ported from `cleanLoaderFiles` (`mod-installer.ts:321`) **including the
/// entries for loaders this build cannot host**. That is deliberate: a server
/// directory can arrive from a desktop backup carrying a Forge install, and
/// switching it to Fabric has to remove the Forge jars or `findLegacyForgeJar`
/// -shaped confusion follows on the next start. Removing files for a loader we
/// never install costs nothing; failing to remove them strands a jar.
///
/// `libraries/` is named here but is a **directory**, and the host removes it
/// recursively.
pub fn files_to_clean(entries: &[String]) -> Vec<String> {
    const FIXED: [&str; 8] = [
        "run.bat",
        "run.sh",
        "user_jvm_args.txt",
        "server.jar",
        "fabric-server-launch.jar",
        "quilt-server-launch.jar",
        ".homerun-loader.json",
        // Not the desktop's — it is this host's record of a *downloaded* jar
        // (`ServerJar`'s `homerun-jar.json`). `server.jar` is on the list
        // above, so leaving the marker that describes it behind would leave a
        // record of a file that is gone. The `jar` module survives that, but
        // only by paying for a digest to find out.
        "homerun-jar.json",
    ];

    let mut out: Vec<String> = FIXED.iter().map(|s| s.to_string()).collect();
    out.push("libraries".to_string());

    for entry in entries {
        let lower = entry.to_ascii_lowercase();
        if !lower.ends_with(".jar") {
            continue;
        }
        // BuildTools output and Paper builds, which carry the version in the
        // name and so are never overwritten by a switch.
        let build_tools = ["paper-", "spigot_server-", "craftbukkit_server-"]
            .iter()
            .any(|p| entry.starts_with(p));
        // Legacy Forge's runnable universal jar, and the vanilla jar its
        // installer downloads alongside. `installer` is excluded so a
        // half-finished install does not delete the thing still running.
        let legacy_forge = lower.starts_with("forge-") && !lower.contains("installer");
        let legacy_vanilla = lower.starts_with("minecraft_server.");

        if build_tools || legacy_forge || legacy_vanilla {
            out.push(entry.clone());
        }
    }

    out.dedup();
    out
}

/// The Java major a server jar's bundler actually needs.
///
/// Mojang's manifest states a version and the bundled jar can disagree with
/// it; the jar wins, because it is what fails. The host reads the first eight
/// bytes of `net/minecraft/bundler/Main.class` and this reads the class-file
/// major out of them — 44 higher than the Java version that produced it.
///
/// Returns `None` when the bytes are not a class file, which is not an error:
/// plenty of server jars have no bundler at all.
///
/// From `getBundlerJavaVersion` (`mod-installer.ts:459`). It matters more here
/// than on the desktop, which downloads whatever JDK it likes — this build has
/// two runtimes and picking between them is a decision that has already been
/// made by the time the installer has produced a jar to inspect.
pub fn bundler_java_major(class_file_head: &[u8]) -> Option<u16> {
    if class_file_head.len() < 8 || class_file_head[..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    let major = u16::from_be_bytes([class_file_head[6], class_file_head[7]]);
    major.checked_sub(44).filter(|v| *v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn installer_index() -> serde_json::Value {
        json!([
            { "url": "https://maven.fabricmc.net/installer-1.0.3.jar", "version": "1.0.3", "stable": false },
            { "url": "https://maven.fabricmc.net/installer-1.0.1.jar", "version": "1.0.1", "stable": true },
            { "url": "https://maven.fabricmc.net/installer-0.9.0.jar", "version": "0.9.0", "stable": true },
        ])
    }

    #[test]
    fn the_first_stable_installer_wins_over_a_newer_unstable_one() {
        assert_eq!(
            fabric_installer_url(&installer_index()).unwrap(),
            "https://maven.fabricmc.net/installer-1.0.1.jar"
        );
    }

    #[test]
    fn an_index_with_nothing_stable_still_yields_an_installer() {
        let unstable = json!([
            { "url": "https://x/installer-2.0.jar", "stable": false },
        ]);
        assert_eq!(
            fabric_installer_url(&unstable).unwrap(),
            "https://x/installer-2.0.jar"
        );
    }

    #[test]
    fn an_empty_or_wrong_shaped_index_is_malformed() {
        assert!(fabric_installer_url(&json!([])).is_err());
        assert!(fabric_installer_url(&json!({ "url": "x" })).is_err());
        assert!(quilt_installer_url(&json!([])).is_err());
        assert!(quilt_installer_url(&json!({ "url": "x" })).is_err());
        // An entry with no URL is malformed rather than skipped: silently
        // taking the next one would install an installer nobody chose.
        assert!(quilt_installer_url(&json!([{ "version": "0.15.1" }])).is_err());
    }

    /// Quilt's index carries no `stable` flag, so the first entry wins.
    ///
    /// The fixture is the real shape from `meta.quiltmc.org/v3`, newest first,
    /// with the `maven`/`hashes` fields left in — a trimmed fixture would not
    /// catch a rule that started depending on one of them.
    #[test]
    fn the_newest_quilt_installer_wins_because_none_is_marked_stable() {
        let index = json!([
            {
                "maven": "org.quiltmc:quilt-installer:0.15.1",
                "version": "0.15.1",
                "url": "https://maven.quiltmc.org/repository/release/org/quiltmc/quilt-installer/0.15.1/quilt-installer-0.15.1.jar",
                "file_size": 8734533,
                "hashes": { "sha1": "a6ea7a9e08e6f5ca399ea12101fbe56195a445e3" }
            },
            {
                "maven": "org.quiltmc:quilt-installer:0.15.0",
                "version": "0.15.0",
                "url": "https://maven.quiltmc.org/repository/release/org/quiltmc/quilt-installer/0.15.0/quilt-installer-0.15.0.jar"
            }
        ]);

        assert_eq!(
            quilt_installer_url(&index).unwrap(),
            "https://maven.quiltmc.org/repository/release/org/quiltmc/quilt-installer/0.15.1/quilt-installer-0.15.1.jar"
        );
    }

    /// Fabric's rule must not be applied to Quilt's data.
    ///
    /// Both would return the first entry here, so the assertion that carries
    /// weight is the *reason*: Fabric's index marks stability and Quilt's does
    /// not, and a shared implementation would look like it was choosing while
    /// always falling through.
    #[test]
    fn quilt_and_fabric_do_not_share_a_selection_rule() {
        // Fabric's index: the stable entry wins even though it is not first.
        assert_eq!(
            fabric_installer_url(&installer_index()).unwrap(),
            "https://maven.fabricmc.net/installer-1.0.1.jar"
        );
        // The same index read Quilt's way takes the first entry instead.
        assert_eq!(
            quilt_installer_url(&installer_index()).unwrap(),
            "https://maven.fabricmc.net/installer-1.0.3.jar"
        );
    }

    /// Quilt launches off a jar manifest, like Fabric and unlike Forge.
    #[test]
    fn quilt_has_a_launch_jar_of_its_own() {
        assert_eq!(launch_jar(Loader::Quilt), Some("quilt-server-launch.jar"));
        // Already swept before Quilt could be hosted, so a directory left by a
        // desktop install was always cleaned correctly.
        assert!(files_to_clean(&[]).contains(&"quilt-server-launch.jar".to_string()));
    }

    /// Quilt has no versioned installer URL to build: the index names it.
    #[test]
    fn quilt_is_refused_a_versioned_installer_url_like_fabric_is() {
        assert!(installer_url(Loader::Quilt, "0.15.1").is_err());
    }

    /// Quilt trails Minecraft, so "has Quilt mapped this yet" is asked before
    /// the installer is ever run.
    #[test]
    fn quilt_refuses_a_minecraft_version_it_has_not_mapped() {
        // A real response shape: a non-empty list means mapped.
        let mapped = json!([{ "maven": "org.quiltmc:hashed:1.21.4", "version": "1.21.4" }]);
        assert!(ensure_quilt_supports("1.21.4", &mapped).is_ok());

        // Everything else is a no, including the shapes a failed request
        // produces. Guessing yes here runs an installer that cannot succeed.
        for unmapped in [json!([]), json!(null), json!({}), json!("nope")] {
            let err = ensure_quilt_supports("26.2", &unmapped).unwrap_err();
            let text = format!("{err}");
            assert!(text.contains("26.2"), "{unmapped}: {text}");
            // The two things a player can do about it.
            assert!(text.contains("Fabric"), "{unmapped}: {text}");
            assert!(text.contains("older"), "{unmapped}: {text}");
        }
    }

    /// The version reaches the URL encoded the same way the desktop encodes it.
    #[test]
    fn the_intermediary_url_encodes_its_version() {
        assert_eq!(
            quilt_intermediary_url("1.21.4"),
            "https://meta.quiltmc.org/v3/versions/intermediary/1.21.4"
        );
        // Snapshots are plain, but a version with anything reserved in it must
        // not split the path.
        assert_eq!(
            quilt_intermediary_url("1.21 pre/1"),
            "https://meta.quiltmc.org/v3/versions/intermediary/1.21%20pre%2F1"
        );
    }

    fn installed(loader: &str, mc: &str, lv: Option<&str>) -> Installed {
        Installed {
            loader: loader.into(),
            mc_version: mc.into(),
            loader_version: lv.map(str::to_string),
        }
    }

    fn metadata(versions: &[&str]) -> String {
        let body: String = versions
            .iter()
            .map(|v| format!("    <version>{v}</version>\n"))
            .collect();
        format!("<metadata><versioning><versions>\n{body}</versions></versioning></metadata>")
    }

    #[test]
    fn neoforge_matches_a_minecraft_version_to_its_build_prefix() {
        let xml = metadata(&["21.1.90", "21.4.155", "21.4.157", "21.5.1"]);
        assert_eq!(
            resolve_neoforge_version(&xml, "1.21.4", None).unwrap(),
            "21.4.157"
        );
        assert_eq!(
            resolve_neoforge_version(&xml, "1.21.1", None).unwrap(),
            "21.1.90"
        );
        assert!(resolve_neoforge_version(&xml, "1.20.1", None).is_err());
    }

    /// `1.21` has no patch component, and NeoForge numbers it `21.0.`.
    #[test]
    fn a_minecraft_version_with_no_patch_still_resolves() {
        let xml = metadata(&["21.0.167", "21.1.1"]);
        assert_eq!(
            resolve_neoforge_version(&xml, "1.21", None).unwrap(),
            "21.0.167"
        );
    }

    #[test]
    fn a_pin_wins_when_it_exists_and_the_newest_wins_when_it_does_not() {
        let xml = metadata(&["21.4.150", "21.4.157"]);
        assert_eq!(
            resolve_neoforge_version(&xml, "1.21.4", Some("21.4.150")).unwrap(),
            "21.4.150"
        );
        // A pin that no longer exists must not stop a server starting.
        assert_eq!(
            resolve_neoforge_version(&xml, "1.21.4", Some("21.4.999")).unwrap(),
            "21.4.157"
        );
    }

    /// Packs express the same Forge build three different ways, and all three
    /// have to resolve to it.
    #[test]
    fn every_spelling_of_a_forge_pin_resolves() {
        let xml = metadata(&["1.20.1-47.2.17", "1.20.1-47.4.20"]);
        for pin in ["47.2.17", "1.20.1-47.2.17"] {
            assert_eq!(
                resolve_forge_version(&xml, "1.20.1", Some(pin)).unwrap(),
                "1.20.1-47.2.17",
                "pin {pin}"
            );
        }

        // Old Minecraft's doubled suffix, matched as a prefix.
        let old = metadata(&["1.7.10-10.13.4.1614-1.7.10"]);
        assert_eq!(
            resolve_forge_version(&old, "1.7.10", Some("10.13.4.1614")).unwrap(),
            "1.7.10-10.13.4.1614-1.7.10"
        );
    }

    /// maven-metadata.xml is not reliably newest-last, and taking the last
    /// element picks a broken build. Position must not decide this.
    #[test]
    fn the_newest_forge_build_is_computed_not_assumed_from_order() {
        let out_of_order = metadata(&[
            "1.7.10-10.13.4.1614-1.7.10",
            "1.7.10-10.13.2.1291-1.7.10",
            "1.7.10-10.13.0.1150",
        ]);
        assert_eq!(
            resolve_forge_version(&out_of_order, "1.7.10", None).unwrap(),
            "1.7.10-10.13.4.1614-1.7.10",
            "the last element is the ancient build whose LaunchWrapper crashes"
        );
    }

    /// Against a slice of NeoForge's real `maven-metadata.xml`.
    ///
    /// Two things it records that a synthetic fixture would not have shown.
    ///
    /// **Most NeoForge versions are betas** — 1087 of 1660 when this was
    /// captured — and the early builds for a Minecraft version are *all*
    /// beta (`21.4.0-beta` upwards) with stable builds appearing later at
    /// higher numbers. So "highest number" lands on a stable build without
    /// needing to know what a beta is, which is why neither this nor the
    /// desktop filters them.
    ///
    /// **The document is not globally newest-last.** The real tail runs
    /// `…26.2.0.59`, `26.1.2.95`. Within one Minecraft version's group it
    /// happened to be ordered, so the desktop's `matching[length - 1]` gets
    /// the same answer here — but it is relying on something the file does not
    /// promise, and `newer()` is not.
    #[test]
    fn real_neoforge_metadata_resolves_to_the_build_that_was_installed() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../shared/fixtures/loaders/neoforge-maven-metadata.xml"
        );
        let xml = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));

        assert_eq!(
            resolve_neoforge_version(&xml, "1.21.4", None).unwrap(),
            "21.4.157",
            "the build a real installer run used"
        );
        assert!(
            maven_versions(&xml).iter().any(|v| v.ends_with("-beta")),
            "the slice must keep betas, or it stops testing anything"
        );
    }

    #[test]
    fn an_installer_url_is_built_per_loader_and_refused_for_the_rest() {
        assert!(installer_url(Loader::NeoForge, "21.4.157")
            .unwrap()
            .ends_with("/neoforge-21.4.157-installer.jar"));
        assert!(installer_url(Loader::Forge, "1.20.1-47.2.17")
            .unwrap()
            .ends_with("/forge-1.20.1-47.2.17-installer.jar"));
        assert!(installer_url(Loader::Fabric, "1.1.2").is_err());
    }

    #[test]
    fn nothing_installed_needs_installing() {
        assert!(needs_reinstall(None, Loader::Fabric, "1.21.4", None));
    }

    #[test]
    fn the_same_loader_and_version_is_left_alone() {
        let have = installed("fabric", "1.21.4", None);
        assert!(!needs_reinstall(
            Some(&have),
            Loader::Fabric,
            "1.21.4",
            None
        ));
    }

    #[test]
    fn a_changed_loader_or_minecraft_version_forces_a_reinstall() {
        let have = installed("fabric", "1.21.4", None);
        assert!(needs_reinstall(
            Some(&have),
            Loader::Vanilla,
            "1.21.4",
            None
        ));
        assert!(needs_reinstall(Some(&have), Loader::Fabric, "1.21.5", None));
    }

    /// A modpack pinned to one loader build must not be run on another: its
    /// mixins target that build's patched classes and injection fails at boot.
    #[test]
    fn a_changed_pin_forces_a_reinstall_but_no_pin_never_does() {
        let pinned = installed("fabric", "1.21.4", Some("0.16.9"));
        assert!(needs_reinstall(
            Some(&pinned),
            Loader::Fabric,
            "1.21.4",
            Some("0.16.10")
        ));
        assert!(!needs_reinstall(
            Some(&pinned),
            Loader::Fabric,
            "1.21.4",
            Some("0.16.9")
        ));
        // Unpinned keeps what it has rather than chasing the newest loader.
        assert!(!needs_reinstall(
            Some(&pinned),
            Loader::Fabric,
            "1.21.4",
            None
        ));
        // Newly pinned, having installed unpinned: the pin has to be honoured.
        let unpinned = installed("fabric", "1.21.4", None);
        assert!(needs_reinstall(
            Some(&unpinned),
            Loader::Fabric,
            "1.21.4",
            Some("0.16.9")
        ));
    }

    #[test]
    fn cleaning_always_removes_the_fixed_set_and_the_libraries_tree() {
        let clean = files_to_clean(&[]);
        for expected in [
            "run.sh",
            "user_jvm_args.txt",
            "server.jar",
            "fabric-server-launch.jar",
            ".homerun-loader.json",
            "libraries",
        ] {
            assert!(
                clean.iter().any(|c| c == expected),
                "missing {expected}: {clean:?}"
            );
        }
    }

    /// Switching a downloaded-jar server to a loader deletes `server.jar`, so
    /// the marker describing it has to go with it — otherwise a switch back
    /// pays for a digest to discover the jar it names is not there.
    #[test]
    fn cleaning_removes_this_hosts_downloaded_jar_marker_too() {
        assert!(files_to_clean(&[]).iter().any(|c| c == "homerun-jar.json"));
    }

    /// A directory restored from a desktop backup can carry a loader this
    /// build cannot install. Switching it still has to clear the old jars.
    #[test]
    fn cleaning_sweeps_versioned_jars_from_loaders_we_do_not_host() {
        let listing: Vec<String> = [
            "forge-1.20.1-47.2.17.jar",
            "minecraft_server.1.20.1.jar",
            "paper-1.21.4-232.jar",
            "spigot_server-1.20.4.jar",
            "craftbukkit_server-1.20.4.jar",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let clean = files_to_clean(&listing);
        for entry in &listing {
            assert!(clean.contains(entry), "missing {entry}");
        }
    }

    /// The jar that is mid-install must survive a sweep, or a failed install
    /// deletes the installer it was about to run.
    #[test]
    fn cleaning_leaves_installers_and_unrelated_files_alone() {
        let listing: Vec<String> = [
            "forge-1.20.1-47.2.17-installer.jar",
            "_fabric-installer.jar",
            "world",
            "server.properties",
            "some-mod.jar",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let clean = files_to_clean(&listing);
        for entry in &listing {
            assert!(!clean.contains(entry), "should not sweep {entry}");
        }
    }

    #[test]
    fn a_bundler_class_file_names_its_java_version() {
        // class-file major 65 is Java 21, 69 is Java 25.
        assert_eq!(
            bundler_java_major(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 65]),
            Some(21)
        );
        assert_eq!(
            bundler_java_major(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 69]),
            Some(25)
        );
    }

    #[test]
    fn anything_that_is_not_a_class_file_says_so_rather_than_guessing() {
        assert_eq!(bundler_java_major(&[]), None);
        assert_eq!(bundler_java_major(&[0xCA, 0xFE]), None);
        assert_eq!(bundler_java_major(b"PK\x03\x04abcd"), None);
        // A major below 45 predates Java 1.1 and means the bytes are wrong.
        assert_eq!(
            bundler_java_major(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 44]),
            None
        );
    }
}

//! Which server jar to run, and how to know it arrived intact.
//!
//! Reference: `src/electron/mod-installer.ts` in the `homerun` repo.
//!
//! These are pure functions over the JSON the endpoints return. The caller
//! makes the request; this decides what the answer means. That split is what
//! lets the awkward cases — a version that does not exist, a loader with no
//! build for it, an array ordered the other way round — be tested exhaustively
//! without a network.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// Mojang's manifest of every version. Consulted for **every** loader, not
/// just vanilla: it is what turns "latest" into a concrete version, and the
/// only source for the Java level a jar needs.
pub const VERSION_MANIFEST: &str = "https://launchermeta.mojang.com/mc/game/version_manifest.json";

/// PaperMC's v3 builds endpoint. Interpolate the resolved Minecraft version.
pub fn paper_builds_url(version: &str) -> String {
    format!("https://fill.papermc.io/v3/projects/paper/versions/{version}/builds")
}

/// Which server software to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loader {
    Vanilla,
    Paper,
}

impl Loader {
    /// Parse the API's `TYPE` environment variable.
    ///
    /// The desktop accepts fabric, forge, neoforge, quilt, spigot and bukkit
    /// too. Those install by *running* an installer, which is a separate piece
    /// of work, so they are refused by name here rather than silently treated
    /// as vanilla — a Forge server quietly started as vanilla would eat the
    /// world's mods.
    pub fn parse(raw: Option<&str>) -> Result<Loader> {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("") | Some("vanilla") => Ok(Loader::Vanilla),
            Some("paper") => Ok(Loader::Paper),
            Some(other) => Err(Error::Unsupported(format!(
                "Homerun cannot host {other} servers on this device yet — those install by \
                 running an installer. Vanilla and Paper both work, and Paper runs Bukkit \
                 and Spigot plugins."
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Loader::Vanilla => "vanilla",
            Loader::Paper => "paper",
        }
    }
}

/// A digest the publisher gave us, and what produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksum {
    pub algorithm: Algorithm,
    pub hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Algorithm {
    Sha1,
    Sha256,
}

/// One downloadable server jar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub url: String,
    pub loader: String,
    pub version: String,
    pub checksum: Option<Checksum>,
    /// The class-file level the jar needs, from Mojang's version metadata.
    pub required_java: u16,
    pub size_bytes: Option<u64>,
}

/// What is on disk, so a restart is free and a version change is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnDisk {
    pub loader: String,
    pub version: String,
    #[serde(default)]
    pub checksum: Option<String>,
}

impl OnDisk {
    /// Is the jar on disk exactly the one [`Artifact`] describes?
    pub fn satisfies(&self, artifact: &Artifact) -> bool {
        self.loader == artifact.loader
            && self.version == artifact.version
            && self.checksum == artifact.checksum.as_ref().map(|c| c.hex.clone())
    }

    /// Loose enough for the offline fallback: any build of the right thing.
    ///
    /// Hosting on a LAN with no internet is a real thing to want, and refusing
    /// to start a world that is already downloaded would be worse than
    /// starting it.
    pub fn could_satisfy(&self, version: Option<&str>, loader: Loader) -> bool {
        if self.loader != loader.as_str() {
            return false;
        }
        match version.map(str::trim) {
            None | Some("") => true,
            Some(v) if v.eq_ignore_ascii_case("LATEST") => true,
            Some(v) => self.version == v,
        }
    }
}

/// Everything before 1.17 predates `javaVersion`, and runs on anything modern.
/// 21 is the desktop's floor for the same reason.
const DEFAULT_REQUIRED_JAVA: u16 = 21;

/// Pick the version to run out of Mojang's manifest.
///
/// `None`, an empty string, or `LATEST` all mean the latest *release* — never
/// a snapshot, which is what `latest.release` gives and `latest.snapshot`
/// would not.
pub fn resolve_version(manifest: &serde_json::Value, requested: Option<&str>) -> Result<String> {
    let wanted = match requested.map(str::trim) {
        Some(v) if !v.is_empty() && !v.eq_ignore_ascii_case("LATEST") => v.to_string(),
        _ => manifest
            .get("latest")
            .and_then(|l| l.get("release"))
            .and_then(|r| r.as_str())
            .ok_or_else(|| Error::Malformed("the version manifest names no latest release".into()))?
            .to_string(),
    };

    let known = manifest
        .get("versions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Malformed("the version manifest has no versions".into()))?
        .iter()
        .any(|v| v.get("id").and_then(|i| i.as_str()) == Some(wanted.as_str()));

    if !known {
        return Err(Error::Malformed(format!(
            "Minecraft {wanted} is not in the version manifest"
        )));
    }
    Ok(wanted)
}

/// The metadata URL for a resolved version, so the caller can fetch it.
pub fn version_metadata_url(manifest: &serde_json::Value, version: &str) -> Result<String> {
    manifest
        .get("versions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Malformed("the version manifest has no versions".into()))?
        .iter()
        .find(|v| v.get("id").and_then(|i| i.as_str()) == Some(version))
        .and_then(|v| v.get("url"))
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .ok_or_else(|| Error::Malformed(format!("Minecraft {version} has no metadata URL")))
}

/// The vanilla server jar, from a version's metadata document.
pub fn vanilla(metadata: &serde_json::Value, version: &str) -> Result<Artifact> {
    let server = metadata
        .get("downloads")
        .and_then(|d| d.get("server"))
        .ok_or_else(|| {
            Error::Malformed(format!("Minecraft {version} publishes no server download"))
        })?;

    let url = server
        .get("url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| Error::Malformed(format!("Minecraft {version} has no server jar URL")))?;

    Ok(Artifact {
        url: url.to_string(),
        loader: Loader::Vanilla.as_str().to_string(),
        version: version.to_string(),
        checksum: server
            .get("sha1")
            .and_then(|s| s.as_str())
            .map(|hex| Checksum {
                algorithm: Algorithm::Sha1,
                hex: hex.to_string(),
            }),
        required_java: metadata
            .get("javaVersion")
            .and_then(|j| j.get("majorVersion"))
            .and_then(|m| m.as_u64())
            .map(|m| m as u16)
            .unwrap_or(DEFAULT_REQUIRED_JAVA),
        size_bytes: server.get("size").and_then(|s| s.as_u64()),
    })
}

/// Paper for an already-resolved Minecraft version.
///
/// # The ordering trap
///
/// **The v3 API returns builds newest-first**, and the array carries every
/// experimental build ever cut for the version. The desktop takes
/// `allBuilds[allBuilds.length - 1]`, which on this API is build 1 — an alpha.
/// That is a live bug in the shipping desktop app, not a hypothetical: the
/// test below is built from a real response and asserts we pick 232 STABLE
/// where the desktop picks 1 ALPHA.
///
/// Position is never trusted here. The highest **stable** id wins; if a
/// version has no stable build yet, the highest id of any channel does, so a
/// brand-new Minecraft release is still hostable.
pub fn paper(builds: &serde_json::Value, version: &str, required_java: u16) -> Result<Artifact> {
    let all = builds
        .as_array()
        .ok_or_else(|| Error::Malformed("the Paper builds response is not a list".into()))?;

    if all.is_empty() {
        return Err(Error::Malformed(format!(
            "Paper has no build for Minecraft {version} yet."
        )));
    }

    let id_of = |b: &serde_json::Value| b.get("id").and_then(|i| i.as_i64()).unwrap_or(i64::MIN);
    let is_stable = |b: &&serde_json::Value| {
        b.get("channel")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.eq_ignore_ascii_case("STABLE"))
    };

    let build = all
        .iter()
        .filter(is_stable)
        .max_by_key(|b| id_of(b))
        .or_else(|| all.iter().max_by_key(|b| id_of(b)))
        .ok_or_else(|| Error::Malformed(format!("no usable Paper build for {version}")))?;

    let download = build
        .get("downloads")
        .and_then(|d| d.get("server:default"))
        .ok_or_else(|| {
            Error::Malformed(format!(
                "Paper build {} publishes no server download",
                id_of(build)
            ))
        })?;

    Ok(Artifact {
        url: download
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| {
                Error::Malformed(format!("Paper build {} has no download URL", id_of(build)))
            })?
            .to_string(),
        loader: Loader::Paper.as_str().to_string(),
        version: version.to_string(),
        // Paper publishes a SHA-256 the desktop fetches and discards. Reading
        // it costs nothing and is the difference between a verified download
        // and a hopeful one.
        checksum: download
            .get("checksums")
            .and_then(|c| c.get("sha256"))
            .and_then(|s| s.as_str())
            .map(|hex| Checksum {
                algorithm: Algorithm::Sha256,
                hex: hex.to_string(),
            }),
        required_java,
        size_bytes: download.get("size").and_then(|s| s.as_u64()),
    })
}

/// Can the runtime this build ships actually run that jar?
///
/// Answering before launch turns a cryptic `UnsupportedClassVersionError` deep
/// in a JVM log into a sentence the player can act on.
pub fn check_java(artifact: &Artifact, bundled_java: Option<u16>) -> Result<()> {
    match bundled_java {
        Some(have) if artifact.required_java > have => Err(Error::Unsupported(format!(
            "Minecraft {} needs Java {}, and this version of Homerun ships Java {}. \
             Update the app, or choose an older Minecraft version.",
            artifact.version, artifact.required_java, have
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> serde_json::Value {
        json!({
            "latest": { "release": "1.21.4", "snapshot": "25w03a" },
            "versions": [
                { "id": "25w03a", "type": "snapshot", "url": "https://example/25w03a.json" },
                { "id": "1.21.4", "type": "release",  "url": "https://example/1.21.4.json" },
                { "id": "1.20.4", "type": "release",  "url": "https://example/1.20.4.json" },
            ]
        })
    }

    #[test]
    fn an_explicit_version_is_honoured() {
        assert_eq!(
            resolve_version(&manifest(), Some("1.20.4")).unwrap(),
            "1.20.4"
        );
    }

    #[test]
    fn absent_blank_and_latest_all_mean_the_latest_release() {
        for requested in [None, Some(""), Some("LATEST"), Some("latest")] {
            assert_eq!(
                resolve_version(&manifest(), requested).unwrap(),
                "1.21.4",
                "requested {requested:?}"
            );
        }
    }

    /// A snapshot is newer than the release, and must never be chosen for
    /// someone who asked for "latest".
    #[test]
    fn latest_never_resolves_to_a_snapshot() {
        assert_eq!(resolve_version(&manifest(), None).unwrap(), "1.21.4");
    }

    #[test]
    fn an_unknown_version_is_refused_by_name() {
        let err = resolve_version(&manifest(), Some("1.99.9")).unwrap_err();
        assert!(format!("{err}").contains("1.99.9"), "{err}");
    }

    #[test]
    fn vanilla_reads_url_sha1_size_and_java() {
        let metadata = json!({
            "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
            "downloads": { "server": {
                "url": "https://piston-data/server.jar",
                "sha1": "823e2250d24b3ddac457a60c92a6a941943fcd6a",
                "size": 60894273u64
            }}
        });
        let artifact = vanilla(&metadata, "1.21.4").unwrap();
        assert_eq!(artifact.url, "https://piston-data/server.jar");
        assert_eq!(artifact.required_java, 21);
        assert_eq!(artifact.size_bytes, Some(60894273));
        assert_eq!(artifact.checksum.unwrap().algorithm, Algorithm::Sha1);
    }

    #[test]
    fn a_version_predating_javaversion_falls_back_to_21() {
        let metadata = json!({
            "downloads": { "server": { "url": "https://piston-data/old.jar" }}
        });
        assert_eq!(vanilla(&metadata, "1.12.2").unwrap().required_java, 21);
    }

    #[test]
    fn a_version_with_no_server_download_is_refused() {
        let metadata = json!({ "downloads": { "client": { "url": "https://x" } } });
        assert!(vanilla(&metadata, "1.5.2").is_err());
    }

    /// Shaped like the real response: **newest first**, mixed channels.
    fn paper_builds() -> serde_json::Value {
        json!([
            { "id": 232, "channel": "STABLE", "downloads": { "server:default": {
                "name": "paper-1.21.4-232.jar",
                "url": "https://fill-data/paper-232.jar",
                "size": 51437498u64,
                "checksums": { "sha256": "5ee4f542f628a14c644410b08c94ea42e772ef4d29fe92973636b6813d4eaffc" }
            }}},
            { "id": 231, "channel": "STABLE", "downloads": { "server:default": {
                "url": "https://fill-data/paper-231.jar", "checksums": { "sha256": "aaa" }
            }}},
            { "id": 1, "channel": "ALPHA", "downloads": { "server:default": {
                "url": "https://fill-data/paper-1.jar", "checksums": { "sha256": "zzz" }
            }}},
        ])
    }

    /// The desktop bug, pinned. `allBuilds[allBuilds.length - 1]` returns the
    /// ALPHA build 1 against this array; the highest stable id is 232.
    #[test]
    fn paper_picks_the_newest_stable_not_the_last_element() {
        let artifact = paper(&paper_builds(), "1.21.4", 21).unwrap();
        assert_eq!(artifact.url, "https://fill-data/paper-232.jar");
        assert_eq!(
            artifact.checksum.unwrap().hex,
            "5ee4f542f628a14c644410b08c94ea42e772ef4d29fe92973636b6813d4eaffc"
        );
    }

    /// The desktop's algorithm, written out, so the divergence is executable
    /// rather than a claim in a comment. If PaperMC ever flips the ordering
    /// back this test starts failing, which is exactly when someone should
    /// look at it again.
    #[test]
    fn the_desktop_expression_would_pick_an_alpha() {
        let builds = paper_builds();
        let all = builds.as_array().unwrap();
        let desktop_choice = all.last().unwrap();

        assert_eq!(desktop_choice.get("channel").unwrap(), "ALPHA");
        assert_eq!(desktop_choice.get("id").unwrap(), 1);

        let ours = paper(&builds, "1.21.4", 21).unwrap();
        assert!(
            !ours.url.contains("paper-1.jar"),
            "we picked the same alpha the desktop does"
        );
    }

    /// Position must not matter at all, so the same input reversed must give
    /// the same answer.
    #[test]
    fn paper_is_insensitive_to_array_order() {
        let mut reversed = paper_builds().as_array().unwrap().clone();
        reversed.reverse();
        let from_reversed = paper(&serde_json::Value::Array(reversed), "1.21.4", 21).unwrap();
        assert_eq!(from_reversed, paper(&paper_builds(), "1.21.4", 21).unwrap());
    }

    /// A brand-new Minecraft release often has only experimental builds. Being
    /// unable to host it at all would be worse than hosting an alpha.
    #[test]
    fn paper_falls_back_to_experimental_when_nothing_is_stable() {
        let builds = json!([
            { "id": 9, "channel": "ALPHA", "downloads": { "server:default": {
                "url": "https://fill-data/paper-9.jar" }}},
            { "id": 3, "channel": "ALPHA", "downloads": { "server:default": {
                "url": "https://fill-data/paper-3.jar" }}},
        ]);
        assert_eq!(
            paper(&builds, "1.99.0", 21).unwrap().url,
            "https://fill-data/paper-9.jar"
        );
    }

    #[test]
    fn paper_with_no_builds_says_so() {
        let err = paper(&json!([]), "1.99.0", 21).unwrap_err();
        assert!(format!("{err}").contains("1.99.0"), "{err}");
    }

    /// Paper carries the Java requirement of the Minecraft version it targets;
    /// its own response never states one.
    #[test]
    fn paper_inherits_the_required_java_it_is_given() {
        assert_eq!(
            paper(&paper_builds(), "1.21.4", 25).unwrap().required_java,
            25
        );
    }

    #[test]
    fn loaders_that_need_an_installer_are_refused_by_name() {
        for loader in ["fabric", "forge", "neoforge", "quilt", "spigot", "bukkit"] {
            let err = Loader::parse(Some(loader)).unwrap_err();
            assert!(format!("{err}").contains(loader), "{loader}: {err}");
        }
    }

    #[test]
    fn absent_or_blank_type_is_vanilla() {
        assert_eq!(Loader::parse(None).unwrap(), Loader::Vanilla);
        assert_eq!(Loader::parse(Some("")).unwrap(), Loader::Vanilla);
        assert_eq!(Loader::parse(Some("  ")).unwrap(), Loader::Vanilla);
        assert_eq!(Loader::parse(Some("VANILLA")).unwrap(), Loader::Vanilla);
        assert_eq!(Loader::parse(Some("Paper")).unwrap(), Loader::Paper);
    }

    #[test]
    fn a_newer_jar_than_the_bundled_runtime_is_refused_before_launch() {
        let artifact = Artifact {
            url: "https://x".into(),
            loader: "vanilla".into(),
            version: "26.2".into(),
            checksum: None,
            required_java: 25,
            size_bytes: None,
        };
        let err = check_java(&artifact, Some(21)).unwrap_err();
        assert!(format!("{err}").contains("needs Java 25"), "{err}");
        assert!(format!("{err}").contains("ships Java 21"), "{err}");

        check_java(&artifact, Some(25)).expect("exactly enough is enough");
        check_java(&artifact, Some(26)).expect("newer is fine");
        check_java(&artifact, None).expect("unknown runtime cannot be judged");
    }

    #[test]
    fn on_disk_matches_only_the_exact_artifact() {
        let artifact = Artifact {
            url: "https://x".into(),
            loader: "vanilla".into(),
            version: "1.21.4".into(),
            checksum: Some(Checksum {
                algorithm: Algorithm::Sha1,
                hex: "abc".into(),
            }),
            required_java: 21,
            size_bytes: None,
        };
        let matching = OnDisk {
            loader: "vanilla".into(),
            version: "1.21.4".into(),
            checksum: Some("abc".into()),
        };
        assert!(matching.satisfies(&artifact));

        // A Paper rebuild of the same Minecraft version is a different jar.
        let stale_digest = OnDisk {
            checksum: Some("def".into()),
            ..matching.clone()
        };
        assert!(!stale_digest.satisfies(&artifact));

        let other_loader = OnDisk {
            loader: "paper".into(),
            ..matching.clone()
        };
        assert!(!other_loader.satisfies(&artifact));

        let other_version = OnDisk {
            version: "1.20.4".into(),
            ..matching
        };
        assert!(!other_version.satisfies(&artifact));
    }

    #[test]
    fn the_offline_fallback_accepts_any_build_of_the_right_thing() {
        let on_disk = OnDisk {
            loader: "vanilla".into(),
            version: "1.21.4".into(),
            checksum: Some("abc".into()),
        };
        assert!(on_disk.could_satisfy(None, Loader::Vanilla));
        assert!(on_disk.could_satisfy(Some("LATEST"), Loader::Vanilla));
        assert!(on_disk.could_satisfy(Some("1.21.4"), Loader::Vanilla));
        assert!(!on_disk.could_satisfy(Some("1.20.4"), Loader::Vanilla));
        assert!(!on_disk.could_satisfy(None, Loader::Paper));
    }
}

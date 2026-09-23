//! Where a game's server comes from, decided but not done.
//!
//! # We never host a game's server
//!
//! Every plan here points at the vendor. Homerun downloads a game's server
//! onto the player's own machine, after that player has accepted the game's
//! terms; it does not mirror, repackage or redistribute one. `steamcmd`
//! itself is fetched from Valve at first use for the same reason.
//!
//! # Anonymous steamcmd only
//!
//! A game whose dedicated server needs a Steam account that *owns* it is out
//! of scope — not a feature request. There is no account we could use that
//! would not be either a shared credential or the player's own, and neither
//! is something to build on.
//!
//! # What "already present" is allowed to mean
//!
//! A pinned build that is on disk is finished work, and skipping it is the
//! difference between a ten-second launch and a nine-gigabyte one. An
//! *unpinned* steamcmd runtime is never already present: the descriptor says
//! "whatever is current", only Steam knows what that is, and a game that
//! force-updates will refuse every client the moment it is behind. steamcmd
//! is cheap when it has nothing to do, so it runs.
//!
//! # Updating and verifying are different questions
//!
//! `app_update` asks Steam what changed and fetches that. `validate` rereads
//! every file on disk and checksums it. Running both on every start made the
//! second the expensive one by a distance: the Rust pilot measured a
//! full re-verify of 5,869,171,402 bytes, minutes per start, on a runtime
//! that was already complete and already current.
//!
//! So they are decided separately. An update check still runs every time an
//! unpinned runtime is launched, because that is what a force-updating game
//! requires. A **verify** runs only when there is a reason to doubt what is
//! on disk: nothing recorded there yet (a first install, or an install that
//! was interrupted before it could stamp), or a host that has one --
//! [`Present::suspect`].
//!
//! The risk this accepts is a runtime that is quietly corrupt in a way Steam
//! believes is current. That is what the suspect flag and a repair are for,
//! and it costs one slow start rather than every start.
//!
//! # A vendor runtime is one directory per version
//!
//! A [`RuntimeSource::Vendor`] runtime is downloaded in the version the
//! player chose, which the host resolves and passes in -- this module never
//! lists or resolves versions. Each version lives in its own directory,
//! `<runtimeRoot>/<id>/<version>`, and is stamped `v<version>`, so a version
//! already on disk is [`Plan::AlreadyPresent`] and switching back to it is
//! free. The version is refused unless it is dotted digits
//! ([`check_runtime_version`]): it becomes part of a URL and of a directory
//! name, and neither is a place for anything a player could shape.
//!
//! What such a runtime is checked against, since no digest can be pinned for
//! a version released after the descriptor was signed, is described on
//! [`RuntimeSource::Vendor`]; the per-machine record of first-seen digests
//! is [`VENDOR_HASHES`], beside the version directories.

use serde::{Deserialize, Serialize};

use super::descriptor::{Extract, GameDescriptor, RuntimeSource};
use crate::{Error, Result};

/// What is already in the runtime directory.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Present {
    /// The build id recorded when this directory was last filled. `None`
    /// means nothing is there, or nothing that said what it was.
    #[serde(default)]
    pub build_id: Option<String>,
    /// The host has reason to believe what is on disk is damaged.
    ///
    /// An observation rather than an instruction: a launch that failed in a
    /// way that looks like a broken install, or a person who asked for a
    /// repair. It is what turns an ordinary update into a full verify, and
    /// it is the only thing that does so once a runtime has been stamped.
    #[serde(default)]
    pub suspect: bool,
}

/// How to get the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Plan {
    /// Nothing to do. The build on disk is the build the descriptor pins.
    #[serde(rename_all = "camelCase")]
    AlreadyPresent { dir: String, build_id: String },
    /// One URL, pinned by digest.
    #[serde(rename_all = "camelCase")]
    Direct {
        dir: String,
        url: String,
        /// Lowercase hex. The build id is its first 12 characters, so the id
        /// a manifest records and the digest it was verified against cannot
        /// disagree.
        sha256: String,
        #[serde(default)]
        size: Option<u64>,
        extract: Extract,
        /// Leading path components dropped from each zip entry.
        #[serde(default)]
        strip_components: u32,
    },
    /// The vendor's own site, in the version the host chose.
    #[serde(rename_all = "camelCase")]
    Vendor {
        /// `<runtimeRoot>/<id>/<version>`.
        dir: String,
        /// The descriptor's pattern with the version substituted. Its origin
        /// is the only one a redirect may lead to.
        url: String,
        version: String,
        #[serde(default)]
        size: Option<u64>,
        #[serde(default)]
        strip_components: u32,
        /// The file recording the sha256 of the first download of each
        /// version on this machine: `<runtimeRoot>/<id>/.vendor-hashes.json`.
        record: String,
    },
    /// Valve's `steamcmd`, anonymous.
    #[serde(rename_all = "camelCase")]
    SteamCmd {
        dir: String,
        app_id: u32,
        /// `None` is "whatever is current" — see the module header.
        #[serde(default)]
        build_id: Option<String>,
        /// Re-read and checksum every file, rather than only fetching what
        /// changed. Minutes on a large game, so it is asked for rather than
        /// assumed — see the module header.
        #[serde(default = "verify_by_default")]
        verify: bool,
    },
}

impl Plan {
    /// Where the runtime lives once this plan has run.
    pub fn dir(&self) -> &str {
        match self {
            Plan::AlreadyPresent { dir, .. }
            | Plan::Direct { dir, .. }
            | Plan::Vendor { dir, .. }
            | Plan::SteamCmd { dir, .. } => dir,
        }
    }
}

// Plans serialized before this field existed always verified Steam installs.
fn verify_by_default() -> bool {
    true
}

/// The build id a direct download is known by.
///
/// The digest prefix, so that the id in a manifest and the bytes it describes
/// cannot drift apart — the same rule the desktop artifacts follow.
pub fn direct_build_id(sha256: &str) -> String {
    sha256.chars().take(12).collect()
}

/// The placeholder a vendor URL carries for the version as written.
pub const VERSION_PLACEHOLDER: &str = "{version}";
/// The placeholder for the version with its dots removed: `1.4.5.8` is
/// `1458`, which is how Terraria names its server downloads.
pub const VERSION_DIGITS_PLACEHOLDER: &str = "{versionDigits}";
/// The per-game file, beside the version directories, recording the sha256
/// of the first download of each version on this machine.
pub const VENDOR_HASHES: &str = ".vendor-hashes.json";

/// The build id a vendor runtime is stamped with.
pub fn vendor_build_id(version: &str) -> String {
    format!("v{version}")
}

/// Refuse a runtime version that is not dotted digits.
///
/// `^[0-9]+(\.[0-9]+){0,5}$`, and nothing more generous: the version is
/// substituted into a URL and becomes a directory name, so a `/`, a `..`, a
/// `?` or a percent sign in it would be somebody else's decision about where
/// a download comes from or lands.
pub fn check_runtime_version(version: &str) -> Result<()> {
    let parts: Vec<&str> = version.split('.').collect();
    let ok = version.len() <= 64
        && parts.len() <= 6
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(Error::Malformed(format!(
            "\"{}\" is not a version Homerun can download: a version is numbers \
             separated by dots, such as 1.4.5.8.",
            version.chars().take(64).collect::<String>()
        )))
    }
}

/// Whether a vendor address is one Homerun will download from.
///
/// HTTPS, always, in a build that ships. A build with debug assertions -- the
/// test suites, and nothing a player runs -- also accepts plain HTTP to
/// `127.0.0.1`, so the whole path can be exercised against a local server
/// without a certificate. Nothing off this machine is reachable that way.
pub fn vendor_scheme_allowed(url: &str) -> bool {
    url.starts_with("https://") || (cfg!(debug_assertions) && url.starts_with("http://127.0.0.1:"))
}

/// Why a vendor URL pattern is not one, or `None` when it is.
///
/// It needs a version placeholder, may carry no other, and must keep them
/// out of the scheme and host: the version decides *which* file, never
/// *whose* site. Returned as a phrase for [`super::validate`] to put in a
/// sentence.
pub fn vendor_url_problem(pattern: &str) -> Option<String> {
    let Some((_, rest)) = pattern.split_once("://") else {
        return Some("is not a web address".into());
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    if rest[..authority_end].contains(['{', '}']) {
        return Some("puts the version in the site's name rather than in the path".into());
    }
    let mut seen_version = false;
    let mut remaining = pattern;
    while let Some(open) = remaining.find('{') {
        if remaining[..open].contains('}') {
            return Some("has a '}' that was never opened".into());
        }
        let after = &remaining[open..];
        let Some(close) = after.find('}') else {
            return Some("has a '{' that is never closed".into());
        };
        let token = &after[..=close];
        if token == VERSION_PLACEHOLDER || token == VERSION_DIGITS_PLACEHOLDER {
            seen_version = true;
        } else {
            return Some(format!(
                "uses {token}, and the only placeholders it may use are \
                 {VERSION_PLACEHOLDER} and {VERSION_DIGITS_PLACEHOLDER}"
            ));
        }
        remaining = &after[close + 1..];
    }
    if remaining.contains('}') {
        return Some("has a '}' that was never opened".into());
    }
    if !seen_version {
        return Some(format!(
            "does not say where the version goes: it needs {VERSION_PLACEHOLDER} or \
             {VERSION_DIGITS_PLACEHOLDER}"
        ));
    }
    None
}

/// A vendor URL pattern with a version filled in.
pub fn vendor_url(pattern: &str, version: &str) -> Result<String> {
    check_runtime_version(version)?;
    if let Some(problem) = vendor_url_problem(pattern) {
        return Err(Error::Malformed(format!(
            "this game's download address {problem}. The file is part of the app, so \
             this is a bug in Homerun rather than something you can fix."
        )));
    }
    let url = pattern
        .replace(VERSION_DIGITS_PLACEHOLDER, &version.replace('.', ""))
        .replace(VERSION_PLACEHOLDER, version);
    if !vendor_scheme_allowed(&url) {
        return Err(Error::Malformed(
            "this game's download address is not a secure (https) one, so Homerun \
             will not download from it."
                .into(),
        ));
    }
    Ok(url)
}

/// Where a game's runtime is cached.
///
/// One directory per game, shared by every server of that game on this
/// machine: a nine-gigabyte install is not copied per server.
pub fn runtime_dir(runtime_root: &str, id: &str) -> String {
    let separator = if runtime_root.contains('\\') {
        '\\'
    } else {
        '/'
    };
    format!(
        "{}{separator}{id}",
        runtime_root.trim_end_matches(['/', '\\'])
    )
}

/// Where this game's runtime lives on this machine, for this version.
///
/// The one answer to "which directory", so fetching, launching, the
/// executable check, a `runtime` working directory and save mounts cannot
/// disagree. `runtime_version` is read only for a vendor runtime, which has
/// one directory per version and cannot be placed without one.
pub fn install_dir(
    descriptor: &GameDescriptor,
    host: &str,
    runtime_root: &str,
    runtime_version: Option<&str>,
) -> Result<String> {
    let game = runtime_dir(runtime_root, &descriptor.id);
    if !is_vendor(descriptor, host) {
        return Ok(game);
    }
    let version = runtime_version.ok_or_else(|| no_version(descriptor))?;
    check_runtime_version(version)?;
    Ok(runtime_dir(&game, version))
}

/// Whether this game's runtime on this host is a vendor download, whose
/// version the host has to choose.
pub fn is_vendor(descriptor: &GameDescriptor, host: &str) -> bool {
    descriptor
        .platform(host)
        .is_some_and(|p| p.runtime.source == RuntimeSource::Vendor)
}

/// Decide how to get this game's server onto this machine.
///
/// For a runtime whose version is not chosen by the player. A vendor runtime
/// is refused here in words; use [`plan_version`].
pub fn plan(
    descriptor: &GameDescriptor,
    host: &str,
    runtime_root: &str,
    present: &Present,
) -> Result<Plan> {
    plan_version(descriptor, host, runtime_root, present, None)
}

/// [`plan`], with the version the host resolved for a vendor runtime.
///
/// `present` must describe [`install_dir`] for the same version. The version
/// is ignored for every other source.
pub fn plan_version(
    descriptor: &GameDescriptor,
    host: &str,
    runtime_root: &str,
    present: &Present,
    runtime_version: Option<&str>,
) -> Result<Plan> {
    let platform = descriptor.platform(host).ok_or_else(|| {
        Error::Unsupported(format!(
            "{} cannot be hosted on this computer.",
            display_name(descriptor)
        ))
    })?;
    let runtime = &platform.runtime;
    let dir = runtime_dir(runtime_root, &descriptor.id);

    match runtime.source {
        RuntimeSource::Direct => {
            let url = runtime
                .url
                .clone()
                .ok_or_else(|| missing(descriptor, "a download address"))?;
            let sha256 = runtime
                .sha256
                .clone()
                .ok_or_else(|| missing(descriptor, "a checksum for its download"))?;
            let build_id = direct_build_id(&sha256);
            if present.build_id.as_deref() == Some(build_id.as_str()) {
                return Ok(Plan::AlreadyPresent { dir, build_id });
            }
            Ok(Plan::Direct {
                dir,
                url,
                sha256,
                size: runtime.size,
                extract: runtime.extract.unwrap_or_default(),
                strip_components: runtime.strip_components.unwrap_or(0),
            })
        }
        RuntimeSource::Vendor => {
            let pattern = runtime
                .url
                .as_deref()
                .ok_or_else(|| missing(descriptor, "a download address"))?;
            let version = runtime_version.ok_or_else(|| no_version(descriptor))?;
            let url = vendor_url(pattern, version)?;
            let version_dir = runtime_dir(&dir, version);
            let build_id = vendor_build_id(version);
            // A suspect install is fetched again, and the recorded digest is
            // what says whether the vendor still serves the same bytes.
            if present.build_id.as_deref() == Some(build_id.as_str()) && !present.suspect {
                return Ok(Plan::AlreadyPresent {
                    dir: version_dir,
                    build_id,
                });
            }
            Ok(Plan::Vendor {
                dir: version_dir,
                url,
                version: version.to_string(),
                size: runtime.size,
                strip_components: runtime.strip_components.unwrap_or(0),
                record: runtime_dir(&dir, VENDOR_HASHES),
            })
        }
        RuntimeSource::Steamcmd => {
            let app_id = runtime
                .app_id
                .ok_or_else(|| missing(descriptor, "a Steam application id"))?;
            if let Some(pinned) = &runtime.build_id {
                if present.build_id.as_deref() == Some(pinned.as_str()) && !present.suspect {
                    return Ok(Plan::AlreadyPresent {
                        dir,
                        build_id: pinned.clone(),
                    });
                }
            }
            Ok(Plan::SteamCmd {
                dir,
                app_id,
                build_id: runtime.build_id.clone(),
                // Nothing recorded means a first install or one that was
                // interrupted before it could say what it was; either way
                // what is on disk is not known to be whole.
                verify: present.build_id.is_none() || present.suspect,
            })
        }
        RuntimeSource::Unknown => Err(Error::Unsupported(format!(
            "{} is downloaded in a way this version of Homerun does not know about. \
             Updating Homerun should fix it.",
            display_name(descriptor)
        ))),
    }
}

fn display_name(descriptor: &GameDescriptor) -> String {
    if descriptor.name.is_empty() {
        "This game".to_string()
    } else {
        descriptor.name.clone()
    }
}

fn no_version(descriptor: &GameDescriptor) -> Error {
    Error::Malformed(format!(
        "{} is downloaded in the version a player chooses, and Homerun was not told \
         which version to use. This is a bug in Homerun rather than something you \
         can fix.",
        display_name(descriptor)
    ))
}

fn missing(descriptor: &GameDescriptor, what: &str) -> Error {
    Error::Malformed(format!(
        "{}'s descriptor does not give {what}, so Homerun cannot download it. \
         The file is part of the app, so this is a bug in Homerun rather than \
         something you can fix.",
        display_name(descriptor)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rust() -> GameDescriptor {
        GameDescriptor::parse(include_str!("testdata/rust.json")).unwrap()
    }

    fn direct(extra: serde_json::Value) -> GameDescriptor {
        let mut runtime = json!({
            "source": "direct",
            "url": "https://terraria.org/server.zip",
            "sha256": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "extract": "zip"
        });
        for (k, v) in extra.as_object().unwrap() {
            runtime[k] = v.clone();
        }
        serde_json::from_value(json!({
            "id": "terraria", "name": "Terraria", "hosts": ["win32-x64"],
            "platforms": { "win32-x64": { "runtime": runtime, "launch": { "exe": "S.exe" } } }
        }))
        .unwrap()
    }

    const NOTHING: Present = Present {
        build_id: None,
        suspect: false,
    };

    #[test]
    fn the_pilot_is_fetched_with_steamcmd_anonymously() {
        let plan = plan(&rust(), "win32-x64", "C:\\rt", &NOTHING).unwrap();
        assert_eq!(
            plan,
            Plan::SteamCmd {
                dir: "C:\\rt\\rust".into(),
                app_id: 258550,
                build_id: None,
                // Nothing recorded on disk, so this first install is
                // verified as well as fetched.
                verify: true
            }
        );
    }

    #[test]
    fn old_serialized_steam_plans_keep_verifying() {
        let old: Plan =
            serde_json::from_value(json!({"kind":"steamCmd","dir":"C:/rt/rust","appId":258550}))
                .unwrap();
        assert!(
            matches!(old, Plan::SteamCmd { verify: true, .. }),
            "omitting the new field must preserve the old verification behavior"
        );
    }

    #[test]
    fn a_suspect_pinned_steam_runtime_is_not_skipped() {
        let mut d = rust();
        d.platforms.get_mut("win32-x64").unwrap().runtime.build_id = Some("pinned".into());
        let present = Present {
            build_id: Some("pinned".into()),
            suspect: true,
        };
        assert!(
            matches!(
                plan(&d, "win32-x64", "C:/rt", &present).unwrap(),
                Plan::SteamCmd { verify: true, .. }
            ),
            "a matching stamp must not hide the host's corruption observation"
        );
    }

    /// The measured cost of getting this wrong: the Rust pilot re-verified
    /// 5,869,171,402 bytes on every start, minutes at a time, on a runtime
    /// that was already complete and already current. An update check is
    /// cheap and still runs; a full verify is not and now needs a reason.
    #[test]
    fn a_stamped_runtime_is_updated_without_being_verified_again() {
        let present = Present {
            build_id: Some("app258550".into()),
            suspect: false,
        };
        match plan(&rust(), "win32-x64", "C:\rt", &present).unwrap() {
            Plan::SteamCmd { verify, .. } => assert!(
                !verify,
                "a complete runtime does not need every file read again"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// Unpinned still means "ask Steam what is current" every time: a game
    /// that force-updates refuses every client the moment it is behind. What
    /// changed is only whether the files are re-read.
    #[test]
    fn an_unpinned_runtime_is_still_never_already_present() {
        let present = Present {
            build_id: Some("app258550".into()),
            suspect: false,
        };
        assert!(
            matches!(
                plan(&rust(), "win32-x64", "C:\rt", &present).unwrap(),
                Plan::SteamCmd { .. }
            ),
            "an unpinned runtime must still be offered to steamcmd"
        );
    }

    /// The two reasons to doubt what is on disk, and the only two.
    #[test]
    fn a_first_install_or_a_suspect_one_is_verified() {
        for present in [
            Present {
                build_id: None,
                suspect: false,
            },
            Present {
                build_id: Some("app258550".into()),
                suspect: true,
            },
        ] {
            match plan(&rust(), "win32-x64", "C:\rt", &present).unwrap() {
                Plan::SteamCmd { verify, .. } => assert!(verify, "for {present:?}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_direct_runtime_carries_its_digest_and_what_to_do_with_it() {
        let plan = plan(&direct(json!({})), "win32-x64", "/var/rt", &NOTHING).unwrap();
        match plan {
            Plan::Direct {
                dir,
                url,
                sha256,
                extract,
                ..
            } => {
                assert_eq!(dir, "/var/rt/terraria");
                assert_eq!(url, "https://terraria.org/server.zip");
                assert_eq!(sha256.len(), 64);
                assert_eq!(extract, Extract::Zip);
            }
            other => panic!("expected a direct download, got {other:?}"),
        }
    }

    #[test]
    fn a_pinned_build_already_on_disk_is_not_downloaded_again() {
        let d = direct(json!({}));
        let id =
            direct_build_id("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
        let present = Present {
            suspect: false,
            build_id: Some(id.clone()),
        };
        assert_eq!(
            plan(&d, "win32-x64", "/rt", &present).unwrap(),
            Plan::AlreadyPresent {
                dir: "/rt/terraria".into(),
                build_id: id
            }
        );
    }

    #[test]
    fn a_different_build_on_disk_is_replaced() {
        let present = Present {
            suspect: false,
            build_id: Some("000000000000".into()),
        };
        assert!(matches!(
            plan(&direct(json!({})), "win32-x64", "/rt", &present).unwrap(),
            Plan::Direct { .. }
        ));
    }

    /// The asymmetry that matters: a pinned steamcmd build can be skipped,
    /// an unpinned one never can.
    #[test]
    fn a_pinned_steam_build_can_be_skipped_and_an_unpinned_one_cannot() {
        let pinned: GameDescriptor = serde_json::from_value(json!({
            "id": "rust", "hosts": ["win32-x64"],
            "platforms": { "win32-x64": {
                "runtime": { "source": "steamcmd", "appId": 258550, "buildId": "19283746" },
                "launch": { "exe": "S.exe" } } }
        }))
        .unwrap();
        let present = Present {
            suspect: false,
            build_id: Some("19283746".into()),
        };
        assert!(matches!(
            plan(&pinned, "win32-x64", "/rt", &present).unwrap(),
            Plan::AlreadyPresent { .. }
        ));

        // The pilot pins nothing, so the same "already present" state still
        // sends it to steamcmd.
        assert!(matches!(
            plan(&rust(), "win32-x64", "/rt", &present).unwrap(),
            Plan::SteamCmd { build_id: None, .. }
        ));
    }

    #[test]
    fn a_build_id_is_the_digest_so_the_two_cannot_disagree() {
        assert_eq!(direct_build_id("deadbeefcafebabe0123"), "deadbeefcafe");
    }

    #[test]
    fn a_direct_runtime_with_no_checksum_is_refused_rather_than_trusted() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "x", "name": "X", "hosts": ["win32-x64"],
            "platforms": { "win32-x64": {
                "runtime": { "source": "direct", "url": "https://x/y" },
                "launch": { "exe": "S.exe" } } }
        }))
        .unwrap();
        let err = plan(&d, "win32-x64", "/rt", &NOTHING)
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum"), "{err}");
    }

    #[test]
    fn a_source_from_a_newer_schema_is_refused_with_advice() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "x", "name": "X", "hosts": ["win32-x64"],
            "platforms": { "win32-x64": {
                "runtime": { "source": "vendor-api", "url": "https://x/y" },
                "launch": { "exe": "S.exe" } } }
        }))
        .unwrap();
        let err = plan(&d, "win32-x64", "/rt", &NOTHING)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Updating Homerun"), "{err}");
    }

    /// A plan crosses the bridge as JSON, and every key on it has to be
    /// camelCase like its neighbours. `rename_all` on an enum renames the
    /// *variants*, not their fields -- which is how `appId` first shipped as
    /// `app_id` beside a camelCase `runtimeRoot`, compiling cleanly the whole
    /// way.
    #[test]
    fn a_plan_reaches_a_host_in_the_shape_it_reads() {
        let steam =
            serde_json::to_value(plan(&rust(), "win32-x64", "C:\\rt", &NOTHING).unwrap()).unwrap();
        assert_eq!(steam["kind"], "steamCmd");
        assert_eq!(steam["appId"], 258550, "camelCase, like every neighbour");
        assert_eq!(steam["dir"], "C:\\rt\\rust");
        assert!(steam.get("app_id").is_none(), "{steam}");

        let download =
            serde_json::to_value(plan(&direct(json!({})), "win32-x64", "/rt", &NOTHING).unwrap())
                .unwrap();
        assert_eq!(download["kind"], "direct");
        assert_eq!(download["extract"], "zip");
        assert!(download["sha256"].is_string());

        let id =
            direct_build_id("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
        let present = Present {
            build_id: Some(id),
            suspect: false,
        };
        let skipped =
            serde_json::to_value(plan(&direct(json!({})), "win32-x64", "/rt", &present).unwrap())
                .unwrap();
        assert_eq!(skipped["kind"], "alreadyPresent");
        assert!(skipped["buildId"].is_string(), "{skipped}");
    }

    // ─── a vendor runtime, in the version the host chose ────────────────────

    fn terraria(extra: serde_json::Value) -> GameDescriptor {
        let mut runtime = json!({
            "source": "vendor",
            "url": "https://terraria.org/api/download/pc-dedicated-server/terraria-server-{versionDigits}.zip",
            "extract": "zip",
            "stripComponents": 1,
            "versionSetting": "version"
        });
        for (k, v) in extra.as_object().unwrap() {
            runtime[k] = v.clone();
        }
        serde_json::from_value(json!({
            "id": "terraria", "name": "Terraria", "hosts": ["win32-x64"],
            "settings": [{ "key": "version", "type": "string", "label": "Version",
                           "default": "latest" }],
            "platforms": { "win32-x64": { "runtime": runtime,
                                          "launch": { "exe": "Windows/TerrariaServer" } } }
        }))
        .unwrap()
    }

    #[test]
    fn a_vendor_runtime_is_fetched_per_version_from_the_descriptors_site() {
        let plan = plan_version(
            &terraria(json!({})),
            "win32-x64",
            "/rt",
            &NOTHING,
            Some("1.4.5.8"),
        )
        .unwrap();
        assert_eq!(
            plan,
            Plan::Vendor {
                dir: "/rt/terraria/1.4.5.8".into(),
                url:
                    "https://terraria.org/api/download/pc-dedicated-server/terraria-server-1458.zip"
                        .into(),
                version: "1.4.5.8".into(),
                size: None,
                strip_components: 1,
                record: "/rt/terraria/.vendor-hashes.json".into(),
            }
        );
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["kind"], "vendor");
        assert_eq!(
            json["stripComponents"], 1,
            "camelCase, like every neighbour"
        );
    }

    #[test]
    fn both_version_placeholders_are_filled() {
        assert_eq!(
            vendor_url(
                "https://x.example/{version}/s-{versionDigits}.zip",
                "1.4.5.8"
            )
            .unwrap(),
            "https://x.example/1.4.5.8/s-1458.zip"
        );
    }

    #[test]
    fn a_version_already_on_disk_is_not_downloaded_again_unless_suspect() {
        let present = Present {
            build_id: Some(vendor_build_id("1.4.5.8")),
            suspect: false,
        };
        assert_eq!(
            plan_version(
                &terraria(json!({})),
                "win32-x64",
                "C:\\rt",
                &present,
                Some("1.4.5.8")
            )
            .unwrap(),
            Plan::AlreadyPresent {
                dir: "C:\\rt\\terraria\\1.4.5.8".into(),
                build_id: "v1.4.5.8".into()
            }
        );
        let suspect = Present {
            suspect: true,
            ..present
        };
        assert!(matches!(
            plan_version(
                &terraria(json!({})),
                "win32-x64",
                "C:\\rt",
                &suspect,
                Some("1.4.5.8")
            )
            .unwrap(),
            Plan::Vendor { .. }
        ));
    }

    #[test]
    fn a_vendor_runtime_with_no_version_is_refused_in_words() {
        let err = plan(&terraria(json!({})), "win32-x64", "/rt", &NOTHING)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not told which version"), "{err}");
        let err = install_dir(&terraria(json!({})), "win32-x64", "/rt", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not told which version"), "{err}");
    }

    /// The version becomes part of a URL and a directory name, so it is
    /// dotted digits and nothing a player could shape into somewhere else.
    #[test]
    fn a_version_that_is_not_dotted_digits_is_refused() {
        for bad in [
            "",
            "latest",
            "1..2",
            ".1",
            "1.",
            "1.4/../../x",
            "..",
            "1.4.5.8?x=1",
            "1.4%2e5",
            "1.2.3.4.5.6.7",
            "１.２",
            "1 .2",
            "-1",
        ] {
            assert!(check_runtime_version(bad).is_err(), "accepted {bad:?}");
            assert!(
                plan_version(
                    &terraria(json!({})),
                    "win32-x64",
                    "/rt",
                    &NOTHING,
                    Some(bad)
                )
                .is_err(),
                "planned {bad:?}"
            );
            assert!(install_dir(&terraria(json!({})), "win32-x64", "/rt", Some(bad)).is_err());
        }
        for good in ["1", "1.4.5.8", "1.2.3.4.5.6", "0.10"] {
            assert!(check_runtime_version(good).is_ok(), "refused {good:?}");
        }
    }

    #[test]
    fn a_vendor_url_pattern_names_the_version_and_nothing_else() {
        assert!(vendor_url_problem("https://t.example/s-{versionDigits}.zip").is_none());
        for (bad, why) in [
            (
                "https://t.example/server.zip",
                "does not say where the version goes",
            ),
            ("https://t.example/{setting:x}.zip", "{setting:x}"),
            ("https://{version}.t.example/s.zip", "site's name"),
            ("https://t.example/{version.zip", "never closed"),
            ("https://t.example/}{version}", "never opened"),
            ("t.example/{version}", "not a web address"),
        ] {
            let problem = vendor_url_problem(bad).unwrap_or_default();
            assert!(problem.contains(why), "{bad}: {problem:?}");
        }
    }

    #[test]
    fn a_vendor_download_that_is_not_https_is_refused() {
        let err = vendor_url("http://t.example/{version}.zip", "1.2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("https"), "{err}");
    }

    #[test]
    fn only_a_vendor_runtime_is_placed_by_version() {
        assert_eq!(
            install_dir(&terraria(json!({})), "win32-x64", "/rt", Some("1.4.5.8")).unwrap(),
            "/rt/terraria/1.4.5.8"
        );
        assert_eq!(
            install_dir(&direct(json!({})), "win32-x64", "/rt", Some("1.4.5.8")).unwrap(),
            "/rt/terraria",
            "a version means nothing to a pinned download"
        );
    }

    #[test]
    fn strip_components_reaches_a_direct_plan_too() {
        match plan(
            &direct(json!({ "stripComponents": 2 })),
            "win32-x64",
            "/rt",
            &NOTHING,
        )
        .unwrap()
        {
            Plan::Direct {
                strip_components, ..
            } => assert_eq!(strip_components, 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_runtime_directory_is_shared_by_every_server_of_that_game() {
        assert_eq!(
            runtime_dir("C:\\Homerun\\runtime\\games", "rust"),
            "C:\\Homerun\\runtime\\games\\rust"
        );
        assert_eq!(
            runtime_dir("/opt/homerun/runtime/", "rust"),
            "/opt/homerun/runtime/rust"
        );
    }
}

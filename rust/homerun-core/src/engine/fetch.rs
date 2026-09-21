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
        #[serde(default)]
        verify: bool,
    },
}

impl Plan {
    /// Where the runtime lives once this plan has run.
    pub fn dir(&self) -> &str {
        match self {
            Plan::AlreadyPresent { dir, .. }
            | Plan::Direct { dir, .. }
            | Plan::SteamCmd { dir, .. } => dir,
        }
    }
}

/// The build id a direct download is known by.
///
/// The digest prefix, so that the id in a manifest and the bytes it describes
/// cannot drift apart — the same rule the desktop artifacts follow.
pub fn direct_build_id(sha256: &str) -> String {
    sha256.chars().take(12).collect()
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

/// Decide how to get this game's server onto this machine.
pub fn plan(
    descriptor: &GameDescriptor,
    host: &str,
    runtime_root: &str,
    present: &Present,
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
            })
        }
        RuntimeSource::Steamcmd => {
            let app_id = runtime
                .app_id
                .ok_or_else(|| missing(descriptor, "a Steam application id"))?;
            if let Some(pinned) = &runtime.build_id {
                if present.build_id.as_deref() == Some(pinned.as_str()) {
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

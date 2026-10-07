//! Xbox Game Pass and the Microsoft Store: which installed package is the
//! game, and the identifiers that start it.
//!
//! # What the runner hands over
//!
//! Windows keeps the record: the user's installed packages, each with a
//! family name, an install folder and a signature kind. The runner asks
//! Windows for the packages a descriptor names, reads each one's
//! `AppxManifest.xml` and `MicrosoftGame.config` through
//! [`super::manifest`] (the desktop-only `client-manifests` feature), and
//! passes them here as [`Package`]s. These types are plain data and are not
//! behind the feature; only the XML reading is.
//!
//! # The executable behind the launch stub
//!
//! A Game Pass application's entry point is Microsoft's
//! `GameLaunchHelper.exe`, which runs from the install folder and then starts
//! the real game. `MicrosoftGame.config` names that game per application id
//! (`AppPalShipping` -> `Pal/Binaries/WinGDK/Palworld-WinGDK-Shipping.exe`).
//! Knowing it lets [`super::Launch::started`] wait for *the game*, not just
//! any new process in the folder, which the stub itself would satisfy even
//! when the game behind it then fails to start.
//!
//! # Store-signed only
//!
//! As Playnite does: a package counts only if the Store signed it. A
//! sideloaded package can take any name it likes, including a game's, and
//! launching it would run whatever it contains.
//!
//! # What a launch is
//!
//! `explorer.exe shell:AppsFolder\<family>!<application>`. Every Game Pass
//! game on the machine this was written on (fifteen of them) has the same
//! entry point, Microsoft's `GameLaunchHelper.exe`, so the same line starts
//! each. Explorer's exit code means nothing: it exits 1 having started the
//! game. Whether the game started is decided by [`super::started`].

use serde::{Deserialize, Serialize};

/// One installed package, as the runner read it from Windows.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub package_family_name: String,
    /// The folder the package is installed in, under `WindowsApps`.
    pub install_location: String,
    /// `Store`, `Developer`, `Enterprise`, `System` or `None`, as Windows
    /// spells `PackageSignatureKind`.
    pub signature_kind: String,
    pub is_framework: bool,
    /// The `Application Id`s in the package's manifest. Empty for a content
    /// package with nothing to start, and absent from what Windows itself
    /// reports: the runner fills it from the manifest afterwards.
    #[serde(default)]
    pub applications: Vec<String>,
    /// What `MicrosoftGame.config` says each application really runs. Empty
    /// when the package has no such file, or the runner could not read it;
    /// the started check then falls back to the install folder.
    #[serde(default)]
    pub executables: Vec<GameExecutable>,
}

/// One `<Executable>` in `MicrosoftGame.config`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameExecutable {
    /// The application id it belongs to, the manifest's `Application Id`.
    pub id: String,
    /// Relative to the install folder, with backslashes, checked to stay
    /// inside it.
    pub name: String,
}

impl Package {
    /// The full path of the game `application` really runs, when
    /// `MicrosoftGame.config` said.
    pub fn executable_for(&self, application: &str) -> Option<String> {
        let exe = self
            .executables
            .iter()
            .find(|e| e.id.eq_ignore_ascii_case(application))?;
        is_relative_inside(&exe.name).then(|| {
            format!(
                "{}\\{}",
                self.install_location.trim_end_matches(['\\', '/']),
                exe.name.replace('/', "\\")
            )
        })
    }
}

/// A path relative to the install folder that stays in it: no drive, no
/// leading separator, no `..`, nothing empty between separators.
pub fn is_relative_inside(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(':')
        && !path.starts_with(['\\', '/'])
        && path
            .split(['\\', '/'])
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !path.chars().any(char::is_control)
}

/// Why a declared package is not something to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// No Store-signed package of that family is installed.
    NotInstalled,
    /// The package is there and the declared application is not in it.
    NoSuchApplication,
}

/// The installed package that is this game, if there is one.
///
/// Family names compare case-insensitively, as Windows compares them;
/// application ids too, as `shell:AppsFolder` resolves them.
pub fn find<'a>(
    packages: &'a [Package],
    family: &str,
    application: &str,
) -> Result<&'a Package, Missing> {
    let package = packages
        .iter()
        .filter(|p| {
            p.signature_kind == "Store" && !p.is_framework && !p.install_location.is_empty()
        })
        .find(|p| p.package_family_name.eq_ignore_ascii_case(family))
        .ok_or(Missing::NotInstalled)?;
    if package
        .applications
        .iter()
        .any(|a| a.eq_ignore_ascii_case(application))
    {
        Ok(package)
    } else {
        Err(Missing::NoSuchApplication)
    }
}

/// `<Name>_<publisher hash>`: a name of letters, digits, `.` and `-`, then
/// Windows' thirteen-character publisher id.
pub fn is_package_family_name(value: &str) -> bool {
    let Some((name, publisher)) = value.rsplit_once('_') else {
        return false;
    };
    (3..=50).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && publisher.len() == 13
        && publisher
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// An `Application Id` as a manifest allows it: a letter, then letters,
/// digits and dots, at most 64.
pub fn is_application_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic())
        && value.len() <= 64
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

/// The one argument `explorer.exe` is given. Both halves are re-checked here
/// so that no caller can build a launch line from unchecked text.
pub fn apps_folder_target(family: &str, application: &str) -> Option<String> {
    (is_package_family_name(family) && is_application_id(application))
        .then(|| format!("shell:AppsFolder\\{family}!{application}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(family: &str, apps: &[&str]) -> Package {
        Package {
            package_family_name: family.into(),
            install_location: format!(r"C:\Program Files\WindowsApps\{family}"),
            signature_kind: "Store".into(),
            is_framework: false,
            applications: apps.iter().map(|a| a.to_string()).collect(),
            executables: Vec::new(),
        }
    }

    #[test]
    fn the_executable_behind_an_application_is_a_full_path_inside_the_package() {
        let mut p = package("Microsoft.Cardinal_8wekyb3d8bbwe", &["Game", "Editor"]);
        p.executables = vec![
            GameExecutable {
                id: "Game".into(),
                name: "RelicCardinal.exe".into(),
            },
            GameExecutable {
                id: "Editor".into(),
                name: "Tools/EssenceEditor.exe".into(),
            },
        ];
        assert_eq!(
            p.executable_for("editor").as_deref(),
            Some(
                r"C:\Program Files\WindowsApps\Microsoft.Cardinal_8wekyb3d8bbwe\Tools\EssenceEditor.exe"
            )
        );
        assert_eq!(p.executable_for("Launcher"), None);

        for escape in [
            r"..\..\Windows\notepad.exe",
            r"C:\Windows\notepad.exe",
            r"\x.exe",
            "a//b.exe",
            "",
        ] {
            p.executables = vec![GameExecutable {
                id: "Game".into(),
                name: escape.into(),
            }];
            assert_eq!(p.executable_for("Game"), None, "{escape:?}");
        }
    }

    /// Read off the dev machine with `Get-AppxPackage`, 2026-10-07.
    fn this_machine() -> Vec<Package> {
        vec![
            package("PocketpairInc.Palworld_ad4psfrxyesvt", &["AppPalShipping"]),
            package("Microsoft.Cardinal_8wekyb3d8bbwe", &["Game", "Editor"]),
            package("Microsoft.AoE2DEEnhancedGraphicsPack_8wekyb3d8bbwe", &[]),
            package(
                "FrictionalGames.AmnesiaTheBunker_yhrbwy6qaj8bt",
                &["XBO.AmnesiaTheBunker"],
            ),
        ]
    }

    #[test]
    fn palworld_is_found_by_family_and_application() {
        let packages = this_machine();
        let found = find(
            &packages,
            "PocketpairInc.Palworld_ad4psfrxyesvt",
            "AppPalShipping",
        )
        .unwrap();
        assert_eq!(
            found.package_family_name,
            "PocketpairInc.Palworld_ad4psfrxyesvt"
        );
        assert!(find(
            &packages,
            "pocketpairinc.palworld_ad4psfrxyesvt",
            "apppalshipping"
        )
        .is_ok());
    }

    #[test]
    fn a_package_with_several_applications_starts_the_declared_one() {
        let packages = this_machine();
        assert!(find(&packages, "Microsoft.Cardinal_8wekyb3d8bbwe", "Editor").is_ok());
        assert_eq!(
            find(&packages, "Microsoft.Cardinal_8wekyb3d8bbwe", "Launcher"),
            Err(Missing::NoSuchApplication)
        );
    }

    #[test]
    fn a_content_package_with_no_application_is_not_a_game() {
        assert_eq!(
            find(
                &this_machine(),
                "Microsoft.AoE2DEEnhancedGraphicsPack_8wekyb3d8bbwe",
                "App"
            ),
            Err(Missing::NoSuchApplication)
        );
    }

    #[test]
    fn only_a_store_signed_application_package_counts() {
        let family = "PocketpairInc.Palworld_ad4psfrxyesvt";
        let tampers: [fn(&mut Package); 4] = [
            |p: &mut Package| p.signature_kind = "Developer".into(),
            |p: &mut Package| p.signature_kind = "None".into(),
            |p: &mut Package| p.is_framework = true,
            |p: &mut Package| p.install_location.clear(),
        ];
        for tamper in tampers {
            let mut p = package(family, &["AppPalShipping"]);
            tamper(&mut p);
            assert_eq!(
                find(&[p], family, "AppPalShipping"),
                Err(Missing::NotInstalled)
            );
        }
    }

    #[test]
    fn every_family_name_on_the_dev_machine_is_one() {
        for family in [
            "PocketpairInc.Palworld_ad4psfrxyesvt",
            "Microsoft.SeaofThieves_8wekyb3d8bbwe",
            "Microsoft.OE-Arkansas_8wekyb3d8bbwe",
            "Microsoft.4297127D64EC6_8wekyb3d8bbwe",
            "BethesdaSoftworks.ProjectAltar_3275kfvn8vcwc",
            "PlayStack.Balatro_3wcqaesafpzfy",
        ] {
            assert!(is_package_family_name(family), "{family}");
        }
        for bad in [
            "Palworld",
            "PocketpairInc.Palworld_AD4PSFRXYESVT",
            "PocketpairInc.Palworld_ad4psfrxyesv",
            "Pocketpair Inc.Palworld_ad4psfrxyesvt",
            r"..\x_ad4psfrxyesvt",
            "PocketpairInc.Palworld_ad4psfrxyesvt!App",
        ] {
            assert!(!is_package_family_name(bad), "{bad}");
        }
    }

    #[test]
    fn every_application_id_on_the_dev_machine_is_one() {
        for app in [
            "AppPalShipping",
            "Game",
            "App",
            "Forzahorizon6",
            "XBO.AmnesiaTheBunker",
        ] {
            assert!(is_application_id(app), "{app}");
        }
        for bad in ["", "1Game", "Game!", "Game App", r"Game\x", &"a".repeat(65)] {
            assert!(!is_application_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_launch_target_is_built_only_from_checked_halves() {
        assert_eq!(
            apps_folder_target("PocketpairInc.Palworld_ad4psfrxyesvt", "AppPalShipping").as_deref(),
            Some(r"shell:AppsFolder\PocketpairInc.Palworld_ad4psfrxyesvt!AppPalShipping")
        );
        assert_eq!(
            apps_folder_target("PocketpairInc.Palworld_ad4psfrxyesvt", "App Pal"),
            None
        );
        assert_eq!(apps_folder_target("x", "Game"), None);
    }
}

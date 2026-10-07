//! Reading the two XML files a Windows game ships. Desktop only, behind the
//! `client-manifests` feature; see `Cargo.toml` for why the crate takes an
//! XML parser at all.
//!
//! - **`AppxManifest.xml`** is the package's manifest: which applications it
//!   has, by `Id`. A launch is `shell:AppsFolder\<family>!<Id>`, so this is
//!   what proves the declared application exists.
//! - **`MicrosoftGame.config`** is the Microsoft game-development-kit
//!   (GDK) config: which executable each application really runs, behind the
//!   `GameLaunchHelper.exe` stub every Game Pass application starts through.
//!   See [`super::xbox`] for why the started check wants it.
//!
//! The runner reads both files from the package's install folder and hands
//! the text here. A file it could not read, or that does not parse, costs the
//! check its precision, not the launch: no applications means the package is
//! not launchable, and no executables means the started check watches the
//! folder instead.
//!
//! # Hostile input
//!
//! A package can be installed by anyone who can sideload one, and these
//! files are its author's. roxmltree refuses a DTD by default, so an
//! entity-expansion file fails to parse. Every value read is held to a shape
//! afterwards ([`super::xbox::is_application_id`],
//! [`super::xbox::is_relative_inside`]) and dropped if it does not fit; a
//! file is also refused outright above [`MAX_BYTES`].

use super::steam::ParseError;
use super::xbox::{is_application_id, is_relative_inside, GameExecutable};

/// Bigger than any real manifest (Palworld's two are under 6 KB together)
/// by a wide margin, and small enough that parsing one costs nothing.
pub const MAX_BYTES: usize = 1 << 20;

/// The application ids in an `AppxManifest.xml`, in file order. Ids that are
/// not in a manifest's shape are dropped.
pub fn appx_applications(text: &str) -> Result<Vec<String>, ParseError> {
    let doc = parse(text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "Package" {
        return Err(ParseError("this is not a package manifest".into()));
    }
    Ok(root
        .children()
        .filter(|n| n.has_tag_name_local("Applications"))
        .flat_map(|apps| {
            apps.children()
                .filter(|n| n.has_tag_name_local("Application"))
        })
        .filter_map(|app| app.attribute("Id"))
        .filter(|id| is_application_id(id))
        .map(str::to_string)
        .collect())
}

/// The executables in a `MicrosoftGame.config`, by application id. Entries
/// whose id or path is not in shape are dropped.
pub fn game_executables(text: &str) -> Result<Vec<GameExecutable>, ParseError> {
    let doc = parse(text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "Game" {
        return Err(ParseError("this is not a MicrosoftGame.config".into()));
    }
    Ok(root
        .children()
        .filter(|n| n.has_tag_name_local("ExecutableList"))
        .flat_map(|list| {
            list.children()
                .filter(|n| n.has_tag_name_local("Executable"))
        })
        .filter_map(|exe| Some((exe.attribute("Id")?, exe.attribute("Name")?)))
        .filter(|(id, name)| is_application_id(id) && is_relative_inside(name))
        .map(|(id, name)| GameExecutable {
            id: id.to_string(),
            name: name.replace('/', "\\"),
        })
        .collect())
}

fn parse(text: &str) -> Result<roxmltree::Document<'_>, ParseError> {
    if text.len() > MAX_BYTES {
        return Err(ParseError("the file is too large to be a manifest".into()));
    }
    // A byte-order mark is common in files Windows tools write.
    roxmltree::Document::parse(text.trim_start_matches('\u{feff}'))
        .map_err(|e| ParseError(format!("not readable XML: {e}")))
}

/// Match on the local name, ignoring the namespace: the foundation namespace
/// has been the same since Windows 10, and an element a newer schema moves
/// into another one should still be found rather than silently not.
trait LocalName {
    fn has_tag_name_local(&self, name: &str) -> bool;
}

impl LocalName for roxmltree::Node<'_, '_> {
    fn has_tag_name_local(&self, name: &str) -> bool {
        self.is_element() && self.tag_name().name() == name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPX: &str = include_str!("../testdata/palworld.AppxManifest.xml");
    const GAME_CONFIG: &str = include_str!("../testdata/palworld.MicrosoftGame.config");

    #[test]
    fn palworlds_manifest_has_one_application() {
        assert_eq!(appx_applications(APPX).unwrap(), ["AppPalShipping"]);
    }

    #[test]
    fn palworlds_config_names_the_real_game_behind_the_stub() {
        assert_eq!(
            game_executables(GAME_CONFIG).unwrap(),
            [GameExecutable {
                id: "AppPalShipping".into(),
                name: r"Pal\Binaries\WinGDK\Palworld-WinGDK-Shipping.exe".into(),
            }]
        );
    }

    /// Age of Empires IV's shape: two applications, each its own executable.
    #[test]
    fn several_applications_are_all_read_in_order() {
        let appx = r#"<?xml version="1.0"?>
            <Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10">
              <Applications>
                <Application Id="Game" Executable="GameLaunchHelper.exe" />
                <Application Id="Editor" Executable="GameLaunchHelper.exe" />
              </Applications>
            </Package>"#;
        assert_eq!(appx_applications(appx).unwrap(), ["Game", "Editor"]);

        let config = r#"<Game configVersion="1"><ExecutableList>
                <Executable Name="RelicCardinal.exe" Id="Game" TargetDeviceFamily="PC" />
                <Executable Name="EssenceEditor.exe" Id="Editor" />
            </ExecutableList></Game>"#;
        let ids: Vec<_> = game_executables(config)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["Game", "Editor"]);
    }

    #[test]
    fn a_content_package_has_no_applications() {
        let appx = r#"<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10">
              <Properties><DisplayName>Enhanced Graphics Pack</DisplayName></Properties>
            </Package>"#;
        assert!(appx_applications(appx).unwrap().is_empty());
    }

    #[test]
    fn values_out_of_shape_are_dropped_not_trusted() {
        let appx = r#"<Package xmlns="x"><Applications>
                <Application Id="Good" /><Application Id="bad id!" /><Application />
            </Applications></Package>"#;
        assert_eq!(appx_applications(appx).unwrap(), ["Good"]);

        let config = r#"<Game><ExecutableList>
                <Executable Name="..\..\Windows\System32\cmd.exe" Id="Game" />
                <Executable Name="C:\Windows\notepad.exe" Id="Game" />
                <Executable Name="ok.exe" Id="1bad" />
                <Executable Name="real.exe" Id="Game" />
            </ExecutableList></Game>"#;
        assert_eq!(
            game_executables(config).unwrap(),
            [GameExecutable {
                id: "Game".into(),
                name: "real.exe".into()
            }]
        );
    }

    /// The billion-laughs file. roxmltree's default is to refuse any DTD,
    /// which is what makes taking it on safe; this pins that default.
    #[test]
    fn a_dtd_is_refused_so_entities_cannot_expand() {
        let bomb = r#"<?xml version="1.0"?>
            <!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;">]>
            <Package><Applications><Application Id="&lol2;" /></Applications></Package>"#;
        assert!(appx_applications(bomb).is_err());
    }

    #[test]
    fn the_wrong_file_or_a_broken_one_is_an_error() {
        assert!(
            appx_applications(GAME_CONFIG).is_err(),
            "a game config is not a manifest"
        );
        assert!(game_executables(APPX).is_err(), "and the other way round");
        for broken in ["", "<Package>", "not xml", "<Package></Game>"] {
            assert!(appx_applications(broken).is_err(), "{broken:?}");
        }
        assert!(appx_applications(&" ".repeat(MAX_BYTES + 1)).is_err());
    }

    #[test]
    fn a_byte_order_mark_is_fine() {
        assert_eq!(
            appx_applications(&format!("\u{feff}{APPX}")).unwrap(),
            ["AppPalShipping"]
        );
    }
}

//! Steam: which library holds a game, and whether it is installed.
//!
//! Steam keeps its own record in two kinds of text file, both in Valve's
//! KeyValues format ("VDF"), and this reads that record rather than any
//! shortcut to it. The Start menu was the tempting shortcut, and it is only
//! there if the player kept Steam's shortcut.
//!
//! - `<Steam>/steamapps/libraryfolders.vdf` lists every library folder, and
//!   in current Steam the app ids each one holds.
//! - `<library>/steamapps/appmanifest_<appid>.acf` exists for each game
//!   installed in that library, and names its folder under
//!   `steamapps/common/`.
//!
//! The runner reads the files; this parses their text. Nothing here touches
//! the disk.
//!
//! # What counts as installed
//!
//! A manifest for the app in a library. Its `StateFlags` are kept but not
//! judged: a game mid-update or mid-download still has one, and Steam itself
//! is the right thing to tell the player it is updating, which it does the
//! moment the launch reaches it.

use serde::{Deserialize, Serialize};

/// One Steam library folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    /// As Steam wrote it, unescaped: `D:\SteamLibrary`.
    pub path: String,
    /// The app ids Steam lists for it. `None` for the older file format,
    /// which listed only paths, so every library has to be asked.
    pub apps: Option<Vec<u32>>,
}

impl Library {
    /// Whether this library might hold `app_id`. True when Steam's list does
    /// not say, so the caller reads the manifest to find out.
    pub fn may_hold(&self, app_id: u32) -> bool {
        self.apps.as_ref().is_none_or(|apps| apps.contains(&app_id))
    }

    /// Where this library keeps `app_id`'s manifest.
    pub fn manifest_path(&self, app_id: u32) -> String {
        format!(
            "{}\\steamapps\\appmanifest_{app_id}.acf",
            trim_separators(&self.path)
        )
    }
}

/// What one `appmanifest_<appid>.acf` says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppManifest {
    pub app_id: u32,
    /// The folder under `steamapps/common/`. One plain folder name.
    pub install_dir: String,
    /// Kept for the log; see the module header for why it is not judged.
    pub state_flags: u32,
}

/// A game Steam has installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Installed {
    /// The library the manifest was found in.
    pub library: String,
    pub manifest: AppManifest,
}

impl Installed {
    /// `<library>\steamapps\common\<installdir>`, where the game's processes
    /// run from.
    pub fn install_dir(&self) -> String {
        format!(
            "{}\\steamapps\\common\\{}",
            trim_separators(&self.library),
            self.manifest.install_dir
        )
    }
}

/// Everything the runner found about Steam on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamFound {
    /// `steam.exe`, from the registry's `SteamPath`.
    pub exe: String,
    /// Manifests the runner read, for the app ids the descriptor declares.
    pub installed: Vec<Installed>,
}

/// Why a file could not be read as what it should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `libraryfolders.vdf`, both formats.
///
/// Current Steam writes `"0" { "path" "C:\\..." "apps" { "<appid>" "<bytes>" } }`
/// per library. Older Steam wrote `"1" "D:\\SteamLibrary"`, a path and no app
/// list, and listed its own folder nowhere. Both are still on disks.
pub fn parse_library_folders(text: &str) -> Result<Vec<Library>, ParseError> {
    let root = parse(text)?;
    let folders = root
        .get("libraryfolders")
        .and_then(Kv::as_object)
        .ok_or_else(|| ParseError("libraryfolders.vdf has no libraryfolders section".into()))?;

    let mut libraries = Vec::new();
    for (key, value) in folders {
        // Libraries are numbered. Other keys (`contentstatsid`,
        // `TimeNextStatsReport`) are bookkeeping.
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        match value {
            Kv::Str(path) if !path.is_empty() => libraries.push(Library {
                path: path.clone(),
                apps: None,
            }),
            Kv::Obj(_) => {
                let Some(path) = value
                    .get("path")
                    .and_then(Kv::as_str)
                    .filter(|p| !p.is_empty())
                else {
                    continue;
                };
                let apps = value.get("apps").and_then(Kv::as_object).map(|apps| {
                    apps.iter()
                        .filter_map(|(id, _)| id.parse::<u32>().ok())
                        .collect()
                });
                libraries.push(Library {
                    path: path.to_string(),
                    apps,
                });
            }
            _ => {}
        }
    }
    Ok(libraries)
}

/// `appmanifest_<appid>.acf`.
pub fn parse_app_manifest(text: &str) -> Result<AppManifest, ParseError> {
    let root = parse(text)?;
    let state = root
        .get("AppState")
        .ok_or_else(|| ParseError("the app manifest has no AppState section".into()))?;
    let app_id = state
        .get("appid")
        .and_then(Kv::as_str)
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|id| *id != 0)
        .ok_or_else(|| ParseError("the app manifest has no app id".into()))?;
    let install_dir = state
        .get("installdir")
        .and_then(Kv::as_str)
        .unwrap_or_default();
    // One folder name. The manifest is Steam's file, but it decides which
    // folder's processes count as the game, so it is held to that.
    if !is_plain_folder_name(install_dir) {
        return Err(ParseError(
            "the app manifest's installdir is not one folder name".into(),
        ));
    }
    let state_flags = state
        .get("StateFlags")
        .and_then(Kv::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(AppManifest {
        app_id,
        install_dir: install_dir.to_string(),
        state_flags,
    })
}

/// `steam.exe`, from the registry's `SteamPath` joined with the file name.
///
/// The registry is the user's to write, so the path is held to the shape it
/// must have: absolute, on a drive, ending in `\steam.exe`, no `..`.
/// Returned with backslashes, since `SteamPath` is written with forward ones.
pub fn steam_exe(path: &str) -> Option<String> {
    let path = path.replace('/', "\\");
    let bytes = path.as_bytes();
    let on_a_drive =
        bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    let ends_right = path.to_ascii_lowercase().ends_with("\\steam.exe");
    let no_traversal = !path.split('\\').any(|part| part == "..");
    (on_a_drive && ends_right && no_traversal && !path.chars().any(char::is_control))
        .then_some(path)
}

fn is_plain_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| matches!(c, '\\' | '/' | ':') || c.is_control())
}

fn trim_separators(path: &str) -> &str {
    path.trim_end_matches(['\\', '/'])
}

// ─── KeyValues ──────────────────────────────────────────────────────────────
//
// Quoted strings with `\\`, `\"`, `\n` and `\t` escapes, braces for nesting,
// and `//` comments. Steam writes nothing else into these two files. Keys are
// matched case-insensitively, as Steam reads them: the older format wrote
// `LibraryFolders`, the current one `libraryfolders`.

#[derive(Debug, Clone, PartialEq)]
enum Kv {
    Str(String),
    Obj(Vec<(String, Kv)>),
}

impl Kv {
    fn get(&self, key: &str) -> Option<&Kv> {
        match self {
            Kv::Obj(entries) => entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Kv::Str(_) => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Kv::Str(s) => Some(s),
            Kv::Obj(_) => None,
        }
    }

    fn as_object(&self) -> Option<&[(String, Kv)]> {
        match self {
            Kv::Obj(entries) => Some(entries),
            Kv::Str(_) => None,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Token {
    Text(String),
    Open,
    Close,
}

fn tokens(text: &str) -> Result<Vec<Token>, ParseError> {
    let mut out = Vec::new();
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '{' => out.push(Token::Open),
            '}' => out.push(Token::Close),
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => return Err(ParseError("a quoted string never ends".into())),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(other) => s.push(other),
                            None => return Err(ParseError("a quoted string never ends".into())),
                        },
                        Some(other) => s.push(other),
                    }
                }
                out.push(Token::Text(s));
            }
            _ => {
                // An unquoted token: Steam does not write these here, but the
                // format allows them, so read one rather than fail.
                let mut s = String::from(c);
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() || next == '{' || next == '}' || next == '"' {
                        break;
                    }
                    s.push(next);
                    chars.next();
                }
                out.push(Token::Text(s));
            }
        }
    }
    Ok(out)
}

fn parse(text: &str) -> Result<Kv, ParseError> {
    let tokens = tokens(text)?;
    let mut at = 0;
    let root = entries(&tokens, &mut at, 0)?;
    if at != tokens.len() {
        return Err(ParseError("unexpected `}`".into()));
    }
    Ok(Kv::Obj(root))
}

/// Deep enough for any real file, shallow enough that a hostile one cannot
/// recurse the stack away.
const MAX_DEPTH: usize = 16;

fn entries(
    tokens: &[Token],
    at: &mut usize,
    depth: usize,
) -> Result<Vec<(String, Kv)>, ParseError> {
    if depth > MAX_DEPTH {
        return Err(ParseError("nested too deeply".into()));
    }
    let mut out = Vec::new();
    while let Some(token) = tokens.get(*at) {
        let key = match token {
            Token::Close => return Ok(out),
            Token::Open => return Err(ParseError("a section with no name".into())),
            Token::Text(key) => key.clone(),
        };
        *at += 1;
        match tokens.get(*at) {
            Some(Token::Text(value)) => {
                *at += 1;
                out.push((key, Kv::Str(value.clone())));
            }
            Some(Token::Open) => {
                *at += 1;
                let inner = entries(tokens, at, depth + 1)?;
                if tokens.get(*at) != Some(&Token::Close) {
                    return Err(ParseError(format!("the \"{key}\" section never closes")));
                }
                *at += 1;
                out.push((key, Kv::Obj(inner)));
            }
            _ => return Err(ParseError(format!("\"{key}\" has no value"))),
        }
    }
    if depth == 0 {
        Ok(out)
    } else {
        Err(ParseError("a section never closes".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the dev machine's file: three libraries, escaped Windows
    /// paths, and each library's app list. App ids changed; Palworld added to
    /// the second library.
    const LIBRARY_FOLDERS: &str = r#""libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"contentid"		"4182706254125371051"
		"totalsize"		"0"
		"update_clean_bytes_tally"		"5313409"
		"time_last_update_verified"		"1759800000"
		"apps"
		{
			"228980"		"435159658"
			"945360"		"448463497"
		}
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
		"label"		""
		"contentid"		"7342816364917712340"
		"totalsize"		"2000381014016"
		"apps"
		{
			"1623730"		"25321918464"
		}
	}
	"2"
	{
		"path"		"E:\\SteamLibrary"
		"label"		"Games"
		"apps"
		{
		}
	}
}
"#;

    const OLD_LIBRARY_FOLDERS: &str = r#""LibraryFolders"
{
	"TimeNextStatsReport"		"1590000000"
	"ContentStatsID"		"-123"
	"1"		"D:\\SteamLibrary"
}
"#;

    const MANIFEST: &str = r#""AppState"
{
	"appid"		"1623730"
	"Universe"		"1"
	"name"		"Palworld"
	"StateFlags"		"4"
	"installdir"		"Palworld"
	"LastUpdated"		"1759700000"
	"InstalledDepots"
	{
		"1623731"
		{
			"manifest"		"123"
			"size"		"25321918464"
		}
	}
}
"#;

    #[test]
    fn current_libraries_come_with_their_paths_unescaped_and_their_apps() {
        let libraries = parse_library_folders(LIBRARY_FOLDERS).unwrap();
        assert_eq!(libraries.len(), 3);
        assert_eq!(libraries[0].path, r"C:\Program Files (x86)\Steam");
        assert_eq!(libraries[1].path, r"D:\SteamLibrary");
        assert_eq!(libraries[1].apps, Some(vec![1623730]));
        assert_eq!(
            libraries[2].apps,
            Some(vec![]),
            "an empty list is a known answer"
        );

        let holding: Vec<_> = libraries
            .iter()
            .filter(|l| l.may_hold(1623730))
            .map(|l| l.path.as_str())
            .collect();
        assert_eq!(holding, [r"D:\SteamLibrary"]);
    }

    #[test]
    fn the_old_format_lists_paths_only_so_every_library_may_hold_anything() {
        let libraries = parse_library_folders(OLD_LIBRARY_FOLDERS).unwrap();
        assert_eq!(
            libraries,
            [Library {
                path: r"D:\SteamLibrary".into(),
                apps: None
            }]
        );
        assert!(libraries[0].may_hold(1623730));
    }

    #[test]
    fn a_manifest_names_the_app_and_its_folder() {
        let m = parse_app_manifest(MANIFEST).unwrap();
        assert_eq!(
            m,
            AppManifest {
                app_id: 1623730,
                install_dir: "Palworld".into(),
                state_flags: 4
            }
        );

        let installed = Installed {
            library: r"D:\SteamLibrary\".into(),
            manifest: m,
        };
        assert_eq!(
            installed.install_dir(),
            r"D:\SteamLibrary\steamapps\common\Palworld"
        );
        let library = Library {
            path: r"D:\SteamLibrary".into(),
            apps: None,
        };
        assert_eq!(
            library.manifest_path(1623730),
            r"D:\SteamLibrary\steamapps\appmanifest_1623730.acf"
        );
    }

    #[test]
    fn a_manifest_whose_folder_is_not_one_folder_is_refused() {
        for dir in [r"..\..\Windows", "a/b", "", "..", r"C:\x"] {
            let text = MANIFEST.replace(
                r#""installdir"		"Palworld""#,
                &format!(r#""installdir"		"{}""#, dir.replace('\\', "\\\\")),
            );
            assert!(parse_app_manifest(&text).is_err(), "{dir:?}");
        }
    }

    #[test]
    fn comments_a_bom_and_key_case_are_tolerated() {
        let text = format!(
            "\u{feff}// written by Steam\n{}",
            LIBRARY_FOLDERS.replace("\"path\"", "\"Path\"")
        );
        assert_eq!(parse_library_folders(&text).unwrap().len(), 3);
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_panic() {
        for text in [
            "",
            "\"libraryfolders\"",
            "\"libraryfolders\" {",
            "\"libraryfolders\" { \"0\" { \"path\" \"C:\\\\x\" }",
            "\"libraryfolders\" { \"0\" { \"path\" \"unterminated }",
            "}",
            "{ }",
            &"\"a\" {".repeat(100),
        ] {
            assert!(parse_library_folders(text).is_err(), "{text:?}");
        }
        assert!(parse_app_manifest("\"AppState\" { \"appid\" \"0\" }").is_err());
        assert!(parse_app_manifest("\"AppState\" { \"appid\" \"x\" }").is_err());
    }

    #[test]
    fn steam_exe_must_look_like_steam_exe() {
        assert_eq!(
            steam_exe("c:/program files (x86)/steam/steam.exe").as_deref(),
            Some(r"c:\program files (x86)\steam\steam.exe")
        );
        for bad in [
            r"steam.exe",
            r"\\server\share\steam.exe",
            r"C:\Steam\notsteam.exe",
            r"C:\Steam\..\Windows\steam.exe",
            r"C:\Steam\steam.exe.bat",
            "C:\\Steam\n\\steam.exe",
        ] {
            assert_eq!(steam_exe(bad), None, "{bad:?}");
        }
    }
}

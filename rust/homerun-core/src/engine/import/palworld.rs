//! Palworld: a world folder from another host's dedicated server.
//!
//! A dedicated server keeps each world in `Pal/Saved/SaveGames/0/<folder>/`
//! (`Level.sav`, `LevelMeta.sav`, `Players/*.sav`) and loads the one named
//! by `DedicatedServerName` in `GameUserSettings.ini`. Importing is copying
//! that folder into the server's `Saved/SaveGames/0/`, keeping its name, and
//! pointing `DedicatedServerName` at it. The save files themselves are never
//! read or rewritten.
//!
//! Players keep their characters because a dedicated server names each
//! player's file after their own account, not after the server. Tested
//! 2026-10-12 by moving a world between two servers, there and back, with a
//! Steam player and a Game Pass player: both kept their characters, items and
//! guild, and the second server reused their files.
//!
//! # What is left out
//!
//! - **`WorldOption.sav`**, which, when present, overrides
//!   `PalWorldSettings.ini`: the server would ignore the settings Homerun
//!   writes. Leaving it out only makes players choose a respawn point again.
//! - **The world's `backup/` folder**: the old server's own snapshots.
//!
//! # What is refused
//!
//! **A co-op or single-player world**, which stores its host under the
//! placeholder `00000000000000000000000000000001.sav`. On a dedicated server
//! that player gets their real id and starts again with nothing; moving them
//! over means rewriting `Level.sav`, which this does not do.
//!
//! # Settings
//!
//! A download that includes the old server's `PalWorldSettings.ini` (from
//! `Config/WindowsServer` or `Config/LinuxServer`) brings it along. The
//! runner writes the game's managed keys into it afterwards, so the old
//! server's other choices (rates, death penalty) stay.

use super::{
    file_name, human_size, join, parent, Candidate, Context, CopyFile, Fact, Importer, IniEdit,
    Plan, Read, Refusal, RefusalCode, Source, Step, World,
};

pub const IMPORTER: Importer = Importer {
    game: "palworld",
    step,
};

/// Where the server's worlds live, relative to its folder (the descriptor
/// mounts the runtime's `Pal/Saved` onto the server's `Saved`).
const WORLDS: &str = "Saved/SaveGames/0";
const SETTINGS_FILE: &str = "Saved/Config/WindowsServer/GameUserSettings.ini";
const SETTINGS_SECTION: &str = "/Script/Pal.PalGameLocalSettings";
const WORLD_KEY: &str = "DedicatedServerName";
const WORLD_SETTINGS: &str = "Saved/Config/WindowsServer/PalWorldSettings.ini";
const CO_OP_HOST: &str = "00000000000000000000000000000001.sav";
const MAX_CANDIDATES: usize = 32;

/// A world folder found in the source: its path, `""` when the source is
/// the world folder itself.
fn worlds(source: &Source) -> Vec<String> {
    let mut found: Vec<String> = source
        .entries
        .iter()
        .filter(|e| file_name(&e.path).eq_ignore_ascii_case("Level.sav"))
        .map(|e| parent(&e.path).to_string())
        .filter(|dir| {
            let meta = join(dir, "LevelMeta.sav");
            source
                .entries
                .iter()
                .any(|e| e.path.eq_ignore_ascii_case(&meta))
        })
        // The old server's snapshots are copies of the world, not worlds.
        .filter(|dir| !dir.split('/').any(|p| p.eq_ignore_ascii_case("backup")))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The world folder's name as the server will know it.
fn world_name(source: &Source, dir: &str) -> String {
    let named = if dir.is_empty() {
        source.root_name.as_deref()
    } else {
        Some(file_name(dir))
    };
    match named.filter(|n| usable_name(n)) {
        Some(name) => name.to_string(),
        None => derived_name(source, dir),
    }
}

/// A folder name the server can be pointed at: what Palworld itself writes
/// is 32 hex digits, and anything plain enough to sit on an INI line works.
fn usable_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 32 hex digits from the world's file list, for a world whose folder had no
/// usable name (picked as the folder itself, inside a zip named oddly).
/// Stable, so importing the same download twice names it the same.
fn derived_name(source: &Source, dir: &str) -> String {
    let fnv = |seed: u64| {
        let mut hash = seed;
        for entry in source.entries.iter().filter(|e| inside(dir, &e.path)) {
            for byte in entry.path.bytes().chain(entry.size.to_le_bytes()) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        }
        hash
    };
    format!(
        "{:016X}{:016X}",
        fnv(0xcbf2_9ce4_8422_2325),
        fnv(0x6c62_272e_07bb_0142)
    )
}

fn inside(dir: &str, path: &str) -> bool {
    dir.is_empty() || path.starts_with(&format!("{dir}/"))
}

/// Files under `dir`, relative to it.
fn files_in<'a>(source: &'a Source, dir: &'a str) -> impl Iterator<Item = (&'a str, u64)> + 'a {
    source.entries.iter().filter_map(move |e| {
        let rel = if dir.is_empty() {
            e.path.as_str()
        } else {
            e.path.strip_prefix(dir)?.strip_prefix('/')?
        };
        Some((rel, e.size))
    })
}

/// Whether a file of the world folder is left behind.
fn left_out(rel: &str) -> bool {
    rel.eq_ignore_ascii_case("WorldOption.sav")
        || rel
            .split('/')
            .next()
            .is_some_and(|p| p.eq_ignore_ascii_case("backup"))
}

fn players(source: &Source, dir: &str) -> Vec<String> {
    files_in(source, dir)
        .filter_map(|(rel, _)| {
            let (folder, name) = rel.split_once('/')?;
            (folder.eq_ignore_ascii_case("Players")
                && !name.contains('/')
                && name.to_ascii_lowercase().ends_with(".sav"))
            .then(|| name.to_string())
        })
        .collect()
}

fn world_size(source: &Source, dir: &str) -> u64 {
    files_in(source, dir)
        .filter(|(rel, _)| !left_out(rel))
        .map(|(_, size)| size)
        .sum()
}

/// The source's server config files of one kind, Windows first.
fn config_files(source: &Source, name: &str) -> Vec<String> {
    let mut found: Vec<(bool, String)> = source
        .entries
        .iter()
        .filter(|e| file_name(&e.path).eq_ignore_ascii_case(name))
        .filter_map(|e| {
            let folder = parent(&e.path).to_ascii_lowercase();
            if folder == "config/windowsserver" || folder.ends_with("/config/windowsserver") {
                Some((false, e.path.clone()))
            } else if folder == "config/linuxserver" || folder.ends_with("/config/linuxserver") {
                Some((true, e.path.clone()))
            } else {
                None
            }
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

/// The world folder an old server's `GameUserSettings.ini` names.
fn named_world(bytes: &[u8]) -> Option<String> {
    let (text, _) = crate::ini_config::decode(bytes)?;
    text.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case(WORLD_KEY)
            .then(|| value.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

fn step(source: &Source, ctx: &Context) -> Step {
    let mut found = worlds(source);
    if let Some(chosen) = ctx.world {
        found.retain(|w| w == chosen);
    }
    if found.is_empty() {
        return Step::Refuse(Refusal::new(
            RefusalCode::NoWorld,
            "There is no Palworld world in what you picked. A world is a folder holding Level.sav and LevelMeta.sav, usually in Pal/Saved/SaveGames/0.",
        ));
    }
    found.truncate(MAX_CANDIDATES);

    if found.len() > 1 {
        // The old server's settings say which world it was running.
        let settings = config_files(source, "GameUserSettings.ini");
        let unread: Vec<Read> = settings
            .iter()
            .filter(|p| source.bytes(p).is_none())
            .map(|p| Read {
                path: p.clone(),
                max_bytes: 64 * 1024,
            })
            .collect();
        if !unread.is_empty() {
            return Step::Read(unread);
        }
        let named: Vec<String> = settings
            .iter()
            .filter_map(|p| source.bytes(p).and_then(named_world))
            .collect();
        let running: Vec<&String> = found
            .iter()
            .filter(|w| named.iter().any(|n| n == file_name(w)))
            .collect();
        if let [only] = running.as_slice() {
            let only = (*only).clone();
            return plan_one(source, &only);
        }
        let mut candidates: Vec<Candidate> = found
            .iter()
            .map(|w| {
                let count = players(source, w).len();
                Candidate {
                    id: w.clone(),
                    label: world_name(source, w),
                    detail: Some(match count {
                        1 => "1 player".to_string(),
                        n => format!("{n} players"),
                    }),
                    size_bytes: world_size(source, w),
                }
            })
            .collect();
        candidates.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes).then(a.label.cmp(&b.label)));
        return Step::Choose(candidates);
    }
    let only = found.remove(0);
    plan_one(source, &only)
}

fn plan_one(source: &Source, dir: &str) -> Step {
    let players = players(source, dir);
    if players.iter().any(|p| p.eq_ignore_ascii_case(CO_OP_HOST)) {
        return Step::Refuse(Refusal::new(
            RefusalCode::UnsupportedWorld,
            "This is a co-op or single-player world. Homerun can import a world from a dedicated server; in a co-op world the host's character would not carry over.",
        ));
    }
    let name = world_name(source, dir);
    let destination = format!("{WORLDS}/{name}");
    let mut copy: Vec<CopyFile> = files_in(source, dir)
        .filter(|(rel, _)| !left_out(rel))
        .map(|(rel, _)| CopyFile {
            from: join(dir, rel),
            to: format!("{destination}/{rel}"),
        })
        .collect();
    let mut warnings = Vec::new();
    if files_in(source, dir).any(|(rel, _)| rel.eq_ignore_ascii_case("WorldOption.sav")) {
        warnings.push(
            "The world's own settings file (WorldOption.sav) was left out so this server's settings apply. Players choose a respawn point again.".into(),
        );
    }
    let configs = config_files(source, "PalWorldSettings.ini");
    if let Some(first) = configs.first() {
        copy.push(CopyFile {
            from: first.clone(),
            to: WORLD_SETTINGS.into(),
        });
        if configs.len() > 1 {
            warnings.push(format!(
                "There was more than one PalWorldSettings.ini; the one in {} was used.",
                parent(first)
            ));
        }
    }
    if players.is_empty() {
        warnings.push("Nobody has played in this world yet.".into());
    }
    let size = world_size(source, dir);
    Step::Plan(Plan {
        world: Some(World {
            // The folder's name is a 32-digit id nobody chose, and the name
            // a player gave the world is inside `LevelMeta.sav`, which is
            // compressed with a library this does not carry. So none: the
            // players and the size say which world it is.
            name: String::new(),
            facts: vec![
                Fact {
                    label: "Players".into(),
                    value: players.len().to_string(),
                },
                Fact {
                    label: "Size".into(),
                    value: human_size(size),
                },
            ],
        }),
        settings: Default::default(),
        warnings,
        copy,
        present: vec![format!("{WORLDS}/*/Level.sav")],
        ini: vec![IniEdit {
            file: SETTINGS_FILE.into(),
            section: SETTINGS_SECTION.into(),
            key: WORLD_KEY.into(),
            value: name,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::super::{plan, Entry};
    use super::*;

    const WORLD: &str = "80F55DF34B4428386691578B34734816";

    fn source(paths: &[&str]) -> Source {
        Source {
            entries: paths
                .iter()
                .map(|p| Entry {
                    path: p.to_string(),
                    size: 100,
                })
                .collect(),
            ..Source::default()
        }
    }

    fn world_files(prefix: &str) -> Vec<String> {
        [
            "Level.sav",
            "LevelMeta.sav",
            "Players/F6ECAEC7000000000000000000000000.sav",
            "Players/6D454EEE000000000000000000000000.sav",
            "WorldOption.sav",
            "backup/world/2026.10.12-13.23.32/Level.sav",
            "backup/world/2026.10.12-13.23.32/LevelMeta.sav",
        ]
        .iter()
        .map(|f| join(prefix, f))
        .collect()
    }

    fn ctx() -> Context<'static> {
        Context::default()
    }

    fn the_plan(src: &Source) -> Plan {
        match plan(&IMPORTER, src, &ctx()) {
            Step::Plan(p) => p,
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    /// The layout the 2026-10-12 move used, from a whole `Pal/Saved` download.
    #[test]
    fn a_dedicated_world_lands_under_its_own_name_and_the_server_is_pointed_at_it() {
        let mut paths = world_files(&format!("Pal/Saved/SaveGames/0/{WORLD}"));
        paths.push("Pal/Saved/Config/LinuxServer/PalWorldSettings.ini".into());
        let src = source(&paths.iter().map(String::as_str).collect::<Vec<_>>());
        let p = the_plan(&src);
        let to: Vec<&str> = p.copy.iter().map(|c| c.to.as_str()).collect();
        assert!(to.contains(&format!("Saved/SaveGames/0/{WORLD}/Level.sav").as_str()));
        assert!(to.contains(
            &format!("Saved/SaveGames/0/{WORLD}/Players/6D454EEE000000000000000000000000.sav")
                .as_str()
        ));
        assert!(to.contains(&"Saved/Config/WindowsServer/PalWorldSettings.ini"));
        assert_eq!(p.ini.len(), 1);
        assert_eq!(
            p.ini[0].managed_key(),
            "[/Script/Pal.PalGameLocalSettings]DedicatedServerName"
        );
        assert_eq!(p.ini[0].value, WORLD);
        assert_eq!(p.present, vec!["Saved/SaveGames/0/*/Level.sav".to_string()]);
        let world = p.world.unwrap();
        assert_eq!(world.name, "", "a folder id is not a name to show");
        assert_eq!(world.facts[0].value, "2");
    }

    #[test]
    fn worldoption_and_the_old_servers_backups_stay_behind() {
        let src = source(
            &world_files(WORLD)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        let p = the_plan(&src);
        assert!(p
            .copy
            .iter()
            .all(|c| !c.to.contains("WorldOption") && !c.to.contains("/backup/")));
        assert!(p.warnings.iter().any(|w| w.contains("WorldOption.sav")));
    }

    #[test]
    fn a_world_folder_picked_by_itself_takes_the_folders_name() {
        let mut src = source(&[
            "Level.sav",
            "LevelMeta.sav",
            "Players/F6ECAEC7000000000000000000000000.sav",
        ]);
        src.root_name = Some(WORLD.into());
        assert_eq!(the_plan(&src).ini[0].value, WORLD);
        // A name the server could not be pointed at is replaced, the same way
        // every time.
        src.root_name = Some("my world (copy)".into());
        let a = the_plan(&src).ini[0].value.clone();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(the_plan(&src).ini[0].value, a);
    }

    #[test]
    fn a_co_op_world_is_refused_and_says_why() {
        let src = source(&[
            "W/Level.sav",
            "W/LevelMeta.sav",
            "W/Players/00000000000000000000000000000001.sav",
        ]);
        let Step::Refuse(r) = plan(&IMPORTER, &src, &ctx()) else {
            panic!()
        };
        assert_eq!(r.code, RefusalCode::UnsupportedWorld);
        assert!(r.message.contains("co-op"));
    }

    #[test]
    fn something_that_is_not_a_world_is_refused() {
        for paths in [
            &["Level.sav"][..],
            &["notes.txt"],
            &[
                "W/backup/world/x/Level.sav",
                "W/backup/world/x/LevelMeta.sav",
            ],
        ] {
            let Step::Refuse(r) = plan(&IMPORTER, &source(paths), &ctx()) else {
                panic!("{paths:?}")
            };
            assert_eq!(r.code, RefusalCode::NoWorld);
        }
    }

    #[test]
    fn of_several_worlds_the_one_the_old_server_ran_is_chosen() {
        let mut paths = world_files("SaveGames/0/AAAA");
        paths.extend(world_files(&format!("SaveGames/0/{WORLD}")));
        paths.push("Config/WindowsServer/GameUserSettings.ini".into());
        let mut src = source(&paths.iter().map(String::as_str).collect::<Vec<_>>());
        // First it asks to read the settings file.
        assert!(matches!(plan(&IMPORTER, &src, &ctx()), Step::Read(_)));
        src.read.insert(
            "Config/WindowsServer/GameUserSettings.ini".into(),
            format!("[/Script/Pal.PalGameLocalSettings]\r\nDedicatedServerName={WORLD}\r\n")
                .into_bytes(),
        );
        assert_eq!(the_plan(&src).ini[0].value, WORLD);
    }

    #[test]
    fn of_several_worlds_with_nothing_naming_one_the_player_chooses() {
        let mut paths = world_files("SaveGames/0/AAAA");
        paths.extend(world_files(&format!("SaveGames/0/{WORLD}")));
        let src = source(&paths.iter().map(String::as_str).collect::<Vec<_>>());
        let Step::Choose(c) = plan(&IMPORTER, &src, &ctx()) else {
            panic!()
        };
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].detail.as_deref(), Some("2 players"));
        let chosen = Context {
            runtime_version: None,
            world: Some("SaveGames/0/AAAA"),
        };
        let Step::Plan(p) = plan(&IMPORTER, &src, &chosen) else {
            panic!()
        };
        assert_eq!(p.ini[0].value, "AAAA");
    }

    #[test]
    fn a_settings_file_in_utf16_still_names_its_world() {
        let text = format!("[/Script/Pal.PalGameLocalSettings]\r\nDedicatedServerName={WORLD}\r\n");
        let bytes = crate::ini_config::encode(&text, crate::ini_config::Encoding::Utf16Le);
        assert_eq!(named_world(&bytes).as_deref(), Some(WORLD));
    }
}

//! Hytale: a universe from another server, or a single-player save.
//!
//! A Hytale server keeps its game in `universe/` beside its own `config.json`:
//! `universe/worlds/<world>/` (a world `config.json`, `chunks/`, `resources/`),
//! `universe/players/<account uuid>.json`, and universe-wide resources. The
//! server loads the world its `config.json` names in `Defaults.World`, which
//! must match a world folder exactly. Single-player runs the same server
//! inside the game, so a save (`%APPDATA%\Hytale\UserData\Saves\<save>\`) has
//! the same shape, and both import the same way: the universe is copied as it
//! is, and `Defaults.World` is pointed at its world.
//!
//! Players keep their characters because every player file is named after
//! the player's account, in a save and on a server alike (observed on a
//! Homerun server 2026-10-12: `universe/players/<uuid>.json`).
//!
//! # What is brought along, and what never is
//!
//! - The universe, without the `.bak` copies the server keeps of its own JSON.
//! - Beside it, when present: `mods/` (plugin state, and packs a world may
//!   need to load), `permissions.json`, `whitelist.json` and `bans.json`.
//! - **Never** the server's sign-in (`auth.enc`, `auth.key`), any other key
//!   or account file, or the client's own `client_metadata`: wherever they
//!   sit in what was picked, they are not copied and not read.
//! - Not the old `config.json` either: the new server writes its own, and only
//!   `Defaults.World` is set in it.
//!
//! # Versions
//!
//! A world records no game build. Its `config.json` has a `Version`, which is
//! the layout of that file (4 on 2026-10-12, matching the server's
//! `WorldConfig.VERSION`); a higher one is a world from a newer Hytale and is
//! warned about. Worlds upgrade one way on load, so an older server may not
//! open a world a newer one has saved.

use serde_json::Value;

use super::{
    file_name, human_size, join, parent, Candidate, Context, CopyFile, Fact, Importer, JsonEdit,
    Plan, Read, Refusal, RefusalCode, Source, Step, World,
};

pub const IMPORTER: Importer = Importer {
    game: "hytale",
    step,
};

/// The newest world config layout this has seen a server write.
pub const KNOWN_WORLD_CONFIG_VERSION: i64 = 4;

const MAX_CANDIDATES: usize = 32;
const CONFIG_BYTES: u64 = 256 * 1024;

/// Files beside the universe that come with it.
const BESIDE: &[&str] = &["permissions.json", "whitelist.json", "bans.json"];

/// Whether a file must never be copied: the server's sign-in, keys and
/// account files, and the client's own metadata. Matched on every part of the
/// path, so it holds wherever a download put them.
pub fn never_copied(path: &str) -> bool {
    path.split('/').any(|part| {
        let p = part.to_ascii_lowercase();
        p == "auth.enc"
            || p == "auth.key"
            || p == "account.dat"
            || p.ends_with(".keys")
            || p.ends_with(".key")
            || p.ends_with(".enc")
            || p.starts_with("keyutil_")
            || p.starts_with("client_metadata")
    })
}

fn is_backup(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".bak")
}

/// Every universe in the source: a folder holding `worlds/<world>/config.json`.
fn universes(source: &Source) -> Vec<String> {
    let mut found: Vec<String> = source
        .entries
        .iter()
        .filter(|e| file_name(&e.path).eq_ignore_ascii_case("config.json"))
        .filter_map(|e| {
            let world = parent(&e.path);
            let worlds = parent(world);
            (file_name(worlds).eq_ignore_ascii_case("worlds") && !world.is_empty())
                .then(|| parent(worlds).to_string())
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The world folder names in a universe.
fn worlds_in(source: &Source, universe: &str) -> Vec<String> {
    let prefix = join(universe, "worlds/");
    let mut names: Vec<String> = source
        .entries
        .iter()
        .filter_map(|e| {
            let rest = e.path.strip_prefix(&prefix)?;
            let (world, file) = rest.split_once('/')?;
            file.eq_ignore_ascii_case("config.json")
                .then(|| world.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

fn players_in(source: &Source, universe: &str) -> usize {
    let prefix = join(universe, "players/");
    source
        .entries
        .iter()
        .filter(|e| {
            e.path.strip_prefix(&prefix).is_some_and(|rest| {
                !rest.contains('/') && rest.to_ascii_lowercase().ends_with(".json")
            })
        })
        .count()
}

/// Files under `dir`, relative to it.
fn files_under<'a>(
    source: &'a Source,
    dir: &'a str,
) -> impl Iterator<Item = (&'a str, &'a str, u64)> + 'a {
    source.entries.iter().filter_map(move |e| {
        let rel = if dir.is_empty() {
            e.path.as_str()
        } else {
            e.path.strip_prefix(dir)?.strip_prefix('/')?
        };
        Some((e.path.as_str(), rel, e.size))
    })
}

/// A name a player would recognise for the universe: the save's or server's
/// folder, when it is not just an id.
fn display_name(source: &Source, universe: &str) -> String {
    let holder = if file_name(universe).eq_ignore_ascii_case("universe") {
        parent(universe)
    } else {
        universe
    };
    let name = if holder.is_empty() {
        source.root_name.clone().unwrap_or_default()
    } else {
        file_name(holder).to_string()
    };
    let looks_like_id = name.len() >= 16 && name.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if looks_like_id || name.chars().any(char::is_control) {
        String::new()
    } else {
        name
    }
}

fn json_at(source: &Source, path: &str) -> Option<Value> {
    serde_json::from_slice(source.bytes(path)?).ok()
}

fn step(source: &Source, ctx: &Context) -> Step {
    let mut found = universes(source);
    if let Some(chosen) = ctx.world {
        found.retain(|u| u == chosen);
    }
    if found.is_empty() {
        return Step::Refuse(Refusal::new(
            RefusalCode::NoWorld,
            "There is no Hytale world in what you picked. A world is a universe folder holding worlds/<name>/config.json, from a server or from Hytale's Saves folder.",
        ));
    }
    found.truncate(MAX_CANDIDATES);
    if found.len() > 1 {
        let mut candidates: Vec<Candidate> = found
            .iter()
            .map(|u| {
                let worlds = worlds_in(source, u);
                let size = files_under(source, u).map(|(_, _, s)| s).sum();
                let name = display_name(source, u);
                Candidate {
                    id: u.clone(),
                    label: if name.is_empty() {
                        worlds.join(", ")
                    } else {
                        name
                    },
                    detail: Some(match players_in(source, u) {
                        1 => "1 player".to_string(),
                        n => format!("{n} players"),
                    }),
                    size_bytes: size,
                }
            })
            .collect();
        candidates.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes).then(a.id.cmp(&b.id)));
        return Step::Choose(candidates);
    }
    let universe = found.remove(0);
    plan_one(source, &universe)
}

fn plan_one(source: &Source, universe: &str) -> Step {
    let holder = if file_name(universe).eq_ignore_ascii_case("universe") {
        parent(universe).to_string()
    } else {
        universe.to_string()
    };
    let worlds = worlds_in(source, universe);
    let server_config = join(&holder, "config.json");
    let world_configs: Vec<String> = worlds
        .iter()
        .map(|w| join(universe, &format!("worlds/{w}/config.json")))
        .collect();

    // The world configs (for the warnings) and the old server's own config
    // (for the world it ran), all small.
    let unread: Vec<Read> = world_configs
        .iter()
        .chain(std::iter::once(&server_config))
        .filter(|p| source.has(p) && source.bytes(p).is_none())
        .map(|p| Read {
            path: p.clone(),
            max_bytes: CONFIG_BYTES,
        })
        .collect();
    if !unread.is_empty() {
        return Step::Read(unread);
    }

    let named = json_at(source, &server_config)
        .and_then(|c| {
            c.pointer("/Defaults/World")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|w| worlds.contains(w));
    let spawn = named
        .or_else(|| worlds.iter().find(|w| w.as_str() == "default").cloned())
        .or_else(|| worlds.first().cloned());
    let Some(spawn) = spawn else {
        return Step::Refuse(Refusal::new(
            RefusalCode::NoWorld,
            "This universe has no world in it.",
        ));
    };

    let mut warnings = Vec::new();
    for (world, path) in worlds.iter().zip(&world_configs) {
        let Some(config) = json_at(source, path) else {
            continue;
        };
        if config.get("Version").and_then(Value::as_i64).unwrap_or(0) > KNOWN_WORLD_CONFIG_VERSION {
            warnings.push(format!(
                "The world \"{world}\" was saved by a newer version of Hytale than Homerun has tested. If the server won't start, update Hytale on this computer."
            ));
        }
        if config.get("DeleteOnUniverseStart").and_then(Value::as_bool) == Some(true) {
            warnings.push(format!(
                "The world \"{world}\" is set to be deleted every time the server starts."
            ));
        }
        if config
            .get("RequiredPlugins")
            .and_then(Value::as_object)
            .is_some_and(|p| !p.is_empty())
        {
            warnings.push(format!(
                "The world \"{world}\" needs mods. Players may see unknown blocks until the same mods are on this server."
            ));
        }
    }

    let mut copy: Vec<CopyFile> = files_under(source, universe)
        .filter(|(path, _, _)| !never_copied(path) && !is_backup(path))
        .map(|(path, rel, _)| CopyFile {
            from: path.to_string(),
            to: format!("universe/{rel}"),
        })
        .collect();
    let mods = join(&holder, "mods");
    copy.extend(
        files_under(source, &mods)
            .filter(|(path, _, _)| !never_copied(path) && !is_backup(path))
            .map(|(path, rel, _)| CopyFile {
                from: path.to_string(),
                to: format!("mods/{rel}"),
            }),
    );
    for name in BESIDE {
        let path = join(&holder, name);
        if source.has(&path) {
            copy.push(CopyFile {
                from: path,
                to: name.to_string(),
            });
        }
    }

    let size: u64 = files_under(source, universe)
        .filter(|(path, _, _)| !never_copied(path) && !is_backup(path))
        .map(|(_, _, s)| s)
        .sum();
    let players = players_in(source, universe);
    let mut facts = vec![Fact {
        label: if worlds.len() == 1 { "World" } else { "Worlds" }.into(),
        value: worlds.join(", "),
    }];
    facts.push(Fact {
        label: "Players".into(),
        value: players.to_string(),
    });
    facts.push(Fact {
        label: "Size".into(),
        value: human_size(size),
    });
    if players == 0 {
        warnings.push("Nobody has played in this world yet.".into());
    }

    Step::Plan(Plan {
        world: Some(World {
            name: display_name(source, universe),
            facts,
        }),
        settings: Default::default(),
        warnings,
        copy,
        present: vec!["universe/worlds/*/config.json".into()],
        ini: Vec::new(),
        json: vec![JsonEdit {
            file: "config.json".into(),
            key: "Defaults.World".into(),
            value: spawn,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::super::{plan, Entry};
    use super::*;

    const PLAYER: &str = "eded0cd8-61ac-4726-8b99-04eafb0ec545";

    /// The layout a Homerun Hytale server wrote on 2026-10-12, under `root`.
    fn server(root: &str) -> Vec<String> {
        [
            "config.json",
            "config.json.bak",
            "bans.json",
            "permissions.json",
            "auth.enc",
            "auth.key",
            "keyutil_localhost_1.key",
            "logs/latest.log",
            "mods/BarterShop/state.json",
            "universe/events.json",
            "universe/events.json.bak",
            &format!("universe/players/{PLAYER}.json"),
            &format!("universe/players/{PLAYER}.json.bak"),
            "universe/resources/Memories.json",
            "universe/worlds/default/config.json",
            "universe/worlds/default/config.json.bak",
            "universe/worlds/default/chunks/0.0.region.bin",
            "universe/worlds/default/resources/Time.json",
        ]
        .iter()
        .map(|f| join(root, f))
        .collect()
    }

    fn source(paths: &[String]) -> Source {
        Source {
            entries: paths
                .iter()
                .map(|p| Entry {
                    path: p.clone(),
                    size: 100,
                })
                .collect(),
            ..Source::default()
        }
    }

    /// Answer every Read with these files' contents, or `{}`.
    fn run(mut src: Source, files: &[(&str, &str)], ctx: &Context) -> Step {
        for _ in 0..4 {
            match plan(&IMPORTER, &src, ctx) {
                Step::Read(reads) => {
                    for r in reads {
                        let body = files
                            .iter()
                            .find(|(p, _)| *p == r.path)
                            .map(|(_, b)| *b)
                            .unwrap_or("{}");
                        src.read.insert(r.path, body.as_bytes().to_vec());
                    }
                }
                other => return other,
            }
        }
        panic!("never settled");
    }

    fn the_plan(step: Step) -> Plan {
        match step {
            Step::Plan(p) => p,
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn a_server_download_brings_its_universe_and_points_the_server_at_its_world() {
        let src = source(&server("Hytale-backup"));
        let p = the_plan(run(
            src,
            &[(
                "Hytale-backup/config.json",
                r#"{"Defaults":{"World":"default"}}"#,
            )],
            &Context::default(),
        ));
        let to: Vec<&str> = p.copy.iter().map(|c| c.to.as_str()).collect();
        assert!(to.contains(&"universe/worlds/default/chunks/0.0.region.bin"));
        assert!(to.contains(&format!("universe/players/{PLAYER}.json").as_str()));
        assert!(to.contains(&"mods/BarterShop/state.json"));
        assert!(to.contains(&"bans.json") && to.contains(&"permissions.json"));
        assert_eq!(p.json.len(), 1);
        assert_eq!(
            (
                p.json[0].file.as_str(),
                p.json[0].key.as_str(),
                p.json[0].value.as_str()
            ),
            ("config.json", "Defaults.World", "default")
        );
        assert_eq!(p.present, vec!["universe/worlds/*/config.json".to_string()]);
    }

    /// The rule that matters most here.
    #[test]
    fn the_servers_sign_in_and_keys_are_never_copied() {
        let mut paths = server("");
        paths.push("universe/auth.enc".into());
        paths.push("mods/Thing/secret.keys".into());
        paths.push("client_metadata.json".into());
        let p = the_plan(run(source(&paths), &[], &Context::default()));
        for c in &p.copy {
            assert!(!never_copied(&c.from), "{} would be copied", c.from);
            assert!(!c.to.ends_with(".bak"), "{} is a backup", c.to);
            assert!(!c.to.starts_with("logs/") && c.to != "config.json");
        }
        assert!(
            never_copied("a/b/AUTH.ENC")
                && never_copied("keyutil_x.crt")
                && never_copied("x/y.keys")
        );
    }

    #[test]
    fn a_single_player_save_imports_the_same_way_and_is_named_after_the_save() {
        let paths: Vec<String> = server("Saves/My Castle")
            .into_iter()
            .filter(|p| !p.contains("auth"))
            .collect();
        let p = the_plan(run(source(&paths), &[], &Context::default()));
        assert_eq!(p.world.as_ref().unwrap().name, "My Castle");
        assert_eq!(p.json[0].value, "default");
    }

    #[test]
    fn the_world_the_old_server_ran_is_the_one_players_spawn_in() {
        let mut paths = server("");
        paths.push("universe/worlds/Arena/config.json".into());
        paths.push("universe/worlds/Arena/chunks/0.0.region.bin".into());
        let p = the_plan(run(
            source(&paths),
            &[("config.json", r#"{"Defaults":{"World":"Arena"}}"#)],
            &Context::default(),
        ));
        assert_eq!(p.json[0].value, "Arena");
        // A config naming a world that isn't there falls back to "default".
        let p = the_plan(run(
            source(&paths),
            &[("config.json", r#"{"Defaults":{"World":"Gone"}}"#)],
            &Context::default(),
        ));
        assert_eq!(p.json[0].value, "default");
        assert_eq!(p.world.unwrap().facts[0].value, "Arena, default");
    }

    #[test]
    fn a_world_from_a_newer_hytale_or_set_to_wipe_or_needing_mods_is_warned_about() {
        let src = source(&server(""));
        let p = the_plan(run(
            src,
            &[(
                "universe/worlds/default/config.json",
                r#"{"Version":5,"DeleteOnUniverseStart":true,"RequiredPlugins":{"Foo":"1.0"}}"#,
            )],
            &Context::default(),
        ));
        assert_eq!(p.warnings.len(), 3, "{:?}", p.warnings);
    }

    #[test]
    fn several_universes_are_offered_then_one_is_imported() {
        let mut paths = server("Saves/Alpha");
        paths.extend(server("Saves/Beta"));
        let Step::Choose(c) = run(source(&paths), &[], &Context::default()) else {
            panic!()
        };
        let labels: Vec<&str> = c.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, ["Alpha", "Beta"]);
        let chosen = Context {
            runtime_version: None,
            world: Some("Saves/Beta/universe"),
        };
        let p = the_plan(run(source(&paths), &[], &chosen));
        assert!(p.copy.iter().all(|c| c.from.starts_with("Saves/Beta/")));
    }

    #[test]
    fn something_that_is_not_a_universe_is_refused() {
        for paths in [
            vec!["notes.txt".to_string()],
            vec!["universe/players/x.json".to_string()],
        ] {
            let Step::Refuse(r) = run(source(&paths), &[], &Context::default()) else {
                panic!("{paths:?}")
            };
            assert_eq!(r.code, RefusalCode::NoWorld);
        }
    }

    #[test]
    fn a_server_folder_named_by_its_id_has_no_name_to_show() {
        let mut src = source(&server(""));
        src.root_name = Some("6740f801-8c63-43b9-a898-e9f68a86061e".into());
        let p = the_plan(run(src, &[], &Context::default()));
        assert_eq!(p.world.unwrap().name, "");
    }
}

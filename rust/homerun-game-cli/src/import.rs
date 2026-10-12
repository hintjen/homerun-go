//! `homerun-game import`: stage a player's existing world for a new server.
//! [`apply`]: put a staged world into the server's folder at its first start.
//!
//! A game supports import when `homerun_core::engine::import` has an importer
//! for it. Everything an importer decides is there; everything here is
//! arguments, reading the source (`homerun_supervisor::world_import`),
//! writing the staging folder, and output.
//!
//! The staging folder is the host's. It holds `files/`, laid out exactly as
//! the files land in the server's folder, and `import.json`
//! (`homerun_core::engine::import::Staged`). The host keeps it until the
//! server's first start, which passes it back as `start.import`.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use homerun_core::engine::import::{
    self, Candidate, Context, Refusal, RefusalCode, Source, Staged, Step, World,
};
use homerun_core::engine::GameDescriptor;
use homerun_supervisor::world_import::{self as effects, Listing};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    prepare::{self, fail},
    protocol::{codes, Event},
    runner::Output,
};

pub const HELP: &str =
    "homerun-game import <game.json or slug> --source <path> --stage <folder> [options]

Stages a world from a .zip, a folder or one file so a new server starts with it.
The game must have an importer (homerun-game --features lists world-import:<game>).

Options:
  --descriptor <game.json>  The game, instead of the first argument
  --source <path>           What the player picked. Read, never changed
  --stage <folder>          Where the staged world goes. Must be empty or absent
  --world <id>              Which world, when the source holds several
  --runtime-version <v>     The game version the server will run, so a world
                            from a newer one is refused
  --json                    NDJSON events: import-candidates, import-progress,
                            import-staged, import-refused

Exit code 0 after import-staged or import-candidates, 2 after import-refused.";

/// What `import --json` prints, one line each. The names are the contract.
#[derive(Debug, Serialize)]
#[serde(tag = "event")]
enum ImportEvent<'a> {
    #[serde(rename = "import-candidates")]
    Candidates { candidates: &'a [Candidate] },
    #[serde(rename = "import-progress")]
    Progress {
        phase: &'static str,
        received: u64,
        total: u64,
    },
    #[serde(rename = "import-staged")]
    Staged {
        world: &'a Option<World>,
        settings: &'a Map<String, Value>,
        warnings: &'a [String],
    },
    #[serde(rename = "import-refused")]
    Refused { code: RefusalCode, message: &'a str },
}

impl ImportEvent<'_> {
    fn print(&self) {
        if let Ok(json) = serde_json::to_string(self) {
            println!("{json}");
        }
    }
}

/// The exit code a refusal ends with.
pub const REFUSED: i32 = 2;

/// Run `homerun-game import`. Returns the exit code; `Err` is a command that
/// could not run at all (bad arguments).
pub fn run(mut args: impl Iterator<Item = String>) -> Result<i32, String> {
    let mut options = BTreeMap::new();
    let (mut descriptor, mut as_json) = (None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => as_json = true,
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(0);
            }
            "--descriptor" | "--source" | "--stage" | "--world" | "--runtime-version" => {
                let value = args.next().ok_or_else(|| format!("{arg} needs a value."))?;
                if options.insert(arg.clone(), value).is_some() {
                    return Err(format!("{arg} was supplied twice."));
                }
            }
            _ if !arg.starts_with('-') && descriptor.is_none() => descriptor = Some(arg),
            _ => return Err(format!("Unknown argument: {arg}")),
        }
    }
    let descriptor = match (descriptor, options.remove("--descriptor")) {
        (Some(_), Some(_)) => return Err("Name the game once.".into()),
        (Some(d), None) | (None, Some(d)) => d,
        (None, None) => return Err("Choose a game descriptor or slug.".into()),
    };
    let source = options.remove("--source").ok_or("import needs --source.")?;
    let stage = options.remove("--stage").ok_or("import needs --stage.")?;
    let d = crate::client::read_descriptor(&descriptor)?;
    let stage = prepare::absolute(&stage).map_err(|(_, m)| m)?;
    if fs::read_dir(&stage).is_ok_and(|mut d| d.next().is_some()) {
        return Err("The staging folder must be empty.".into());
    }

    let ctx = Context {
        runtime_version: options.get("--runtime-version").map(String::as_str),
        world: options.get("--world").map(String::as_str),
    };
    let report = |event: ImportEvent| {
        if as_json {
            event.print();
        } else {
            human(&event);
        }
    };
    match stage_world(
        &d,
        Path::new(&source),
        &stage,
        &ctx,
        &mut |received, total| {
            report(ImportEvent::Progress {
                phase: "copy",
                received,
                total,
            })
        },
    ) {
        Ok(Outcome::Candidates(candidates)) => {
            report(ImportEvent::Candidates {
                candidates: &candidates,
            });
            Ok(0)
        }
        Ok(Outcome::Staged(plan)) => {
            report(ImportEvent::Staged {
                world: &plan.world,
                settings: &plan.settings,
                warnings: &plan.warnings,
            });
            Ok(0)
        }
        Err(refusal) => {
            report(ImportEvent::Refused {
                code: refusal.code,
                message: &refusal.message,
            });
            Ok(REFUSED)
        }
    }
}

fn human(event: &ImportEvent) {
    match event {
        ImportEvent::Candidates { candidates } => {
            println!("This holds more than one world. Choose one with --world:");
            for c in *candidates {
                println!(
                    "  {}  {}{}",
                    c.id,
                    c.label,
                    c.detail
                        .as_deref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default()
                );
            }
        }
        ImportEvent::Progress { .. } => {}
        ImportEvent::Staged {
            world, warnings, ..
        } => {
            if let Some(world) = world {
                if world.name.is_empty() {
                    println!("Staged the world.");
                } else {
                    println!("Staged {}.", world.name);
                }
                for fact in &world.facts {
                    println!("  {}: {}", fact.label, fact.value);
                }
            }
            for warning in *warnings {
                println!("Note: {warning}");
            }
        }
        ImportEvent::Refused { message, .. } => eprintln!("{message}"),
    }
}

/// What staging came to.
#[derive(Debug)]
pub enum Outcome {
    Candidates(Vec<Candidate>),
    Staged(import::Plan),
}

/// List `source`, run the game's importer, and stage what it plans.
///
/// `stage` must be empty or absent. On a refusal it is left as it was found.
pub fn stage_world(
    d: &GameDescriptor,
    source: &Path,
    stage: &Path,
    ctx: &Context,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Outcome, Refusal> {
    let importer = import::importer(&d.id).ok_or_else(|| {
        Refusal::new(
            RefusalCode::NoImporter,
            format!("Homerun can't import worlds for {} yet.", display_name(d)),
        )
    })?;
    let Listing {
        opened,
        entries,
        root_name,
    } = effects::open(source)?;
    let mut src = Source {
        entries,
        read: BTreeMap::new(),
        root_name,
    };
    for _ in 0..import::MAX_ROUNDS {
        match import::plan(importer, &src, ctx) {
            Step::Read(reads) => {
                for read in reads {
                    if src.read.contains_key(&read.path) {
                        continue;
                    }
                    let bytes = effects::read_prefix(&opened, &read.path, read.max_bytes)?;
                    src.read.insert(read.path, bytes);
                }
            }
            Step::Choose(candidates) => return Ok(Outcome::Candidates(candidates)),
            Step::Refuse(refusal) => return Err(refusal),
            Step::Plan(plan) => {
                copy_into_stage(&opened, &src, &plan, d, stage, progress)?;
                return Ok(Outcome::Staged(plan));
            }
        }
    }
    Err(Refusal::new(
        RefusalCode::ReadFailed,
        "Homerun couldn't work out which world this is.",
    ))
}

fn display_name(d: &GameDescriptor) -> &str {
    if d.name.is_empty() {
        &d.id
    } else {
        &d.name
    }
}

fn copy_into_stage(
    opened: &effects::Opened,
    src: &Source,
    plan: &import::Plan,
    d: &GameDescriptor,
    stage: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), Refusal> {
    let existed = stage.exists();
    let total: u64 = plan
        .copy
        .iter()
        .filter_map(|c| src.entry(&c.from))
        .map(|e| e.size)
        .sum();
    let result = (|| {
        let files = stage.join("files");
        let mut received = 0;
        progress(0, total);
        for copy in &plan.copy {
            // `plan` checked every destination with `safe_relative`.
            let dest = copy
                .to
                .split('/')
                .fold(files.clone(), |p, part| p.join(part));
            let mut since = 0u64;
            effects::copy_out(opened, &copy.from, &dest, &mut |n| {
                received += n;
                since += n;
                if since >= 8 * 1024 * 1024 {
                    since = 0;
                    progress(received, total);
                }
            })?;
            progress(received, total);
        }
        let staged =
            serde_json::to_vec_pretty(&Staged::new(&d.id, plan)).map_err(|_| cannot_stage())?;
        fs::write(stage.join("import.json"), staged).map_err(|_| cannot_stage())
    })();
    if result.is_err() {
        // Leave the folder as it was found: absent, or empty.
        if existed {
            if let Ok(children) = fs::read_dir(stage) {
                for child in children.flatten() {
                    let _ =
                        fs::remove_dir_all(child.path()).or_else(|_| fs::remove_file(child.path()));
                }
            }
        } else {
            let _ = fs::remove_dir_all(stage);
        }
    }
    result
}

fn cannot_stage() -> Refusal {
    Refusal::new(
        RefusalCode::ReadFailed,
        "Homerun couldn't save a copy of the world on this computer. Check there's free space and try again.",
    )
}

// ─── the first start ─────────────────────────────────────────────────────────

/// Put a staged world into `server` before the game's config is written.
///
/// Nothing happens when the server already has a world (one of the staged
/// `present` patterns matches): that is how a restore, which the host runs
/// before `start`, wins over an import, and why passing the same staging
/// folder on every start is harmless. Files are copied only where nothing is
/// already, then the staged INI keys are set with the same merge the game's
/// config uses.
pub fn apply(
    d: &GameDescriptor,
    server: &Path,
    stage: &str,
    id: &str,
    out: &Output,
) -> prepare::Result<()> {
    let failed = |message: &str| fail(codes::IMPORT_FAILED, message);
    let stage = PathBuf::from(stage);
    if !stage.is_absolute() {
        return Err(failed(
            "The imported world's folder must be an absolute path.",
        ));
    }
    let staged: Staged = fs::read(stage.join("import.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| {
            failed("The imported world can't be found on this computer. Import it again.")
        })?;
    staged.check(&d.id).map_err(|m| failed(&m))?;
    // " Terrarua", or nothing for a world with no name to show.
    let named = staged
        .world
        .as_ref()
        .map(|w| w.name.trim())
        .filter(|n| !n.is_empty())
        .map(|n| format!(" {n}"))
        .unwrap_or_default();
    let log = |line: String| {
        out.send(Event::ServerLog {
            server_id: id.into(),
            line,
            stream: "host".into(),
        })
    };

    if staged.present.iter().any(|p| present(server, p)) {
        log(format!(
            "This server already has a world, so the imported world{named} was not copied in."
        ));
        return Ok(());
    }

    let files = stage.join("files");
    let mut staged_files = Vec::new();
    collect(&files, "", &mut staged_files)
        .map_err(|_| failed("The imported world's files can't be read. Import it again."))?;
    for rel in &staged_files {
        let dest = prepare::confined(server, rel).map_err(|_| {
            failed("The imported world has a file that would leave the server's folder.")
        })?;
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|_| {
                failed("The imported world could not be copied into the server's folder.")
            })?;
        }
        let from = rel.split('/').fold(files.clone(), |p, part| p.join(part));
        fs::copy(&from, &dest).map_err(|_| {
            failed("The imported world could not be copied into the server's folder.")
        })?;
    }

    let mut by_file: BTreeMap<&str, Vec<(String, Value)>> = BTreeMap::new();
    for edit in &staged.ini {
        by_file
            .entry(edit.file.as_str())
            .or_default()
            .push((edit.managed_key(), Value::String(edit.value.clone())));
    }
    for (file, keys) in by_file {
        let path = prepare::confined(server, file).map_err(|_| {
            failed("The imported world's settings file would leave the server's folder.")
        })?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => return Err(failed("A server settings file can't be read.")),
        };
        let (text, encoding) = homerun_core::ini_config::decode(&bytes)
            .ok_or_else(|| failed("A server settings file can't be read."))?;
        let merged = homerun_core::ini_config::merge(&text, &keys, &[]).map_err(|_| {
            failed("The server's settings could not be pointed at the imported world.")
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|_| failed("A server settings folder can't be created."))?;
        }
        fs::write(&path, homerun_core::ini_config::encode(&merged, encoding))
            .map_err(|_| failed("A server settings file can't be saved."))?;
    }
    log(format!("Imported the world{named}."));
    Ok(())
}

/// Whether anything in `root` matches a `present` pattern.
fn present(root: &Path, pattern: &str) -> bool {
    let parts: Vec<&str> = pattern.split('/').collect();
    fn walk(dir: &Path, parts: &[&str]) -> bool {
        let Some((first, rest)) = parts.split_first() else {
            return dir.exists();
        };
        if *first == "*" {
            fs::read_dir(dir)
                .map(|children| children.flatten().any(|c| walk(&c.path(), rest)))
                .unwrap_or(false)
        } else {
            walk(&dir.join(first), rest)
        }
    }
    walk(root, &parts)
}

/// Every file under `dir`, `/`-separated, skipping symbolic links.
fn collect(dir: &Path, prefix: &str, out: &mut Vec<String>) -> std::io::Result<()> {
    for child in fs::read_dir(dir)? {
        let child = child?;
        let meta = fs::symlink_metadata(child.path())?;
        let Some(name) = child.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if meta.is_dir() {
            collect(&child.path(), &rel, out)?;
        } else if meta.is_file() {
            out.push(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("homerun-import-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn descriptor(id: &str) -> GameDescriptor {
        serde_json::from_value(serde_json::json!({ "schema": 0, "id": id, "name": id })).unwrap()
    }

    fn output() -> (Output, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = lines.clone();
        (
            Output::new(move |e| {
                if let Event::ServerLog { line, .. } = e {
                    seen.lock().unwrap().push(line);
                }
            }),
            lines,
        )
    }

    /// A Palworld download staged, then applied to an empty server: the world
    /// lands under its own name and the server is pointed at it, with the
    /// settings file's other lines and its line endings kept.
    #[test]
    fn a_palworld_world_is_staged_and_then_lands_in_an_empty_server() {
        let dir = scratch("pal");
        let world = dir.join("download/Pal/Saved/SaveGames/0/ABCD");
        fs::create_dir_all(world.join("Players")).unwrap();
        fs::write(world.join("Level.sav"), b"level").unwrap();
        fs::write(world.join("LevelMeta.sav"), b"meta").unwrap();
        fs::write(world.join("WorldOption.sav"), b"opt").unwrap();
        fs::write(world.join("Players/F6EC.sav"), b"player").unwrap();
        let d = descriptor("palworld");
        let stage = dir.join("stage");
        let staged = stage_world(
            &d,
            &dir.join("download"),
            &stage,
            &Context::default(),
            &mut |_, _| (),
        )
        .unwrap();
        assert!(matches!(staged, Outcome::Staged(_)));
        assert!(stage
            .join("files/Saved/SaveGames/0/ABCD/Level.sav")
            .is_file());
        assert!(!stage
            .join("files/Saved/SaveGames/0/ABCD/WorldOption.sav")
            .exists());

        let server = dir.join("server");
        let settings = server.join("Saved/Config/WindowsServer/GameUserSettings.ini");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            "[/Script/Pal.PalGameLocalSettings]\r\nAudio=1\r\nDedicatedServerName=OLD\r\n",
        )
        .unwrap();
        let (out, lines) = output();
        apply(&d, &server, &stage.to_string_lossy(), "s1", &out).unwrap();
        assert_eq!(
            fs::read(server.join("Saved/SaveGames/0/ABCD/Players/F6EC.sav")).unwrap(),
            b"player"
        );
        let text = fs::read_to_string(&settings).unwrap();
        assert!(text.contains("DedicatedServerName=ABCD\r\n"), "{text:?}");
        assert!(text.contains("Audio=1\r\n"));
        assert!(!text.contains("OLD"));
        assert_eq!(lines.lock().unwrap()[0], "Imported the world.");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A restore runs before `start`; when it has put a world back, the
    /// import must not land on top of it.
    #[test]
    fn a_server_that_already_has_a_world_keeps_it() {
        let dir = scratch("present");
        fs::write(dir.join("w.wld"), terraria_header()).unwrap();
        let d = descriptor("terraria");
        let stage = dir.join("stage");
        stage_world(
            &d,
            &dir.join("w.wld"),
            &stage,
            &Context::default(),
            &mut |_, _| (),
        )
        .unwrap();
        let server = dir.join("server");
        fs::create_dir_all(server.join("data/Worlds")).unwrap();
        fs::write(server.join("data/Worlds/world.wld"), b"restored").unwrap();
        let (out, lines) = output();
        apply(&d, &server, &stage.to_string_lossy(), "s1", &out).unwrap();
        assert_eq!(
            fs::read(server.join("data/Worlds/world.wld")).unwrap(),
            b"restored"
        );
        assert!(lines.lock().unwrap()[0].contains("already has a world"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusal_leaves_the_staging_folder_as_it_was() {
        let dir = scratch("refused");
        fs::write(dir.join("notes.txt"), b"no world here").unwrap();
        let stage = dir.join("stage");
        let refused = stage_world(
            &descriptor("terraria"),
            &dir,
            &stage,
            &Context::default(),
            &mut |_, _| (),
        )
        .unwrap_err();
        assert_eq!(refused.code, RefusalCode::NoWorld);
        assert!(!stage.exists());
        let refused = stage_world(
            &descriptor("rust"),
            &dir,
            &stage,
            &Context::default(),
            &mut |_, _| (),
        )
        .unwrap_err();
        assert_eq!(refused.code, RefusalCode::NoImporter);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_staged_world_for_another_game_is_not_applied() {
        let dir = scratch("other-game");
        fs::write(dir.join("w.wld"), terraria_header()).unwrap();
        let stage = dir.join("stage");
        stage_world(
            &descriptor("terraria"),
            &dir.join("w.wld"),
            &stage,
            &Context::default(),
            &mut |_, _| (),
        )
        .unwrap();
        let (out, _) = output();
        let (code, _) = apply(
            &descriptor("palworld"),
            &dir.join("server"),
            &stage.to_string_lossy(),
            "s1",
            &out,
        )
        .unwrap_err();
        assert_eq!(code, codes::IMPORT_FAILED);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The event names and fields the desktop reads.
    #[test]
    fn the_json_lines_are_spelled_for_the_desktop() {
        let refused = serde_json::to_value(ImportEvent::Refused {
            code: RefusalCode::TooNew,
            message: "m",
        })
        .unwrap();
        assert_eq!(refused["event"], "import-refused");
        assert_eq!(refused["code"], "too_new");
        let progress = serde_json::to_value(ImportEvent::Progress {
            phase: "copy",
            received: 1,
            total: 2,
        })
        .unwrap();
        assert_eq!(progress["event"], "import-progress");
        let candidates = [Candidate {
            id: "a".into(),
            label: "A".into(),
            detail: None,
            size_bytes: 3,
        }];
        let line = serde_json::to_value(ImportEvent::Candidates {
            candidates: &candidates,
        })
        .unwrap();
        assert_eq!(line["event"], "import-candidates");
        assert_eq!(line["candidates"][0]["sizeBytes"], 3);
        let (settings, warnings, world) = (Map::new(), Vec::<String>::new(), None);
        let staged = serde_json::to_value(ImportEvent::Staged {
            world: &world,
            settings: &settings,
            warnings: &warnings,
        })
        .unwrap();
        assert_eq!(staged["event"], "import-staged");
    }

    /// A small 1.4.5.8 world's header, enough for the importer.
    fn terraria_header() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(326i32.to_le_bytes());
        b.extend(b"relogic");
        b.push(2);
        b.extend([0u8; 12]);
        b.extend(1i16.to_le_bytes());
        b.extend(30i32.to_le_bytes());
        b.push(1);
        b.extend(b"W");
        b.push(1);
        b.extend(b"1");
        b.extend([0u8; 8 + 16 + 4 + 16]);
        b.extend(1200i32.to_le_bytes());
        b.extend(4200i32.to_le_bytes());
        b.extend(0i32.to_le_bytes());
        b
    }
}

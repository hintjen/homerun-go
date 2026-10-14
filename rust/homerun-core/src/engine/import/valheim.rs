//! Valheim: a world folder from Valheim 1.0, or a `.fwl`/`.db` pair from
//! before it.
//!
//! The descriptor runs the server with `-savedir save`, so its worlds live in
//! `save/worlds_local/`, and `-world <name>` picks one by its **folder or file
//! name**, not by the name inside it. Importing is placing the world there
//! under a name, and handing that name back as the `worldName` setting the new
//! server is created with. Characters (`.fch`) stay on each player's computer.
//!
//! # The two formats
//!
//! - **1.0** (observed on server 1.0.17): a folder `<name>/` holding, for
//!   its newest save generation `n`, `_main.<n>.fwl2` (metadata),
//!   `_main.<n>.db2`, `_main.<n>.chunks` and `_main.<n>.ok`, plus chunk files
//!   named like `00_00__0_1.chunk`. The server removes older generations when
//!   it saves. The `.ok` is written last (four bytes: the save format, the
//!   same number as in the `.fwl2`), so a generation without one is a save
//!   that never finished. A world the server generated but never saved holds
//!   only `_main.0.fwl2`, and is not a world it can load.
//! - **Before 1.0**: `<name>.fwl` (metadata) and `<name>.db`, in
//!   `worlds_local/` or the older `worlds/`. The 1.0 server converts such a
//!   pair the first time it loads it: one way, silent, and minutes long for a
//!   big world, which is what the warning says.
//!
//! # The metadata
//!
//! Both `.fwl` and `.fwl2` start the same way: an int32 length of the rest,
//! an int32 version (the save format; 41 from 1.0.17, 26 in a 2024 world),
//! the world's name and its seed as .NET strings, and the int32 seed. That is
//! all this reads. A `.fwl` then has a uid, a generator version and, from
//! version 30, world modifiers; what follows the seed in a `.fwl2` is not
//! documented and is not depended on. A `.db` starts with its int32 version.
//!
//! # Backups
//!
//! Valheim keeps `<name>_backup_<yyyymmdd-hhmmss>.fwl/.db` copies. They **are
//! offered**, labelled as backups, but only when the world they back up is
//! not in what was picked: then they are often the only copy a player has
//! left (1.0 converted the world in place, the cloud kept another, or the
//! original was moved aside as `.old`), and when the world itself is there a
//! backup is only noise. A backup lands under the world's own name, without
//! the suffix, as Valheim's own advice for restoring one says, when the
//! suffix is exactly that date and the name left is one the server can be
//! given; otherwise under its whole file name. A 1.0 folder whose name holds
//! `_backup` is treated the same way.
//!
//! Never offered: `.old` files (the save before the last), the automatic
//! `_backup_auto-*` copies, and anything [`never_read`] names.
//!
//! # What is refused
//!
//! - A character (`.fch`) or nothing Valheim at all: `no_world`.
//! - A 1.0 folder with no finished save, or a `.fwl` without its `.db`:
//!   `read_failed`, because the world is not all there.
//! - Metadata this cannot read, and a Game Pass save (`containers.index`),
//!   which is a container of unnamed blobs rather than world files:
//!   `unsupported_world`.
//! - A world newer than the server: `too_new` when the host names a release
//!   in [`RELEASES`], a warning otherwise.

use serde_json::{json, Map};

use super::{
    file_name, human_size, join, parent, safe_relative, Candidate, Context, CopyFile, Fact,
    Importer, Plan, Read, Refusal, RefusalCode, Source, Step, World,
};

pub const IMPORTER: Importer = Importer {
    game: "valheim",
    step,
};

/// Where the server looks for worlds, relative to its folder.
const WORLDS: &str = "save/worlds_local";

/// A `.fwl` is about fifty bytes; this is the whole of any real one.
const FWL_BYTES: u64 = 64 * 1024;
/// A `.fwl2` is about fifty bytes too.
const FWL2_BYTES: u64 = 4 * 1024;
/// A `.db` is read for its version, an `.ok` for being whole.
const HEAD_BYTES: u64 = 16;

const MAX_CANDIDATES: usize = 32;

/// The save format each server release writes, observed by saving a world
/// with each one (2026-10-13). Add a row when a new release is tested.
pub const RELEASES: &[(&str, i32)] = &[("1.0.17", 41)];

/// Whether a file is never read or copied, wherever it sits: characters,
/// the old server's permission lists, client preferences, logs, the map
/// texture caches the client rebuilds, and Steam's cloud bookkeeping.
pub fn never_read(path: &str) -> bool {
    path.split('/').any(|part| {
        let p = part.to_ascii_lowercase();
        p.ends_with(".fch")
            || p.contains(".fch.")
            || p.starts_with("characters")
            || p == "adminlist.txt"
            || p == "bannedlist.txt"
            || p == "permittedlist.txt"
            || p == "prefs"
            || p == "logs"
            || p.ends_with(".log")
            || p.ends_with("_heighttexcache")
            || p.ends_with("_maptexcache")
            || p.ends_with("_forestmasktexcache")
            || p == "steam_autocloud.vdf"
    })
}

/// The metadata at the start of a `.fwl` or `.fwl2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub version: i32,
    pub name: String,
    pub seed_name: String,
    pub seed: i32,
}

/// Why metadata could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaError {
    Truncated,
    NotAWorld,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], MetaError> {
        let end = self.at.checked_add(n).ok_or(MetaError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(MetaError::Truncated)?;
        self.at = end;
        Ok(slice)
    }
    fn i32(&mut self) -> Result<i32, MetaError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    /// A .NET `BinaryWriter` string: a 7-bit length, then UTF-8.
    fn string(&mut self) -> Result<String, MetaError> {
        let (mut len, mut shift) = (0usize, 0);
        loop {
            let b = self.take(1)?[0];
            len |= ((b & 0x7F) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 28 {
                return Err(MetaError::NotAWorld);
            }
        }
        if len > 1024 {
            return Err(MetaError::NotAWorld);
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| MetaError::NotAWorld)
    }
}

/// Read a world's metadata from its `.fwl` or `.fwl2`.
pub fn metadata(bytes: &[u8]) -> Result<Meta, MetaError> {
    let mut r = Reader { bytes, at: 0 };
    let rest = r.i32()?;
    if !(8..=FWL_BYTES as i32).contains(&rest) {
        return Err(MetaError::NotAWorld);
    }
    let version = r.i32()?;
    if !(1..=1000).contains(&version) {
        return Err(MetaError::NotAWorld);
    }
    let name = r.string()?;
    let seed_name = r.string()?;
    let seed = r.i32()?;
    // The fields read so far must sit inside the length the file declares.
    if r.at > rest as usize + 4 {
        return Err(MetaError::NotAWorld);
    }
    Ok(Meta {
        version,
        name,
        seed_name,
        seed,
    })
}

/// The version at the start of a `.db` (or an `.ok`), if it is one.
fn leading_version(bytes: &[u8]) -> Option<Result<i32, MetaError>> {
    let head: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let version = i32::from_le_bytes(head);
    Some(if (1..=1000).contains(&version) {
        Ok(version)
    } else {
        Err(MetaError::NotAWorld)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// A 1.0 world folder (`""` when it is what was picked) and its newest
    /// finished save generation.
    Folder { dir: String, generation: u64 },
    /// A pre-1.0 `.fwl` and the `.db` beside it.
    Legacy { fwl: String, db: String },
}

/// A world in the source.
#[derive(Debug, Clone)]
struct Found {
    kind: Kind,
    /// The name the server will know it by. Empty for a folder picked
    /// directly with no name to go by; its metadata names it then.
    name: String,
    /// `Some` for a backup Valheim made, with the date in its name if any.
    backup: Option<Option<String>>,
    /// The name the backup is of, to hide it when that world is here too.
    backup_of: Option<String>,
    size: u64,
}

impl Found {
    /// What the runner is given back as `--world`: the metadata file.
    fn id(&self) -> String {
        match &self.kind {
            Kind::Folder { dir, generation } => join(dir, &format!("_main.{generation}.fwl2")),
            Kind::Legacy { fwl, .. } => fwl.clone(),
        }
    }

    /// The metadata file, and the file that says the world is whole.
    fn reads(&self) -> [Read; 2] {
        match &self.kind {
            Kind::Folder { dir, generation } => [
                Read {
                    path: self.id(),
                    max_bytes: FWL2_BYTES,
                },
                Read {
                    path: join(dir, &format!("_main.{generation}.ok")),
                    max_bytes: HEAD_BYTES,
                },
            ],
            Kind::Legacy { fwl, db } => [
                Read {
                    path: fwl.clone(),
                    max_bytes: FWL_BYTES,
                },
                Read {
                    path: db.clone(),
                    max_bytes: HEAD_BYTES,
                },
            ],
        }
    }

    fn legacy(&self) -> bool {
        matches!(self.kind, Kind::Legacy { .. })
    }
}

/// `_main.<n>.<ext>` → `(n, ext)`.
fn generation_file(name: &str) -> Option<(u64, &str)> {
    let rest = name.strip_prefix("_main.")?;
    let (n, ext) = rest.split_once('.')?;
    Some((n.parse().ok()?, ext))
}

/// A name the server can be given as `-world` and that can be a folder or
/// file name: one path part, not an option, no control characters.
fn usable_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && safe_relative(name)
        && !name.contains('/')
        && !name.starts_with(['-', ' '])
}

fn is_auto_backup(name: &str) -> bool {
    name.to_ascii_lowercase().contains("_backup_auto")
}

/// For a backup's name, the world it is of and the date in its suffix when
/// that suffix is exactly `_backup_<yyyymmdd-hhmmss>`.
fn backup_parts(name: &str) -> Option<(&str, Option<String>)> {
    let at = name.to_ascii_lowercase().find("_backup")?;
    let (base, suffix) = name.split_at(at);
    let date = suffix
        .strip_prefix("_backup_")
        .filter(|d| {
            d.len() == 15
                && d.char_indices()
                    .all(|(i, c)| if i == 8 { c == '-' } else { c.is_ascii_digit() })
        })
        .map(|d| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8]));
    Some((base, date))
}

/// Name a world found under `name` (a folder or file name).
fn named(kind: Kind, name: &str, size: u64) -> Found {
    let (server_name, backup, backup_of) = match backup_parts(name) {
        Some((base, date)) => {
            let stripped = date.is_some() && usable_name(base);
            (
                if stripped { base } else { name }.to_string(),
                Some(date),
                Some(base.to_string()),
            )
        }
        None => (name.to_string(), None, None),
    };
    Found {
        kind,
        name: server_name,
        backup,
        backup_of,
        size,
    }
}

/// Every world in the source, backups of worlds that are here left out.
fn worlds(source: &Source) -> Vec<Found> {
    let entries = || source.entries.iter().filter(|e| !never_read(&e.path));
    let mut found = Vec::new();

    // 1.0 folders: the newest generation with all of .fwl2, .db2 and .ok.
    let mut dirs: Vec<&str> = entries()
        .filter(|e| generation_file(file_name(&e.path)).is_some())
        .map(|e| parent(&e.path))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    for dir in dirs {
        let name = if dir.is_empty() {
            source.root_name.clone().unwrap_or_default()
        } else {
            file_name(dir).to_string()
        };
        if is_auto_backup(&name) {
            continue;
        }
        let has = |n: u64, ext: &str| source.has(&join(dir, &format!("_main.{n}.{ext}")));
        let newest = entries()
            .filter(|e| parent(&e.path) == dir)
            .filter_map(|e| generation_file(file_name(&e.path)))
            .map(|(n, _)| n)
            .filter(|n| has(*n, "fwl2") && has(*n, "db2") && has(*n, "ok"))
            .max();
        let Some(generation) = newest else { continue };
        let kind = Kind::Folder {
            dir: dir.to_string(),
            generation,
        };
        let size = folder_files(source, dir, generation)
            .map(|(_, _, size)| size)
            .sum();
        found.push(named(kind, &name, size));
    }

    // Pre-1.0 pairs.
    for fwl in entries().filter(|e| e.path.to_ascii_lowercase().ends_with(".fwl")) {
        let stem = &file_name(&fwl.path)[..file_name(&fwl.path).len() - ".fwl".len()];
        if is_auto_backup(stem) {
            continue;
        }
        let Some(db) = legacy_db(source, &fwl.path) else {
            continue;
        };
        let kind = Kind::Legacy {
            fwl: fwl.path.clone(),
            db: db.path.clone(),
        };
        found.push(named(kind, stem, fwl.size + db.size));
    }

    let worlds: Vec<String> = found
        .iter()
        .filter(|f| f.backup.is_none())
        .map(|f| f.name.to_ascii_lowercase())
        .collect();
    found.retain(|f| {
        f.backup_of
            .as_ref()
            .is_none_or(|of| !worlds.contains(&of.to_ascii_lowercase()))
    });
    found
}

/// The `.db` beside a `.fwl`.
fn legacy_db<'a>(source: &'a Source, fwl: &str) -> Option<&'a super::Entry> {
    let db = format!("{}.db", &fwl[..fwl.len() - ".fwl".len()]);
    source
        .entries
        .iter()
        .find(|e| e.path.eq_ignore_ascii_case(&db) && !never_read(&e.path))
}

/// A 1.0 world's files: the generation's own and every chunk, as
/// `(path, relative to the folder, size)`.
fn folder_files<'a>(
    source: &'a Source,
    dir: &'a str,
    generation: u64,
) -> impl Iterator<Item = (&'a str, &'a str, u64)> + 'a {
    source.entries.iter().filter_map(move |e| {
        if never_read(&e.path) {
            return None;
        }
        let rel = if dir.is_empty() {
            e.path.as_str()
        } else {
            e.path.strip_prefix(dir)?.strip_prefix('/')?
        };
        let name = file_name(rel);
        let ours = (!rel.contains('/')
            && generation_file(name).is_some_and(|(n, _)| n == generation))
            || name.to_ascii_lowercase().ends_with(".chunk");
        ours.then_some((e.path.as_str(), rel, e.size))
    })
}

/// The refusal for a source with no world in it, in the words that fit what
/// is there instead.
fn nothing_found(source: &Source) -> Refusal {
    let names: Vec<String> = source
        .entries
        .iter()
        .map(|e| file_name(&e.path).to_ascii_lowercase())
        .collect();
    let any = |f: &dyn Fn(&str) -> bool| names.iter().any(|n| f(n));
    if any(&|n| n == "containers.index") {
        return Refusal::new(
            RefusalCode::UnsupportedWorld,
            "This looks like a Game Pass (Xbox app) save. Valheim keeps those in a container Homerun can't read. Homerun can import a world from Valheim's worlds_local folder on Steam, or from another server's save folder.",
        );
    }
    let unsaved = source.entries.iter().any(|e| {
        !never_read(&e.path)
            && generation_file(file_name(&e.path)).is_some_and(|(_, ext)| ext == "fwl2")
    });
    if unsaved {
        return Refusal::new(
            RefusalCode::ReadFailed,
            "This world was never saved, so there is nothing in it to import. Start it in Valheim, play for a moment, save, and pick it again.",
        );
    }
    let lone_fwl = source.entries.iter().any(|e| {
        !never_read(&e.path)
            && e.path.to_ascii_lowercase().ends_with(".fwl")
            && !is_auto_backup(file_name(&e.path))
            && legacy_db(source, &e.path).is_none()
    });
    if lone_fwl {
        return Refusal::new(
            RefusalCode::ReadFailed,
            "This world's .db file isn't with its .fwl file, and the .db is the world itself. Pick the folder that holds both.",
        );
    }
    if any(&|n| n.ends_with(".fwl.old")) {
        return Refusal::new(
            RefusalCode::NoWorld,
            "There is only an older copy of a world here (a .fwl.old file). To use it, copy its .fwl.old and .db.old files, rename the copies to end in .fwl and .db, and pick them again.",
        );
    }
    if any(&|n| n.ends_with(".fch") || n.contains(".fch.")) {
        return Refusal::new(
            RefusalCode::NoWorld,
            "This is a Valheim character, not a world. Characters stay on each player's computer. Pick the world's folder in worlds_local instead.",
        );
    }
    Refusal::new(
        RefusalCode::NoWorld,
        "There is no Valheim world in what you picked. A world is a folder in worlds_local, or a .fwl file with its .db from before Valheim 1.0.",
    )
}

fn step(source: &Source, ctx: &Context) -> Step {
    let mut found = worlds(source);
    if let Some(chosen) = ctx.world {
        found.retain(|f| f.id() == chosen);
    }
    if found.is_empty() {
        return Step::Refuse(nothing_found(source));
    }
    found.sort_by(|a, b| b.size.cmp(&a.size).then(a.id().cmp(&b.id())));
    found.truncate(MAX_CANDIDATES);

    let unread: Vec<Read> = found
        .iter()
        .flat_map(Found::reads)
        .filter(|r| source.bytes(&r.path).is_none())
        .collect();
    if !unread.is_empty() {
        return Step::Read(unread);
    }

    if found.len() > 1 {
        let candidates = found
            .iter()
            .map(|f| {
                let meta = source.bytes(&f.id()).map(metadata);
                let (label, detail) = match meta {
                    Some(Ok(m)) => (
                        shown_name(&m, f),
                        [
                            Some(format_name(f, false).to_string()),
                            f.backup.as_ref().map(|date| match date {
                                Some(d) => format!("Backup from {d}"),
                                None => "Backup".into(),
                            }),
                            shown(&m.seed_name).then(|| format!("Seed {}", m.seed_name)),
                            Some(human_size(f.size)),
                        ]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · "),
                    ),
                    _ => (
                        if f.name.is_empty() {
                            file_name(&f.id()).to_string()
                        } else {
                            f.name.clone()
                        },
                        "Not a world Homerun can read".into(),
                    ),
                };
                Candidate {
                    id: f.id(),
                    label,
                    detail: Some(detail),
                    size_bytes: f.size,
                }
            })
            .collect();
        return Step::Choose(candidates);
    }
    plan_one(source, ctx, &found[0])
}

fn shown(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(char::is_control)
}

fn shown_name(meta: &Meta, found: &Found) -> String {
    if shown(&meta.name) {
        meta.name.clone()
    } else {
        found.name.clone()
    }
}

fn format_name(found: &Found, long: bool) -> &'static str {
    match (found.legacy(), long) {
        (false, _) => "Valheim 1.0",
        (true, false) => "Before 1.0",
        (true, true) => "From before Valheim 1.0",
    }
}

fn plan_one(source: &Source, ctx: &Context, found: &Found) -> Step {
    let refuse = |code, message: &str| Step::Refuse(Refusal::new(code, message));
    let [meta_read, check_read] = found.reads();
    let meta = match metadata(source.bytes(&meta_read.path).unwrap_or_default()) {
        Ok(m) => m,
        Err(MetaError::Truncated) => {
            return refuse(
                RefusalCode::ReadFailed,
                "This world's files are incomplete. Copy the world again and retry.",
            )
        }
        Err(MetaError::NotAWorld) => {
            return refuse(
                RefusalCode::UnsupportedWorld,
                "This isn't a Valheim world Homerun can read.",
            )
        }
    };
    match leading_version(source.bytes(&check_read.path).unwrap_or_default()) {
        Some(Ok(_)) => {}
        None if found.legacy() => {
            return refuse(
                RefusalCode::ReadFailed,
                "This world's .db file is empty or incomplete. Copy the world again and retry.",
            )
        }
        None => {
            return refuse(
                RefusalCode::ReadFailed,
                "This world's last save didn't finish. Open it in Valheim or on its server, let it save, and pick it again.",
            )
        }
        Some(Err(_)) => {
            return refuse(
                RefusalCode::UnsupportedWorld,
                "This isn't a Valheim world Homerun can read.",
            )
        }
    }

    // A folder picked directly is named by its metadata, which names the
    // world as Valheim made it, rather than by a zip's download name.
    let name = match &found.kind {
        Kind::Folder { dir, .. } if dir.is_empty() && usable_name(&meta.name) => meta.name.clone(),
        _ => found.name.clone(),
    };
    if !usable_name(&name) {
        return refuse(
            RefusalCode::UnsupportedWorld,
            "This world's name can't be used for a server. Rename its folder or files to plain letters and numbers, and pick it again.",
        );
    }

    let mut warnings = Vec::new();
    let newest = RELEASES.iter().map(|r| r.1).max().unwrap_or_default();
    match ctx
        .runtime_version
        .and_then(|v| RELEASES.iter().find(|r| r.0 == v))
    {
        Some((release, max)) if meta.version > *max => {
            return Step::Refuse(Refusal::new(
                RefusalCode::TooNew,
                format!(
                    "This world was saved by a newer version of Valheim than this server runs ({release}). Choose a newer server version, or import an older copy of the world."
                ),
            ))
        }
        Some(_) => {}
        None if meta.version > newest => warnings.push(
            "This world was saved by a newer version of Valheim than Homerun has tested. If the server won't start, choose the newest server version.".into(),
        ),
        None => {}
    }
    if found.legacy() {
        warnings.push(
            "This world is from before Valheim 1.0. The server converts it the first time it starts, which can take several minutes, and the converted world can't go back to an older Valheim.".into(),
        );
    }
    match &found.backup {
        Some(Some(date)) => warnings.push(format!(
            "This is a backup Valheim made on {date}. Anything built after that isn't in it."
        )),
        Some(None) => warnings.push(
            "This is a backup copy of a world. Anything built after it was made isn't in it."
                .into(),
        ),
        None => {}
    }

    let copy: Vec<CopyFile> = match &found.kind {
        Kind::Folder { dir, generation } => folder_files(source, dir, *generation)
            .map(|(path, rel, _)| CopyFile {
                from: path.to_string(),
                to: format!("{WORLDS}/{name}/{rel}"),
            })
            .collect(),
        Kind::Legacy { fwl, db } => vec![
            CopyFile {
                from: fwl.clone(),
                to: format!("{WORLDS}/{name}.fwl"),
            },
            CopyFile {
                from: db.clone(),
                to: format!("{WORLDS}/{name}.db"),
            },
        ],
    };

    let mut facts = vec![Fact {
        label: "Format".into(),
        value: format_name(found, true).into(),
    }];
    if let Some(Some(date)) = &found.backup {
        facts.push(Fact {
            label: "Backup from".into(),
            value: date.clone(),
        });
    }
    if shown(&meta.seed_name) {
        facts.push(Fact {
            label: "Seed".into(),
            value: meta.seed_name.clone(),
        });
    }
    facts.push(Fact {
        label: "Size".into(),
        value: human_size(found.size),
    });

    let mut settings = Map::new();
    settings.insert("worldName".into(), json!(name));
    Step::Plan(Plan {
        world: Some(World {
            name: shown_name(&meta, found),
            facts,
        }),
        settings,
        warnings,
        copy,
        // A world by this name, in either format: the server would load it
        // (or convert it) instead.
        present: vec![format!("{WORLDS}/{name}"), format!("{WORLDS}/{name}.fwl")],
        ini: Vec::new(),
        json: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{plan, Entry};
    use super::*;

    /// A world server 1.0.17 generated and saved once (2026-10-13): its
    /// `_main.1.fwl2` and `_main.1.ok`, whole.
    const FWL2: &[u8] = include_bytes!("../testdata/valheim-1.0.17-probe.fwl2");
    const OK: &[u8] = include_bytes!("../testdata/valheim-1.0.17-probe.ok");

    /// A pre-1.0 `.fwl`, in the layout valheim-save-tools documents.
    fn legacy_fwl(version: i32, name: &str, seed_name: &str) -> Vec<u8> {
        let mut rest = Vec::new();
        rest.extend(version.to_le_bytes());
        for s in [name, seed_name] {
            rest.push(s.len() as u8);
            rest.extend(s.as_bytes());
        }
        rest.extend(1234i32.to_le_bytes()); // seed
        rest.extend(42i64.to_le_bytes()); // uid
        rest.extend(1i32.to_le_bytes()); // generator version
        if version >= 30 {
            rest.push(0); // needs a .db rebuild
            rest.extend(0i32.to_le_bytes()); // no world modifiers
        }
        let mut b = (rest.len() as i32).to_le_bytes().to_vec();
        b.extend(rest);
        b
    }

    /// The start of a `.db`: its version, then the world.
    fn legacy_db(version: i32) -> Vec<u8> {
        let mut b = version.to_le_bytes().to_vec();
        b.extend([0u8; 12]);
        b
    }

    /// A 1.0 world folder as server 1.0.17 left it, under `dir`.
    fn folder(dir: &str) -> Vec<(String, u64)> {
        [
            ("_main.1.fwl2", 50),
            ("_main.1.db2", 144_310),
            ("_main.1.chunks", 21),
            ("_main.1.ok", 4),
            ("00_00__0_1.chunk", 1464),
        ]
        .iter()
        .map(|(f, s)| (join(dir, f), *s))
        .collect()
    }

    fn source(files: &[(String, u64)]) -> Source {
        Source {
            entries: files
                .iter()
                .map(|(p, s)| Entry {
                    path: p.clone(),
                    size: *s,
                })
                .collect(),
            ..Source::default()
        }
    }

    fn files(paths: &[&str]) -> Vec<(String, u64)> {
        paths.iter().map(|p| (p.to_string(), 1000)).collect()
    }

    /// Answer every Read: a `.fwl2` with the real one, an `.ok` with the
    /// real one, a `.fwl` with a legacy one named after its file, a `.db`
    /// with a legacy start. Records every path read.
    fn run(mut src: Source, ctx: &Context, read: &mut Vec<String>) -> Step {
        for _ in 0..super::super::MAX_ROUNDS {
            match plan(&IMPORTER, &src, ctx) {
                Step::Read(reads) => {
                    for r in reads {
                        let lower = r.path.to_ascii_lowercase();
                        let body = if lower.ends_with(".fwl2") {
                            FWL2.to_vec()
                        } else if lower.ends_with(".ok") {
                            OK.to_vec()
                        } else if lower.ends_with(".fwl") {
                            // Valheim names a backup's world as the world.
                            let stem = file_name(&r.path).trim_end_matches(".fwl");
                            let name = stem.split("_backup").next().unwrap_or(stem);
                            legacy_fwl(34, name, "SeedSeed")
                        } else {
                            legacy_db(34)
                        };
                        read.push(r.path.clone());
                        src.read.insert(r.path, body);
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

    fn refusal(step: Step) -> Refusal {
        match step {
            Step::Refuse(r) => r,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_real_fwl2_reads_as_the_server_wrote_it() {
        let m = metadata(FWL2).unwrap();
        assert_eq!(m.version, 41);
        assert_eq!(m.name, "Probe");
        assert_eq!(m.seed_name, "Bq4LZbE9SR");
        assert_eq!(leading_version(OK), Some(Ok(41)));
        assert_eq!(metadata(&FWL2[..12]), Err(MetaError::Truncated));
        assert_eq!(metadata(b"PK\x03\x04 a zip"), Err(MetaError::NotAWorld));
        let m = metadata(&legacy_fwl(26, "Legacy", "abc")).unwrap();
        assert_eq!((m.version, m.name.as_str(), m.seed), (26, "Legacy", 1234));
    }

    #[test]
    fn a_saved_1_0_folder_lands_under_its_name_with_every_chunk() {
        let mut paths = folder("IronGate/Valheim/worlds_local/Probe");
        // An older generation the server had not cleaned up yet.
        paths.push((
            "IronGate/Valheim/worlds_local/Probe/_main.0.fwl2".into(),
            50,
        ));
        let mut read = Vec::new();
        let p = the_plan(run(source(&paths), &Context::default(), &mut read));
        let to: Vec<&str> = p.copy.iter().map(|c| c.to.as_str()).collect();
        assert_eq!(
            to,
            [
                "save/worlds_local/Probe/_main.1.fwl2",
                "save/worlds_local/Probe/_main.1.db2",
                "save/worlds_local/Probe/_main.1.chunks",
                "save/worlds_local/Probe/_main.1.ok",
                "save/worlds_local/Probe/00_00__0_1.chunk",
            ]
        );
        assert_eq!(p.settings["worldName"], "Probe");
        assert_eq!(
            p.present,
            ["save/worlds_local/Probe", "save/worlds_local/Probe.fwl"]
        );
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(read.len(), 2, "one round, two small files: {read:?}");
    }

    #[test]
    fn a_folder_picked_directly_is_named_by_its_metadata() {
        let mut src = source(&folder(""));
        src.root_name = Some("download (2)".into());
        let p = the_plan(run(src, &Context::default(), &mut Vec::new()));
        assert_eq!(p.settings["worldName"], "Probe");
        assert!(p
            .copy
            .iter()
            .all(|c| c.to.starts_with("save/worlds_local/Probe/")));
    }

    #[test]
    fn a_legacy_pair_is_placed_beside_the_worlds_and_warned_about() {
        let src = source(&files(&["worlds/Legacy.fwl", "worlds/Legacy.db"]));
        let p = the_plan(run(src, &Context::default(), &mut Vec::new()));
        let to: Vec<&str> = p.copy.iter().map(|c| c.to.as_str()).collect();
        assert_eq!(
            to,
            [
                "save/worlds_local/Legacy.fwl",
                "save/worlds_local/Legacy.db"
            ]
        );
        assert_eq!(p.settings["worldName"], "Legacy");
        assert!(p.warnings[0].contains("several minutes"));
        assert_eq!(p.world.unwrap().facts[0].value, "From before Valheim 1.0");
    }

    #[test]
    fn a_backup_is_offered_only_when_its_world_is_not_here_and_lands_under_its_name() {
        // The case a player is left with after 1.0: the pair moved aside as
        // .old, a dated backup, and nothing else.
        let alone = files(&[
            "worlds/Legacy.fwl.old",
            "worlds/Legacy.db.old",
            "worlds/Legacy_backup_20241227-231725.fwl",
            "worlds/Legacy_backup_20241227-231725.db",
            "worlds/Legacy_backup_auto-20241228000000.fwl",
            "worlds/Legacy_backup_auto-20241228000000.db",
        ]);
        let p = the_plan(run(source(&alone), &Context::default(), &mut Vec::new()));
        assert_eq!(p.settings["worldName"], "Legacy");
        assert_eq!(p.copy[0].from, "worlds/Legacy_backup_20241227-231725.fwl");
        assert_eq!(p.copy[0].to, "save/worlds_local/Legacy.fwl");
        assert!(p.warnings.iter().any(|w| w.contains("2024-12-27")));

        // With the world itself here, the backup is noise.
        let mut with_world = alone.clone();
        with_world.extend(files(&["worlds/Legacy.fwl", "worlds/Legacy.db"]));
        let p = the_plan(run(
            source(&with_world),
            &Context::default(),
            &mut Vec::new(),
        ));
        assert_eq!(p.copy[0].from, "worlds/Legacy.fwl");
        // A converted 1.0 folder of the same name hides it too.
        let mut converted = alone.clone();
        converted.extend(folder("worlds_local/Legacy"));
        let p = the_plan(run(
            source(&converted),
            &Context::default(),
            &mut Vec::new(),
        ));
        assert!(p.copy[0].from.starts_with("worlds_local/Legacy/"));

        // A suffix that is not Valheim's date keeps the whole name.
        let odd = files(&["Legacy_backup_mine.fwl", "Legacy_backup_mine.db"]);
        let p = the_plan(run(source(&odd), &Context::default(), &mut Vec::new()));
        assert_eq!(p.settings["worldName"], "Legacy_backup_mine");
    }

    #[test]
    fn several_worlds_are_offered_largest_first_then_one_is_imported() {
        let mut paths = folder("worlds_local/Probe");
        paths.extend([
            ("worlds/Small.fwl".to_string(), 40),
            ("worlds/Small.db".to_string(), 10),
            ("worlds/Old_backup_20240101-120000.fwl".to_string(), 40),
            (
                "worlds/Old_backup_20240101-120000.db".to_string(),
                9_000_000,
            ),
        ]);
        let Step::Choose(c) = run(source(&paths), &Context::default(), &mut Vec::new()) else {
            panic!()
        };
        let labels: Vec<&str> = c.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, ["Old", "Probe", "Small"]);
        let detail = c[0].detail.as_deref().unwrap();
        assert!(detail.starts_with("Before 1.0 · Backup from 2024-01-01 · Seed SeedSeed"));
        assert!(c[1]
            .detail
            .as_deref()
            .unwrap()
            .starts_with("Valheim 1.0 · Seed Bq4LZbE9SR"));
        let chosen = Context {
            runtime_version: None,
            world: Some("worlds/Small.fwl"),
        };
        let p = the_plan(run(source(&paths), &chosen, &mut Vec::new()));
        assert_eq!(p.settings["worldName"], "Small");
    }

    /// The rule that matters most here.
    #[test]
    fn characters_permission_lists_and_caches_are_never_read_or_copied() {
        let mut paths = folder("Valheim/worlds_local/Probe");
        paths.extend(files(&[
            "Valheim/characters_local/Viking.fch",
            "Valheim/characters/Viking.fch.old",
            "Valheim/worlds_local/Probe/Viking.fch",
            "Valheim/worlds_local/Probe/x.chunk.fch",
            "Valheim/adminlist.txt",
            "Valheim/bannedlist.txt",
            "Valheim/permittedlist.txt",
            "Valheim/prefs",
            "Valheim/logs/x.chunk",
            "Valheim/worlds_local/Probe/logs/0_0.chunk",
            "Valheim/worlds_local/Probe/server.log",
            "Valheim/worlds_local/Probe_heightTexCache",
            "Valheim/worlds_local/Probe_mapTexCache",
            "Valheim/worlds_local/Probe_forestMaskTexCache",
            "Valheim/worlds/steam_autocloud.vdf",
            "Valheim/characters/Thing.fwl",
            "Valheim/characters/Thing.db",
        ]));
        let mut read = Vec::new();
        let p = the_plan(run(source(&paths), &Context::default(), &mut read));
        for path in &read {
            assert!(!never_read(path), "{path} was read");
        }
        for c in &p.copy {
            assert!(!never_read(&c.from), "{} would be copied", c.from);
        }
        assert_eq!(p.copy.len(), 5);
        assert!(never_read("a/B.FCH") && never_read("x/AdminList.txt"));
    }

    #[test]
    fn what_is_not_a_world_is_refused_in_words() {
        let ctx = Context::default();
        let none = |paths: &[&str]| refusal(run(source(&files(paths)), &ctx, &mut Vec::new()));

        let r = none(&["characters_local/Viking.fch"]);
        assert_eq!(r.code, RefusalCode::NoWorld);
        assert!(r.message.contains("character"));

        let r = none(&["Probe/_main.0.fwl2"]);
        assert_eq!(r.code, RefusalCode::ReadFailed);
        assert!(r.message.contains("never saved"));

        let r = none(&["worlds/Legacy.fwl"]);
        assert_eq!(r.code, RefusalCode::ReadFailed);
        assert!(r.message.contains(".db"));

        let r = none(&["wgs/0123/containers.index", "wgs/0123/ABCD/EF01"]);
        assert_eq!(r.code, RefusalCode::UnsupportedWorld);

        let r = none(&["worlds/Legacy.fwl.old", "worlds/Legacy.db.old"]);
        assert_eq!(r.code, RefusalCode::NoWorld);
        assert!(r.message.contains(".fwl.old"));

        assert_eq!(none(&["readme.txt"]).code, RefusalCode::NoWorld);
    }

    #[test]
    fn a_generation_without_its_ok_is_not_a_world_and_a_bad_header_is_refused() {
        // The newest generation never finished; the one before it did.
        let mut paths = folder("w/Probe");
        paths.extend(files(&["w/Probe/_main.2.fwl2", "w/Probe/_main.2.db2"]));
        let p = the_plan(run(source(&paths), &Context::default(), &mut Vec::new()));
        assert!(p.copy.iter().all(|c| !c.from.contains("_main.2.")));

        let mut src = source(&files(&["Legacy.fwl", "Legacy.db"]));
        src.read
            .insert("Legacy.fwl".into(), b"not a world at all".to_vec());
        src.read.insert("Legacy.db".into(), legacy_db(26));
        assert_eq!(
            refusal(plan(&IMPORTER, &src, &Context::default())).code,
            RefusalCode::UnsupportedWorld
        );
        // A .db cut off before its version.
        src.read
            .insert("Legacy.fwl".into(), legacy_fwl(26, "Legacy", "s"));
        src.read.insert("Legacy.db".into(), vec![0x1a]);
        assert_eq!(
            refusal(plan(&IMPORTER, &src, &Context::default())).code,
            RefusalCode::ReadFailed
        );
    }

    #[test]
    fn a_world_from_a_newer_valheim_is_refused_when_the_version_is_known() {
        let mut src = source(&folder("Probe"));
        let mut newer = FWL2.to_vec();
        newer[4] = 42;
        src.read.insert("Probe/_main.1.fwl2".into(), newer);
        src.read.insert("Probe/_main.1.ok".into(), OK.to_vec());
        let on = |v| Context {
            runtime_version: v,
            world: None,
        };
        let r = refusal(plan(&IMPORTER, &src, &on(Some("1.0.17"))));
        assert_eq!(r.code, RefusalCode::TooNew);
        assert!(r.message.contains("1.0.17"));
        let p = the_plan(plan(&IMPORTER, &src, &on(None)));
        assert!(p.warnings[0].contains("newer version"));
    }

    #[test]
    fn a_name_the_server_cannot_be_given_is_refused() {
        let src = source(&files(&["-batchmode.fwl", "-batchmode.db"]));
        let r = refusal(run(src, &Context::default(), &mut Vec::new()));
        assert_eq!(r.code, RefusalCode::UnsupportedWorld);
    }
}

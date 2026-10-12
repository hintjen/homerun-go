//! Bringing a player's existing world into a new server — the decisions half.
//!
//! # What this is
//!
//! A player who already has a world (a Terraria `.wld`, a Palworld world
//! downloaded from another host) can start a new server with it instead of a
//! fresh one. What that takes is different for every game, so each game that
//! supports it has an **importer**: a function that looks at what the player
//! picked and says what to do with it. Most games have none, and a game
//! without one simply cannot import.
//!
//! The split is the engine's usual one. Everything here is pure: an importer
//! never opens a file. The runner (`homerun-game import`) lists the source,
//! reads the few bytes an importer asks for, and copies what the plan names;
//! the same runner applies the staged world at the server's first start.
//!
//! # How a conversation with an importer goes
//!
//! The runner hands over a [`Source`]: every file in what the player picked,
//! with its size, and the bytes of any file read so far. The importer answers
//! with a [`Step`]:
//!
//! | Step | Means | The runner |
//! |---|---|---|
//! | [`Step::Read`] | "show me the start of these files" | reads them and asks again |
//! | [`Step::Choose`] | several worlds, and nobody said which | reports them; the player picks one and the runner is run again with `--world` |
//! | [`Step::Plan`] | this world, these files, these settings | stages the files |
//! | [`Step::Refuse`] | it cannot be imported, in a sentence for a player | reports it |
//!
//! [`plan`] runs an importer and checks its answer, so a plan that names a
//! file outside the source, a path that would leave the server's folder, or
//! more than [`MAX_TOTAL_BYTES`] never reaches the runner.
//!
//! # What a plan holds
//!
//! - **`copy`**: files from the source, and where each lands relative to the
//!   server's folder.
//! - **`present`**: patterns that mean "this server already has a world". The
//!   runner imports nothing when one matches, which is how a backup restore
//!   wins over an import.
//! - **`ini`**: keys to set in an INI file after the copy, such as the one
//!   that tells Palworld which world folder to load.
//! - **`settings`**: values for the game's create-only settings that the world
//!   decides (Terraria's size). The host sends them with the new server; the
//!   runner never applies settings itself.
//! - **`world`** and **`warnings`**: what the player is shown.
//!
//! [`Staged`] is what the runner writes beside the staged files: the parts of
//! a plan the first start still needs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub mod palworld;
pub mod terraria;

/// The version of [`Staged`]. A runner refuses a staged world of another one.
pub const FORMAT: u32 = 1;

/// The feature a runner advertises when it has the import mechanism: the
/// `import` verb, the staged-world format and `start.import`.
pub const FEATURE: &str = "world-import";

/// The feature a runner advertises for each game it can import.
pub fn feature(game: &str) -> String {
    format!("world-import:{game}")
}

/// The most a world may unpack to. Large Palworld worlds run to a few hundred
/// megabytes; this is ten times that, and stops a zip that would fill a disk.
pub const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// How many [`Step::Read`] rounds a runner allows before it gives up. Every
/// importer here needs at most two.
pub const MAX_ROUNDS: usize = 4;

/// One file in what the player picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Relative to the source's root, `/`-separated.
    pub path: String,
    pub size: u64,
}

/// What the player picked, as far as the runner has read it.
#[derive(Debug, Clone, Default)]
pub struct Source {
    pub entries: Vec<Entry>,
    /// The start of each file the importer has asked to see, by path.
    pub read: BTreeMap<String, Vec<u8>>,
    /// The name of the folder or zip the player picked, without extension.
    /// A world folder picked directly has no other name to go by.
    pub root_name: Option<String>,
}

impl Source {
    pub fn entry(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.path == path)
    }

    pub fn has(&self, path: &str) -> bool {
        self.entry(path).is_some()
    }

    pub fn bytes(&self, path: &str) -> Option<&[u8]> {
        self.read.get(path).map(Vec::as_slice)
    }
}

/// What an importer is told besides the source.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context<'a> {
    /// The game's version the server will run, when the host knows it (a
    /// vendor runtime's chosen version). Lets an importer refuse a world from
    /// a newer version of the game.
    pub runtime_version: Option<&'a str>,
    /// The candidate the player chose, by [`Candidate::id`].
    pub world: Option<&'a str>,
}

/// "Show me the first `max_bytes` of this file."
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Read {
    pub path: String,
    pub max_bytes: u64,
}

/// One of several worlds in a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    /// What the runner is given back as `--world`.
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub size_bytes: u64,
}

/// One line of what a player is shown about a world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub label: String,
    pub value: String,
}

/// The world a plan imports, as a player is shown it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct World {
    pub name: String,
    pub facts: Vec<Fact>,
}

/// A source file and where it lands, relative to the server's folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyFile {
    pub from: String,
    pub to: String,
}

/// One key to set in an INI file after the copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IniEdit {
    /// Relative to the server's folder.
    pub file: String,
    pub section: String,
    pub key: String,
    pub value: String,
}

impl IniEdit {
    /// The key as [`crate::ini_config::merge`] names it.
    pub fn managed_key(&self) -> String {
        format!("[{}]{}", self.section, self.key)
    }
}

/// What to import, and what follows from it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub world: Option<World>,
    pub settings: Map<String, Value>,
    pub warnings: Vec<String>,
    pub copy: Vec<CopyFile>,
    pub present: Vec<String>,
    pub ini: Vec<IniEdit>,
}

/// Why a world cannot be imported. Spelled as the contract spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    NoImporter,
    NoWorld,
    UnsupportedWorld,
    TooNew,
    TooLarge,
    UnsafePath,
    ReadFailed,
}

/// A refusal, with a sentence for a player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: RefusalCode,
    pub message: String,
}

impl Refusal {
    pub fn new(code: RefusalCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// An importer's answer. See the module header.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Read(Vec<Read>),
    Choose(Vec<Candidate>),
    Plan(Plan),
    Refuse(Refusal),
}

/// One game's importer.
#[derive(Debug, Clone, Copy)]
pub struct Importer {
    /// The descriptor `id` of the game it imports for.
    pub game: &'static str,
    pub step: fn(&Source, &Context) -> Step,
}

/// Every importer this build carries.
pub const IMPORTERS: &[Importer] = &[palworld::IMPORTER, terraria::IMPORTER];

/// The importer for the game whose descriptor `id` is `game`.
pub fn importer(game: &str) -> Option<&'static Importer> {
    IMPORTERS.iter().find(|i| i.game == game)
}

/// What the runner writes beside the staged files as `import.json`: the
/// parts of a plan the first start still needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub format: u32,
    pub game: String,
    pub world: Option<World>,
    pub present: Vec<String>,
    pub ini: Vec<IniEdit>,
}

impl Staged {
    pub fn new(game: &str, plan: &Plan) -> Self {
        Self {
            format: FORMAT,
            game: game.to_string(),
            world: plan.world.clone(),
            present: plan.present.clone(),
            ini: plan.ini.clone(),
        }
    }

    /// Whether a runner may apply this to a server of `game`.
    pub fn check(&self, game: &str) -> Result<(), String> {
        if self.format != FORMAT {
            return Err(
                "This world was prepared by a different version of Homerun. Import it again."
                    .into(),
            );
        }
        if self.game != game {
            return Err("This world was prepared for a different game.".into());
        }
        if self.present.iter().any(|p| !safe_pattern(p)) || self.ini.iter().any(|e| !safe_ini(e)) {
            return Err("This world's import instructions cannot be used.".into());
        }
        Ok(())
    }
}

/// Run `importer` on `source` and check its answer.
///
/// A plan comes back only if every file it copies is in the source, every
/// destination stays inside the server's folder, its INI edits are writable,
/// and the copy fits under [`MAX_TOTAL_BYTES`].
pub fn plan(importer: &Importer, source: &Source, ctx: &Context) -> Step {
    match (importer.step)(source, ctx) {
        Step::Plan(plan) => match check(&plan, source) {
            Ok(()) => Step::Plan(plan),
            Err(refusal) => Step::Refuse(refusal),
        },
        other => other,
    }
}

fn check(plan: &Plan, source: &Source) -> Result<(), Refusal> {
    let unsafe_path = || {
        Refusal::new(
            RefusalCode::UnsafePath,
            "This world has a file Homerun cannot place safely inside the server's folder.",
        )
    };
    if plan.copy.is_empty() {
        return Err(Refusal::new(
            RefusalCode::NoWorld,
            "There is no world in what you picked.",
        ));
    }
    let mut total: u64 = 0;
    let mut seen = std::collections::BTreeSet::new();
    for file in &plan.copy {
        let entry = source.entry(&file.from).ok_or_else(unsafe_path)?;
        if !safe_relative(&file.to) || !seen.insert(file.to.to_ascii_lowercase()) {
            return Err(unsafe_path());
        }
        total = total.saturating_add(entry.size);
    }
    if total > MAX_TOTAL_BYTES {
        return Err(Refusal::new(
            RefusalCode::TooLarge,
            "This world is larger than Homerun can import (4 GB).",
        ));
    }
    if plan.present.is_empty() || plan.present.iter().any(|p| !safe_pattern(p)) {
        return Err(unsafe_path());
    }
    if plan.ini.iter().any(|e| !safe_ini(e)) {
        return Err(unsafe_path());
    }
    Ok(())
}

/// Whether `path` is a plain relative path that stays where it is joined:
/// `/`-separated, no empty, `.` or `..` part, no drive, no `\`, no control
/// characters, and no Windows device name or trailing dot or space, which
/// Windows would quietly rewrite.
pub fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(safe_part)
}

fn safe_part(part: &str) -> bool {
    if part.is_empty() || part == "." || part == ".." || part.ends_with(['.', ' ']) {
        return false;
    }
    if part.contains(['<', '>', '"', '|', '?', '*']) {
        return false;
    }
    let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    !device
}

/// A `present` pattern: a safe relative path whose parts may each be `*`.
fn safe_pattern(pattern: &str) -> bool {
    safe_relative(&pattern.replace('*', "x"))
        && pattern
            .split('/')
            .all(|part| part == "*" || !part.contains('*'))
}

fn safe_ini(edit: &IniEdit) -> bool {
    let line = |s: &str| !s.chars().any(char::is_control);
    safe_relative(&edit.file)
        && !edit.section.is_empty()
        && line(&edit.section)
        && !edit.section.contains(']')
        && !edit.key.is_empty()
        && line(&edit.key)
        && !edit.key.contains(['=', '(', ')', '[', ']'])
        && line(&edit.value)
}

/// Whether `path` (relative, `/`-separated) matches a `present` pattern,
/// where a `*` part matches any one part.
pub fn matches(pattern: &str, path: &str) -> bool {
    let (p, q): (Vec<_>, Vec<_>) = (pattern.split('/').collect(), path.split('/').collect());
    p.len() == q.len()
        && p.iter()
            .zip(&q)
            .all(|(a, b)| *a == "*" || a.eq_ignore_ascii_case(b))
}

/// The file-name part of a `/`-separated path.
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The folder part of a `/`-separated path, `""` at the root.
pub(crate) fn parent(path: &str) -> &str {
    path.rfind('/').map(|i| &path[..i]).unwrap_or("")
}

/// `prefix/rest`, or `rest` at the root.
pub(crate) fn join(prefix: &str, rest: &str) -> String {
    if prefix.is_empty() {
        rest.to_string()
    } else {
        format!("{prefix}/{rest}")
    }
}

/// A size as a player reads it.
pub(crate) fn human_size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if bytes < MB {
        format!("{} KB", bytes.div_ceil(1024).max(1))
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(paths: &[(&str, u64)]) -> Source {
        Source {
            entries: paths
                .iter()
                .map(|(p, s)| Entry {
                    path: p.to_string(),
                    size: *s,
                })
                .collect(),
            ..Source::default()
        }
    }

    fn plan_of(copy: &[(&str, &str)]) -> Plan {
        Plan {
            copy: copy
                .iter()
                .map(|(f, t)| CopyFile {
                    from: f.to_string(),
                    to: t.to_string(),
                })
                .collect(),
            present: vec!["data/*/world.wld".into()],
            ..Plan::default()
        }
    }

    #[test]
    fn a_path_that_would_leave_the_folder_is_not_safe() {
        for bad in [
            "",
            "/abs",
            "a/../b",
            "..",
            "a//b",
            "./a",
            "C:/x",
            "a\\b",
            "a/b.",
            "a/b ",
            "CON",
            "nul.txt",
            "COM1/x",
            "a/LPT9.log",
            "a?b",
            "a\nb",
        ] {
            assert!(!safe_relative(bad), "{bad:?} should be refused");
        }
        for good in [
            "a",
            "Saved/SaveGames/0/X/Level.sav",
            "data/Worlds/world.wld.bak",
            "COM10",
            "console.log",
        ] {
            assert!(safe_relative(good), "{good:?} should be allowed");
        }
    }

    #[test]
    fn a_plan_is_checked_before_a_runner_sees_it() {
        let src = source(&[("w.wld", 10)]);
        assert_eq!(check(&plan_of(&[("w.wld", "data/w.wld")]), &src), Ok(()));
        // A file the source does not have, or a destination outside.
        for copy in [[("other.wld", "data/w.wld")], [("w.wld", "../w.wld")]] {
            let refused = check(&plan_of(&copy), &src).unwrap_err();
            assert_eq!(refused.code, RefusalCode::UnsafePath);
        }
        // Two files onto one destination, even differing only in case.
        let two = source(&[("a", 1), ("b", 1)]);
        assert!(check(&plan_of(&[("a", "x/y"), ("b", "X/Y")]), &two).is_err());
        // Nothing to copy at all.
        assert_eq!(
            check(&plan_of(&[]), &src).unwrap_err().code,
            RefusalCode::NoWorld
        );
    }

    #[test]
    fn a_world_too_large_is_refused_by_size_not_by_reading_it() {
        let src = source(&[("a", MAX_TOTAL_BYTES), ("b", 1)]);
        let refused = check(&plan_of(&[("a", "a"), ("b", "b")]), &src).unwrap_err();
        assert_eq!(refused.code, RefusalCode::TooLarge);
    }

    #[test]
    fn an_ini_edit_cannot_add_a_line_or_a_section() {
        let edit = |section: &str, key: &str, value: &str| IniEdit {
            file: "Saved/x.ini".into(),
            section: section.into(),
            key: key.into(),
            value: value.into(),
        };
        assert!(safe_ini(&edit(
            "/Script/Pal.PalGameLocalSettings",
            "DedicatedServerName",
            "ABC"
        )));
        assert!(!safe_ini(&edit("S", "K", "a\nEvil=1")));
        assert!(!safe_ini(&edit("S]x", "K", "v")));
        assert!(!safe_ini(&edit("S", "K=1", "v")));
        assert!(!safe_ini(&edit("S", "K(M)", "v")));
    }

    #[test]
    fn present_patterns_match_one_part_per_star() {
        assert!(matches(
            "Saved/SaveGames/0/*/Level.sav",
            "Saved/SaveGames/0/80F5/Level.sav"
        ));
        assert!(!matches(
            "Saved/SaveGames/0/*/Level.sav",
            "Saved/SaveGames/0/a/b/Level.sav"
        ));
        assert!(matches("data/Worlds/world.wld", "data/worlds/WORLD.wld"));
        assert!(safe_pattern("Saved/SaveGames/0/*/Level.sav"));
        assert!(!safe_pattern("Saved/*x/Level.sav"));
        assert!(!safe_pattern("../*"));
    }

    #[test]
    fn a_staged_world_is_only_applied_to_its_own_game_and_format() {
        let staged = Staged::new("terraria", &plan_of(&[("w.wld", "data/w.wld")]));
        assert_eq!(staged.check("terraria"), Ok(()));
        assert!(staged.check("palworld").is_err());
        let older = Staged {
            format: 0,
            ..staged.clone()
        };
        assert!(older.check("terraria").is_err());
        let json = serde_json::to_value(&staged).unwrap();
        assert_eq!(json["format"], 1);
        assert_eq!(json["game"], "terraria");
    }

    #[test]
    fn each_importer_has_a_feature_and_a_unique_game() {
        let mut games: Vec<_> = IMPORTERS.iter().map(|i| i.game).collect();
        games.sort();
        games.dedup();
        assert_eq!(games.len(), IMPORTERS.len());
        assert_eq!(feature("palworld"), "world-import:palworld");
        assert!(importer("terraria").is_some());
        assert!(importer("rust").is_none());
    }
}

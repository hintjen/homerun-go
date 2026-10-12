//! Terraria: a `.wld` file is the whole world.
//!
//! Characters are `.plr` files on each player's own computer and never move,
//! so importing a world is placing one file where the server's launch line
//! looks (`data/Worlds/world.wld`, fixed by the descriptor's `-world`). What
//! this importer adds is reading the world's header, so the new server's
//! create-only settings describe the world it actually runs, and refusing the
//! worlds a server cannot open.
//!
//! # The header
//!
//! A world file starts with its version, the `relogic` signature, a file type
//! (2 is a world), a table of section offsets, and then the header section:
//! name, seed, generator version, GUID, id, bounds, height and width in tiles,
//! and the game mode. Everything this reads is in the first few hundred bytes.
//!
//! # What is refused
//!
//! - **A world from a newer Terraria.** The server stops with "File is from a
//!   later version of Terraria" and, worse, exits 0, so a host would see a
//!   clean stop. Checked against [`RELEASES`] when the host names the version
//!   the server will run.
//! - **A tModLoader world** (a `.twld` beside it), which a vanilla server
//!   cannot run.
//! - **A custom size**, which the size setting cannot describe.
//!
//! Observed 2026-10-12 on 1.4.5.8: loading a medium world with `-autocreate 1`
//! and saving it changed only the header's save counter, so a size mismatch
//! did not corrupt anything there. Matching the setting to the world is still
//! right, and costs nothing.

use serde_json::{json, Map};

use super::{
    file_name, human_size, Candidate, Context, CopyFile, Fact, Importer, Plan, Read, Refusal,
    RefusalCode, Source, Step, World,
};

pub const IMPORTER: Importer = Importer {
    game: "terraria",
    step,
};

/// Where the descriptor's launch line looks for the world.
const WORLD: &str = "data/Worlds/world.wld";

/// How much of a world file to read for its header. The header section starts
/// a few hundred bytes in (167 in a 1.4.5.8 world).
const HEADER_BYTES: u64 = 64 * 1024;

/// The most worlds one source may offer to choose from.
const MAX_CANDIDATES: usize = 32;

/// The world-file version each server release writes, observed by generating
/// a world with each one (2026-10-12).
pub const RELEASES: &[(&str, i32)] = &[
    ("1.4.5.0", 315),
    ("1.4.5.1", 315),
    ("1.4.5.6", 319),
    ("1.4.5.8", 326),
];

/// The oldest world version this reads: the first with the `relogic`
/// signature (Terraria 1.3).
const OLDEST: i32 = 135;

/// What the importer needs from a world's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: i32,
    pub name: String,
    pub seed: Option<String>,
    pub width: i32,
    pub height: i32,
    /// 0 Classic, 1 Expert, 2 Master, 3 Journey.
    pub mode: i32,
}

/// Why a header could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// The bytes end before the header does.
    Truncated,
    /// Not a Terraria world, or one older than [`OLDEST`].
    NotAWorld,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], HeaderError> {
        let end = self.at.checked_add(n).ok_or(HeaderError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(HeaderError::Truncated)?;
        self.at = end;
        Ok(slice)
    }
    fn i16(&mut self) -> Result<i16, HeaderError> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, HeaderError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u8(&mut self) -> Result<u8, HeaderError> {
        Ok(self.take(1)?[0])
    }
    /// A .NET `BinaryWriter` string: a 7-bit length, then UTF-8.
    fn string(&mut self) -> Result<String, HeaderError> {
        let (mut len, mut shift) = (0usize, 0);
        loop {
            let b = self.u8()?;
            len |= ((b & 0x7F) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 28 {
                return Err(HeaderError::NotAWorld);
            }
        }
        if len > 4096 {
            return Err(HeaderError::NotAWorld);
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| HeaderError::NotAWorld)
    }
}

/// Read a world's header from the start of its file.
pub fn header(bytes: &[u8]) -> Result<Header, HeaderError> {
    let mut r = Reader { bytes, at: 0 };
    let version = r.i32()?;
    if !(OLDEST..=10_000).contains(&version) {
        return Err(HeaderError::NotAWorld);
    }
    if r.take(7)? != b"relogic" || r.u8()? != 2 {
        return Err(HeaderError::NotAWorld);
    }
    r.take(4 + 8)?; // revision, favourite flags
    let sections = r.i16()?;
    if !(1..=64).contains(&sections) {
        return Err(HeaderError::NotAWorld);
    }
    let first = r.i32()?;
    let first = usize::try_from(first).map_err(|_| HeaderError::NotAWorld)?;
    if first < r.at {
        return Err(HeaderError::NotAWorld);
    }
    r.at = first;
    let name = r.string()?;
    let seed = if version >= 179 {
        let seed = r.string()?;
        r.take(8)?; // generator version
        Some(seed)
    } else {
        None
    };
    if version >= 181 {
        r.take(16)?; // GUID
    }
    r.take(4 + 16)?; // id, then left, right, top, bottom
    let height = r.i32()?;
    let width = r.i32()?;
    let mode = if version >= 209 {
        r.i32()?
    } else if version >= 112 {
        i32::from(r.u8()? != 0)
    } else {
        0
    };
    Ok(Header {
        version,
        name,
        seed,
        width,
        height,
        mode,
    })
}

/// The size setting for a world this wide, if it is a standard size.
pub fn size_setting(width: i32) -> Option<(&'static str, &'static str)> {
    match width {
        4200 => Some(("1", "Small")),
        6400 => Some(("2", "Medium")),
        8400 => Some(("3", "Large")),
        _ => None,
    }
}

fn mode_name(mode: i32) -> Option<&'static str> {
    ["Classic", "Expert", "Master", "Journey"]
        .get(usize::try_from(mode).ok()?)
        .copied()
}

fn is_world(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".wld")
}

/// The tModLoader data file that sits beside a modded world.
fn tmod_sibling(source: &Source, world: &str) -> bool {
    let stem = &world[..world.len() - ".wld".len()];
    source
        .entries
        .iter()
        .any(|e| e.path.eq_ignore_ascii_case(&format!("{stem}.twld")))
}

fn step(source: &Source, ctx: &Context) -> Step {
    let mut worlds: Vec<&str> = source
        .entries
        .iter()
        .filter(|e| is_world(&e.path))
        .map(|e| e.path.as_str())
        .collect();
    worlds.sort_unstable();
    if let Some(chosen) = ctx.world {
        worlds.retain(|w| *w == chosen);
    }
    if worlds.is_empty() {
        return Step::Refuse(Refusal::new(
            RefusalCode::NoWorld,
            "There is no Terraria world (.wld file) in what you picked.",
        ));
    }
    worlds.truncate(MAX_CANDIDATES);

    let unread: Vec<Read> = worlds
        .iter()
        .filter(|w| source.bytes(w).is_none())
        .map(|w| Read {
            path: w.to_string(),
            max_bytes: HEADER_BYTES,
        })
        .collect();
    if !unread.is_empty() {
        return Step::Read(unread);
    }

    if worlds.len() > 1 {
        let candidates = worlds
            .iter()
            .map(|w| {
                let size = source.entry(w).map(|e| e.size).unwrap_or_default();
                match source.bytes(w).map(header) {
                    Some(Ok(h)) => Candidate {
                        id: w.to_string(),
                        label: if h.name.is_empty() {
                            file_name(w).to_string()
                        } else {
                            h.name.clone()
                        },
                        detail: Some(
                            [size_setting(h.width).map(|s| s.1), mode_name(h.mode)]
                                .into_iter()
                                .flatten()
                                .collect::<Vec<_>>()
                                .join(" · "),
                        ),
                        size_bytes: size,
                    },
                    _ => Candidate {
                        id: w.to_string(),
                        label: file_name(w).to_string(),
                        detail: Some("Not a world Homerun can read".into()),
                        size_bytes: size,
                    },
                }
            })
            .collect();
        return Step::Choose(candidates);
    }

    let world = worlds[0];
    plan_one(source, ctx, world)
}

fn plan_one(source: &Source, ctx: &Context, world: &str) -> Step {
    let refuse = |code, message: &str| Step::Refuse(Refusal::new(code, message));
    if tmod_sibling(source, world) {
        return refuse(
            RefusalCode::UnsupportedWorld,
            "This is a tModLoader world. Homerun runs Terraria without mods, so it can't host it.",
        );
    }
    let h = match header(source.bytes(world).unwrap_or_default()) {
        Ok(h) => h,
        Err(HeaderError::Truncated) => {
            return refuse(
                RefusalCode::ReadFailed,
                "This world file is incomplete. Copy it again and retry.",
            )
        }
        Err(HeaderError::NotAWorld) => {
            return refuse(
                RefusalCode::UnsupportedWorld,
                "This file isn't a Terraria world Homerun can read.",
            )
        }
    };
    let Some((size, size_name)) = size_setting(h.width) else {
        return refuse(
            RefusalCode::UnsupportedWorld,
            "This world has a custom size. Homerun can host small, medium and large worlds.",
        );
    };
    let Some(mode) = mode_name(h.mode) else {
        return refuse(
            RefusalCode::UnsupportedWorld,
            "This world uses a game mode Homerun doesn't know.",
        );
    };

    let mut warnings = Vec::new();
    let newest = RELEASES.iter().map(|r| r.1).max().unwrap_or_default();
    match ctx
        .runtime_version
        .and_then(|v| RELEASES.iter().find(|r| r.0 == v))
    {
        Some((release, max)) if h.version > *max => {
            return Step::Refuse(Refusal::new(
                RefusalCode::TooNew,
                format!(
                    "This world was saved by a newer version of Terraria than this server runs ({release}). Choose a newer server version, or import an older copy of the world."
                ),
            ))
        }
        Some(_) => {}
        None if h.version > newest => warnings.push(
            "This world was saved by a newer version of Terraria than Homerun has tested. If the server won't start, choose the newest server version.".into(),
        ),
        None => {}
    }
    if h.mode == 3 {
        warnings.push("Only Journey characters can join a Journey world.".into());
    }

    let mut copy = vec![CopyFile {
        from: world.to_string(),
        to: WORLD.to_string(),
    }];
    let backup = format!("{world}.bak");
    if let Some(entry) = source
        .entries
        .iter()
        .find(|e| e.path.eq_ignore_ascii_case(&backup))
    {
        copy.push(CopyFile {
            from: entry.path.clone(),
            to: format!("{WORLD}.bak"),
        });
    }

    let mut settings = Map::new();
    settings.insert("worldSize".into(), json!(size));
    settings.insert("difficulty".into(), json!(h.mode.to_string()));
    if !h.name.is_empty() && !h.name.chars().any(char::is_control) {
        settings.insert("worldName".into(), json!(h.name));
    }
    let mut facts = vec![
        Fact {
            label: "Size".into(),
            value: size_name.into(),
        },
        Fact {
            label: "Mode".into(),
            value: mode.into(),
        },
    ];
    if let Some(seed) = h.seed.as_deref().filter(|s| !s.is_empty()) {
        facts.push(Fact {
            label: "Seed".into(),
            value: seed.into(),
        });
    }
    if let Some(entry) = source.entry(world) {
        facts.push(Fact {
            label: "File".into(),
            value: human_size(entry.size),
        });
    }
    Step::Plan(Plan {
        world: Some(World {
            name: if h.name.is_empty() {
                file_name(world).to_string()
            } else {
                h.name
            },
            facts,
        }),
        settings,
        warnings,
        copy,
        present: vec![WORLD.into()],
        ini: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{plan, Entry};
    use super::*;

    /// The first 512 bytes of a small world a 1.4.5.8 server generated.
    const REAL: &[u8] = include_bytes!("../testdata/terraria-1.4.5.8-small.wld.head");

    /// A header with the fields this reads, in the layout `version` uses.
    fn synthetic(version: i32, name: &str, width: i32, mode: i32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(version.to_le_bytes());
        b.extend(b"relogic");
        b.push(2);
        b.extend([0u8; 12]);
        b.extend(1i16.to_le_bytes());
        let first = (b.len() + 4) as i32;
        b.extend(first.to_le_bytes());
        b.push(name.len() as u8);
        b.extend(name.as_bytes());
        b.push(4);
        b.extend(b"1234");
        b.extend([0u8; 8 + 16 + 4 + 16]);
        b.extend(1200i32.to_le_bytes());
        b.extend(width.to_le_bytes());
        b.extend(mode.to_le_bytes());
        b
    }

    fn source(files: &[(&str, Option<Vec<u8>>)]) -> Source {
        let mut s = Source::default();
        for (path, bytes) in files {
            s.entries.push(Entry {
                path: path.to_string(),
                size: bytes.as_ref().map(|b| b.len() as u64).unwrap_or(1000),
            });
            if let Some(b) = bytes {
                s.read.insert(path.to_string(), b.clone());
            }
        }
        s
    }

    fn ctx(version: Option<&'static str>) -> Context<'static> {
        Context {
            runtime_version: version,
            world: None,
        }
    }

    #[test]
    fn a_real_world_header_reads_as_the_server_wrote_it() {
        let h = header(REAL).unwrap();
        assert_eq!(h.version, 326);
        assert_eq!(h.name, "ImportTest");
        assert_eq!((h.width, h.height, h.mode), (4200, 1200, 0));
        assert!(h.seed.unwrap().starts_with("pumpkinseason|"));
    }

    #[test]
    fn a_file_that_is_not_a_world_or_is_cut_short_is_told_apart() {
        assert_eq!(
            header(b"PK\x03\x04 not a world at all"),
            Err(HeaderError::NotAWorld)
        );
        assert_eq!(header(&REAL[..100]), Err(HeaderError::Truncated));
        let mut wrong_type = REAL.to_vec();
        wrong_type[11] = 1; // a player file, not a world
        assert_eq!(header(&wrong_type), Err(HeaderError::NotAWorld));
    }

    #[test]
    fn the_settings_come_from_the_world() {
        let src = source(&[("Worlds/Big.wld", Some(synthetic(326, "Big", 8400, 1)))]);
        let Step::Plan(p) = plan(&IMPORTER, &src, &ctx(Some("1.4.5.8"))) else {
            panic!("expected a plan");
        };
        assert_eq!(p.settings["worldSize"], "3");
        assert_eq!(p.settings["difficulty"], "1");
        assert_eq!(p.settings["worldName"], "Big");
        assert_eq!(p.copy[0].to, "data/Worlds/world.wld");
        assert_eq!(p.present, vec!["data/Worlds/world.wld".to_string()]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn the_header_is_asked_for_before_anything_is_decided() {
        let src = source(&[("a.wld", None)]);
        assert_eq!(
            plan(&IMPORTER, &src, &ctx(None)),
            Step::Read(vec![Read {
                path: "a.wld".into(),
                max_bytes: HEADER_BYTES
            }])
        );
    }

    #[test]
    fn its_backup_comes_along() {
        let w = synthetic(326, "W", 6400, 0);
        let src = source(&[
            ("W.wld", Some(w.clone())),
            ("W.wld.bak", None),
            ("W.wld.bak2", None),
        ]);
        let Step::Plan(p) = plan(&IMPORTER, &src, &ctx(None)) else {
            panic!()
        };
        let to: Vec<_> = p.copy.iter().map(|c| c.to.as_str()).collect();
        assert_eq!(to, ["data/Worlds/world.wld", "data/Worlds/world.wld.bak"]);
    }

    #[test]
    fn a_world_from_a_newer_terraria_is_refused_when_the_version_is_known() {
        let src = source(&[("w.wld", Some(synthetic(326, "W", 4200, 0)))]);
        let Step::Refuse(r) = plan(&IMPORTER, &src, &ctx(Some("1.4.5.6"))) else {
            panic!()
        };
        assert_eq!(r.code, RefusalCode::TooNew);
        assert!(r.message.contains("1.4.5.6"));
        // The same world on the server that wrote it.
        assert!(matches!(
            plan(&IMPORTER, &src, &ctx(Some("1.4.5.8"))),
            Step::Plan(_)
        ));
        // A version newer than any tested, with no server version named: a
        // warning, because a newer server may well open it.
        let newer = source(&[("w.wld", Some(synthetic(400, "W", 4200, 0)))]);
        let Step::Plan(p) = plan(&IMPORTER, &newer, &ctx(None)) else {
            panic!()
        };
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn worlds_it_cannot_host_are_refused_in_words() {
        let tmod = source(&[
            ("m.wld", Some(synthetic(326, "M", 4200, 0))),
            ("m.twld", None),
        ]);
        let custom = source(&[("c.wld", Some(synthetic(326, "C", 5000, 0)))]);
        for (src, code) in [
            (tmod, RefusalCode::UnsupportedWorld),
            (custom, RefusalCode::UnsupportedWorld),
        ] {
            let Step::Refuse(r) = plan(&IMPORTER, &src, &ctx(None)) else {
                panic!()
            };
            assert_eq!(r.code, code);
        }
        let none = source(&[("readme.txt", None)]);
        let Step::Refuse(r) = plan(&IMPORTER, &none, &ctx(None)) else {
            panic!()
        };
        assert_eq!(r.code, RefusalCode::NoWorld);
    }

    #[test]
    fn a_journey_world_says_who_can_join() {
        let src = source(&[("j.wld", Some(synthetic(326, "J", 4200, 3)))]);
        let Step::Plan(p) = plan(&IMPORTER, &src, &ctx(None)) else {
            panic!()
        };
        assert_eq!(p.settings["difficulty"], "3");
        assert!(p.warnings[0].contains("Journey"));
    }

    #[test]
    fn several_worlds_are_offered_by_name_then_one_is_imported() {
        let src = source(&[
            ("Worlds/A.wld", Some(synthetic(326, "Alpha", 4200, 0))),
            ("Worlds/B.wld", Some(synthetic(326, "Beta", 8400, 2))),
        ]);
        let Step::Choose(c) = plan(&IMPORTER, &src, &ctx(None)) else {
            panic!()
        };
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].label, "Beta");
        assert_eq!(c[1].detail.as_deref(), Some("Large · Master"));
        let chosen = Context {
            runtime_version: None,
            world: Some("Worlds/B.wld"),
        };
        let Step::Plan(p) = plan(&IMPORTER, &src, &chosen) else {
            panic!()
        };
        assert_eq!(p.settings["worldName"], "Beta");
    }
}

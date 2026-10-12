//! Reading what a player picked to import: a folder, a zip, or one file.
//!
//! The effects half of `homerun_core::engine::import`. The core decides which
//! files make the world and where they go; this lists the source, reads the
//! start of the files the importer asks about, and copies one file out. It
//! never writes into the source, never follows a symbolic link inside a
//! folder, and refuses a zip entry whose name would leave wherever it is
//! unpacked.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use homerun_core::engine::import::{Entry, Refusal, RefusalCode};

/// The most files a source may hold. A world is tens of files; this is a
/// folder picked by mistake, such as a whole drive.
pub const MAX_ENTRIES: usize = 100_000;

/// How deep a folder is walked. A Palworld download nests six deep.
const MAX_DEPTH: usize = 16;

/// A source, opened.
pub enum Opened {
    Folder(PathBuf),
    /// A zip, and each entry's index by its `/`-separated path.
    Zip {
        path: PathBuf,
        index: BTreeMap<String, usize>,
    },
    File(PathBuf),
}

/// A source's files, and how to reach them.
pub struct Listing {
    pub opened: Opened,
    pub entries: Vec<Entry>,
    /// The folder's name, or the zip's without `.zip`.
    pub root_name: Option<String>,
}

fn unreadable() -> Refusal {
    Refusal::new(
        RefusalCode::ReadFailed,
        "Homerun couldn't read what you picked. Check it's still there and try again.",
    )
}

fn too_many() -> Refusal {
    Refusal::new(RefusalCode::TooLarge, "What you picked holds too many files to be one world. Pick the world's folder, or a zip of it.")
}

fn unsafe_name() -> Refusal {
    Refusal::new(RefusalCode::UnsafePath, "The zip has a file whose name would place it outside the world, so Homerun hasn't used it.")
}

/// List a source.
pub fn open(source: &Path) -> Result<Listing, Refusal> {
    let meta = fs::metadata(source).map_err(|_| unreadable())?;
    let name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_string);
    if meta.is_dir() {
        let mut entries = Vec::new();
        walk(source, "", 0, &mut entries)?;
        return Ok(Listing {
            opened: Opened::Folder(source.to_path_buf()),
            entries,
            root_name: name(source),
        });
    }
    if is_zip(source) {
        let (entries, index) = list_zip(source)?;
        return Ok(Listing {
            opened: Opened::Zip {
                path: source.to_path_buf(),
                index,
            },
            entries,
            root_name: source
                .file_stem()
                .and_then(|n| n.to_str())
                .map(str::to_string),
        });
    }
    let file = name(source).ok_or_else(unreadable)?;
    Ok(Listing {
        opened: Opened::File(source.to_path_buf()),
        entries: vec![Entry {
            path: file,
            size: meta.len(),
        }],
        root_name: None,
    })
}

/// A zip by its signature, whatever it is called.
fn is_zip(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && (magic == *b"PK\x03\x04" || magic == *b"PK\x05\x06")
}

fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<Entry>) -> Result<(), Refusal> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let mut children: Vec<_> = fs::read_dir(dir)
        .map_err(|_| unreadable())?
        .filter_map(Result::ok)
        .collect();
    children.sort_by_key(|c| c.file_name());
    for child in children {
        let Some(name) = child.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // `symlink_metadata`, so a link is seen as a link and skipped: a
        // folder that links to the rest of the disk is not part of a world.
        let Ok(meta) = fs::symlink_metadata(child.path()) else {
            continue;
        };
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk(&child.path(), &path, depth + 1, out)?;
        } else if meta.is_file() {
            if out.len() >= MAX_ENTRIES {
                return Err(too_many());
            }
            out.push(Entry {
                path,
                size: meta.len(),
            });
        }
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn list_zip(path: &Path) -> Result<(Vec<Entry>, BTreeMap<String, usize>), Refusal> {
    let file = fs::File::open(path).map_err(|_| unreadable())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| {
        Refusal::new(
            RefusalCode::ReadFailed,
            "This zip can't be opened. It may be incomplete; download it again.",
        )
    })?;
    if zip.len() > MAX_ENTRIES {
        return Err(too_many());
    }
    let (mut entries, mut index) = (Vec::new(), BTreeMap::new());
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(|_| unreadable())?;
        if entry.is_dir() {
            continue;
        }
        // The library's own traversal check, then plain parts only: a name
        // like `a/../b` stays inside but is not one a world would have.
        let relative = entry.enclosed_name().ok_or_else(unsafe_name)?;
        let mut parts = Vec::new();
        for part in relative.components() {
            match part {
                Component::Normal(p) => parts.push(p.to_str().ok_or_else(unsafe_name)?.to_string()),
                _ => return Err(unsafe_name()),
            }
        }
        let path = parts.join("/");
        if index.insert(path.clone(), i).is_none() {
            entries.push(Entry {
                path,
                size: entry.size(),
            });
        }
    }
    Ok((entries, index))
}

fn folder_path(root: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(root.to_path_buf(), |p, part| p.join(part))
}

/// The first `max` bytes of one file of the source.
pub fn read_prefix(opened: &Opened, path: &str, max: u64) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    match opened {
        Opened::Folder(root) => {
            fs::File::open(folder_path(root, path))
                .map_err(|_| unreadable())?
                .take(max)
                .read_to_end(&mut out)
                .map_err(|_| unreadable())?;
        }
        Opened::File(file) => {
            fs::File::open(file)
                .map_err(|_| unreadable())?
                .take(max)
                .read_to_end(&mut out)
                .map_err(|_| unreadable())?;
        }
        Opened::Zip {
            path: zip_path,
            index,
        } => {
            let i = *index.get(path).ok_or_else(unreadable)?;
            let file = fs::File::open(zip_path).map_err(|_| unreadable())?;
            let mut zip = zip::ZipArchive::new(file).map_err(|_| unreadable())?;
            let entry = zip.by_index(i).map_err(|_| unreadable())?;
            entry
                .take(max)
                .read_to_end(&mut out)
                .map_err(|_| unreadable())?;
        }
    }
    Ok(out)
}

/// Copy one file of the source to `dest`, which must not exist yet.
/// `progress` hears each chunk's size. A zip entry is read to its end, so its
/// checksum is checked; a mismatch is a refusal and `dest` is removed.
pub fn copy_out(
    opened: &Opened,
    path: &str,
    dest: &Path,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, Refusal> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|_| cannot_stage())?;
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|_| cannot_stage())?;
    let result = match opened {
        Opened::Folder(root) => fs::File::open(folder_path(root, path))
            .map_err(|_| unreadable())
            .and_then(|mut f| pump(&mut f, &mut out, progress)),
        Opened::File(file) => fs::File::open(file)
            .map_err(|_| unreadable())
            .and_then(|mut f| pump(&mut f, &mut out, progress)),
        Opened::Zip {
            path: zip_path,
            index,
        } => (|| {
            let i = *index.get(path).ok_or_else(unreadable)?;
            let file = fs::File::open(zip_path).map_err(|_| unreadable())?;
            let mut zip = zip::ZipArchive::new(file).map_err(|_| unreadable())?;
            let mut entry = zip.by_index(i).map_err(|_| unreadable())?;
            pump(&mut entry, &mut out, progress).map_err(|e| {
                if e.code == RefusalCode::ReadFailed {
                    Refusal::new(RefusalCode::ReadFailed, "The zip is damaged: a file in it didn't match its checksum. Download it again.")
                } else {
                    e
                }
            })
        })(),
    };
    if result.is_err() {
        drop(out);
        let _ = fs::remove_file(dest);
    }
    result
}

fn cannot_stage() -> Refusal {
    Refusal::new(RefusalCode::ReadFailed, "Homerun couldn't save a copy of the world on this computer. Check there's free space and try again.")
}

fn pump(
    from: &mut impl Read,
    to: &mut impl Write,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, Refusal> {
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0;
    loop {
        let n = from.read(&mut buffer).map_err(|_| unreadable())?;
        if n == 0 {
            return Ok(total);
        }
        to.write_all(&buffer[..n]).map_err(|_| cannot_stage())?;
        total += n as u64;
        progress(n as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "homerun-world-import-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn zip_of(path: &Path, files: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (name, bytes) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn a_folder_is_listed_with_slashes_and_sizes() {
        let dir = scratch("folder");
        fs::create_dir_all(dir.join("W/Players")).unwrap();
        fs::write(dir.join("W/Level.sav"), b"level").unwrap();
        fs::write(dir.join("W/Players/p.sav"), b"pp").unwrap();
        let listing = open(&dir).unwrap();
        let paths: Vec<_> = listing
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.size))
            .collect();
        assert_eq!(paths, [("W/Level.sav", 5), ("W/Players/p.sav", 2)]);
        assert_eq!(
            read_prefix(&listing.opened, "W/Level.sav", 3).unwrap(),
            b"lev"
        );
        let dest = dir.join("out/Level.sav");
        let mut seen = 0;
        copy_out(&listing.opened, "W/Level.sav", &dest, &mut |n| seen += n).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"level");
        assert_eq!(seen, 5);
        // Never over a file that is already there.
        assert!(copy_out(&listing.opened, "W/Level.sav", &dest, &mut |_| ()).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"level");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zip_is_listed_by_its_signature_and_read_from_inside() {
        let dir = scratch("zip");
        let archive = dir.join("download.bin");
        zip_of(
            &archive,
            &[("Pal/Saved/a.sav", b"aaaa"), ("Pal/Saved/b.sav", b"b")],
        );
        let listing = open(&archive).unwrap();
        assert_eq!(listing.root_name.as_deref(), Some("download"));
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(
            read_prefix(&listing.opened, "Pal/Saved/a.sav", 2).unwrap(),
            b"aa"
        );
        let dest = dir.join("a.sav");
        copy_out(&listing.opened, "Pal/Saved/a.sav", &dest, &mut |_| ()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"aaaa");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zip_entry_that_would_climb_out_is_refused() {
        let dir = scratch("evil");
        let archive = dir.join("evil.zip");
        zip_of(&archive, &[("ok.sav", b"x"), ("a/../../escape.sav", b"x")]);
        let refused = open(&archive).err().expect("refused");
        assert_eq!(refused.code, RefusalCode::UnsafePath);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_file_is_a_source_of_one_entry() {
        let dir = scratch("one");
        let world = dir.join("My World.wld");
        fs::write(&world, b"relogic").unwrap();
        let listing = open(&world).unwrap();
        assert_eq!(listing.entries[0].path, "My World.wld");
        assert_eq!(listing.root_name, None);
        let _ = fs::remove_dir_all(&dir);
    }
}

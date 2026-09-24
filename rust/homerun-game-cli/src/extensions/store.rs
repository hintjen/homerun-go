//! What an extension keeps between runs.
//!
//! Two stores, because two kinds of thing are kept:
//!
//! | Store | Where | Holds | Protection |
//! |---|---|---|---|
//! | [`MachineStore`] | `<tools dir>/extensions/<name>/` | one document per extension per machine: an account's refresh token | sealed to this user (DPAPI), exclusive lock, atomic save |
//! | [`ServerStore`] | `<server dir>/.homerun/extensions/<name>.json` | one small document per server: a chosen profile | plain JSON, atomic save; **never a secret** |
//!
//! # Why the machine store locks and saves the way it does
//!
//! A refresh token that rotates is invalidated the moment its replacement
//! is issued. Two servers starting together that both refresh the stored
//! token sign the person out; a crash between "vendor issued a new token"
//! and "new token on disk" does the same. So [`MachineStore::update`] holds
//! an exclusive lock from reading the old document to saving the new one,
//! and saves by writing a new file and renaming it over the old, which
//! leaves either the old document or the new one -- never neither, never
//! half of one.
//!
//! The tools directory is outside every server folder, so a machine store
//! is never in a backup; the server store sits in a dot-folder that no
//! descriptor's `saves.paths` names.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use homerun_supervisor::local_secret;
use serde_json::Value;

use super::ExtError;
use crate::protocol::codes;

const DOCUMENT: &str = "store.bin";
const LOCK: &str = ".lock";

/// How long an update waits for another holder of the lock.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// One extension's sealed document on this machine.
#[derive(Debug, Clone)]
pub struct MachineStore {
    dir: PathBuf,
    name: &'static str,
}

impl MachineStore {
    pub(crate) fn new(tools_dir: &Path, name: &'static str) -> Self {
        Self {
            dir: tools_dir.join("extensions").join(name),
            name,
        }
    }

    /// The document, or `None` if nothing is kept.
    ///
    /// A document that cannot be opened -- copied from another computer or
    /// account, or damaged -- reads as `None`: an extension treats it as
    /// "not signed in" and asks again, rather than failing every start.
    pub fn read(&self) -> Result<Option<Value>, ExtError> {
        let _lock = self.lock()?;
        Ok(self.load())
    }

    /// Replace the document with what `change` makes of it, under the lock.
    /// `None` from `change` deletes it. Returns what was saved.
    ///
    /// Nothing is saved if `change` fails.
    pub fn update(
        &self,
        change: impl FnOnce(Option<Value>) -> Result<Option<Value>, ExtError>,
    ) -> Result<Option<Value>, ExtError> {
        let _lock = self.lock()?;
        let next = change(self.load())?;
        match &next {
            Some(value) => {
                let plain = serde_json::to_vec(value).map_err(|_| cannot_save())?;
                let sealed = local_secret::seal(&plain, self.name).map_err(|_| cannot_save())?;
                write_atomically(&self.dir.join(DOCUMENT), &sealed).map_err(|_| cannot_save())?;
            }
            None => match fs::remove_file(self.dir.join(DOCUMENT)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(cannot_save()),
            },
        }
        Ok(next)
    }

    /// Forget everything: the host's "Sign out".
    pub fn clear(&self) -> Result<(), ExtError> {
        self.update(|_| Ok(None)).map(|_| ())
    }

    fn load(&self) -> Option<Value> {
        let sealed = fs::read(self.dir.join(DOCUMENT)).ok()?;
        let opened = match local_secret::open(&sealed, self.name) {
            Ok(plain) => serde_json::from_slice(&plain).ok(),
            Err(_) => None,
        };
        if opened.is_none() {
            eprintln!(
                "What the \"{}\" extension kept on this computer cannot be read; treating it \
                 as not signed in.",
                self.name
            );
        }
        opened
    }

    fn lock(&self) -> Result<File, ExtError> {
        fs::create_dir_all(&self.dir).map_err(|_| cannot_save())?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK))
            .map_err(|_| cannot_save())?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => {
                    return Err(ExtError::new(
                        codes::EXTENSION_FAILED,
                        "Another server is using this game's sign-in right now. Try again in a \
                         moment.",
                    ))
                }
            }
        }
    }
}

/// One server's small, plain document for its extension.
#[derive(Debug, Clone)]
pub struct ServerStore {
    path: PathBuf,
}

impl ServerStore {
    pub(crate) fn new(server_dir: &Path, name: &str) -> Self {
        Self {
            path: server_dir
                .join(".homerun")
                .join("extensions")
                .join(format!("{name}.json")),
        }
    }

    /// The document, or `None` if nothing is kept (or it cannot be read).
    pub fn read(&self) -> Option<Value> {
        serde_json::from_slice(&fs::read(&self.path).ok()?).ok()
    }

    /// Replace the document. Not for secrets: this file is plain JSON.
    pub fn write(&self, value: &Value) -> Result<(), ExtError> {
        let text = serde_json::to_vec_pretty(value).map_err(|_| cannot_save())?;
        write_atomically(&self.path, &text).map_err(|_| cannot_save())
    }
}

/// Write beside, flush, then rename over: the old file or the new one.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = File::create(&staging)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&staging, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

fn cannot_save() -> ExtError {
    ExtError::new(
        codes::EXTENSION_FAILED,
        "Homerun could not save this game's sign-in on this computer. Check that there is \
         free disk space, then try again.",
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn temp() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "homerun-ext-store-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_document_round_trips_sealed_and_clears() {
        let store = MachineStore::new(&temp(), "fixture");
        assert_eq!(store.read().unwrap(), None);
        store
            .update(|old| {
                assert_eq!(old, None);
                Ok(Some(json!({ "refresh": "rt-secret-91ab" })))
            })
            .unwrap();
        assert_eq!(store.read().unwrap().unwrap()["refresh"], "rt-secret-91ab");
        let on_disk = fs::read(store.dir.join(DOCUMENT)).unwrap();
        assert!(
            !String::from_utf8_lossy(&on_disk).contains("rt-secret-91ab"),
            "sealed, not plain"
        );
        store.clear().unwrap();
        assert_eq!(store.read().unwrap(), None);
        assert!(!store.dir.join(DOCUMENT).exists());
    }

    #[test]
    fn a_failed_change_saves_nothing() {
        let store = MachineStore::new(&temp(), "fixture");
        store.update(|_| Ok(Some(json!(1)))).unwrap();
        let err = store
            .update(|_| Err(ExtError::new(codes::VENDOR_UNAVAILABLE, "no")))
            .unwrap_err();
        assert_eq!(err.code, codes::VENDOR_UNAVAILABLE);
        assert_eq!(store.read().unwrap(), Some(json!(1)));
    }

    /// The rotation race: updates from many threads, each reading the last
    /// value and writing the next, lose none of them.
    #[test]
    fn updates_are_serialised_by_the_lock() {
        let store = Arc::new(MachineStore::new(&temp(), "fixture"));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                thread::spawn(move || {
                    for _ in 0..5 {
                        store
                            .update(|old| {
                                let n = old.and_then(|v| v.as_u64()).unwrap_or(0);
                                thread::sleep(Duration::from_millis(2));
                                Ok(Some(json!(n + 1)))
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(store.read().unwrap(), Some(json!(40)));
    }

    /// A crash between writing the new document and renaming it over the
    /// old leaves the old one readable, and the next save succeeds.
    #[test]
    fn a_crash_mid_save_leaves_the_old_document() {
        let store = MachineStore::new(&temp(), "fixture");
        store.update(|_| Ok(Some(json!("old")))).unwrap();
        fs::write(
            store.dir.join(DOCUMENT).with_extension("999.tmp"),
            b"half a document",
        )
        .unwrap();
        assert_eq!(store.read().unwrap(), Some(json!("old")));
        store.update(|_| Ok(Some(json!("new")))).unwrap();
        assert_eq!(store.read().unwrap(), Some(json!("new")));
    }

    #[test]
    fn a_document_that_cannot_be_opened_reads_as_nothing() {
        let store = MachineStore::new(&temp(), "fixture");
        fs::create_dir_all(&store.dir).unwrap();
        fs::write(store.dir.join(DOCUMENT), b"copied from another computer").unwrap();
        assert_eq!(store.read().unwrap(), None);
        // And another extension's sealed document does not open either.
        let root = temp();
        let other = MachineStore::new(&root, "fixture");
        other.update(|_| Ok(Some(json!("mine")))).unwrap();
        fs::create_dir_all(root.join("extensions/hytale")).unwrap();
        fs::copy(
            other.dir.join(DOCUMENT),
            root.join("extensions/hytale").join(DOCUMENT),
        )
        .unwrap();
        assert_eq!(MachineStore::new(&root, "hytale").read().unwrap(), None);
    }

    #[test]
    fn a_server_document_round_trips() {
        let dir = temp();
        let store = ServerStore::new(&dir, "fixture");
        assert_eq!(store.read(), None);
        store.write(&json!({ "profile": "p-1" })).unwrap();
        assert_eq!(store.read().unwrap()["profile"], "p-1");
        assert!(dir.join(".homerun/extensions/fixture.json").is_file());
    }
}

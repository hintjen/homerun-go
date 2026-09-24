//! Runtime ownership and recoverable save links. No updater runs through a link.
use crate::prepare::{confined, fail, Result};
use crate::protocol::codes;
use homerun_core::engine::{descriptor::Mount, validate::mount_path_is_safe, GameDescriptor};
use homerun_supervisor::{
    job::Job,
    platform::{self, RuntimeLease},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Record {
    mounts: Vec<Mount>,
    job: String,
    /// The vendor runtime version whose directory the mounts were made in.
    /// Absent for every other source, and in a record written before
    /// runtimes could be versioned: both mean `<root>/<id>` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

pub struct Runtime {
    pub dir: PathBuf,
    /// The vendor runtime version `dir` holds, if it is one.
    version: Option<String>,
    journal: PathBuf,
    mounts: Vec<Mount>,
    job: Option<Arc<Job>>,
    // Keep the lease until AFTER Drop has removed the links.
    _lease: RuntimeLease,
}

impl Runtime {
    /// `version` is the vendor runtime's, already checked by
    /// `prepare::runtime_version`; `None` for every other source.
    pub fn acquire(d: &GameDescriptor, root: &Path, version: Option<&str>) -> Result<Self> {
        Self::acquire_with(d, root, version, Job::recover)
    }

    fn acquire_with(
        d: &GameDescriptor,
        root: &Path,
        version: Option<&str>,
        recover: impl FnOnce(&str) -> std::result::Result<(), String>,
    ) -> Result<Self> {
        fs::create_dir_all(root)
            .map_err(|_| fail(codes::FETCH_FAILED, "The runtime folder cannot be created."))?;
        let root = fs::canonicalize(root).map_err(|_| {
            fail(
                codes::FETCH_FAILED,
                "The runtime folder cannot be resolved.",
            )
        })?;
        let lock = confined(&root, &format!(".homerun-runtime-{}.lock", d.id))?;
        let lease = RuntimeLease::acquire(&lock).map_err(|e| fail(codes::BUSY, e))?;
        // The lock and the journal stay per game, not per version: one
        // runner at a time owns a game's runtimes, whichever version it runs.
        let game = confined(&root, &d.id)?;
        let dir_for = |version: Option<&str>| -> Result<PathBuf> {
            match version {
                None => Ok(game.clone()),
                Some(v) => {
                    homerun_core::engine::fetch::check_runtime_version(v)
                        .map_err(|e| fail(codes::FETCH_FAILED, e.to_string()))?;
                    confined(&game, v)
                }
            }
        };
        let mut owned = Self {
            dir: dir_for(version)?,
            version: version.map(str::to_string),
            journal: confined(&root, &format!(".homerun-mounts-{}.json", d.id))?,
            mounts: vec![],
            job: None,
            _lease: lease,
        };
        // Recover even if the new descriptor no longer declares these mounts.
        match fs::read(&owned.journal) {
            Ok(bytes) => {
                let record: Record = serde_json::from_slice(&bytes).map_err(|_| fail(codes::FETCH_FAILED,
                    "The saved runtime mount record lacks valid process ownership. Stop all users and repair it before updating this game."))?;
                for mount in &record.mounts {
                    if !mount_path_is_safe(&mount.runtime) || !mount_path_is_safe(&mount.server) {
                        return Err(fail(
                            codes::FETCH_FAILED,
                            "The saved runtime mount record has an unsafe path.",
                        ));
                    }
                }
                // The mounts are in the directory of the version that made
                // them, which need not be the one being launched now.
                let recorded_dir = dir_for(record.version.as_deref())?;
                // Do not arm Drop cleanup until the entire old tree is gone.
                recover(&record.job).map_err(|e| fail(codes::FETCH_FAILED, e))?;
                let current = std::mem::replace(&mut owned.dir, recorded_dir);
                owned.mounts = record.mounts;
                owned.cleanup()?;
                owned.dir = current;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(fail(
                    codes::FETCH_FAILED,
                    "The saved runtime mount record cannot be opened.",
                ))
            }
        }
        // Undocumented/pre-existing links are never handed to an updater.
        // Refuse rather than guessing who owns them or deleting a real folder.
        for mount in &d.saves.mounts {
            let link = owned.link(&mount.runtime)?;
            if fs::symlink_metadata(&link).is_ok() {
                return Err(fail(codes::FETCH_FAILED, "A save mount location already exists in the runtime. Move or recover it before updating this game."));
            }
        }
        Ok(owned)
    }

    fn link(&self, relative: &str) -> Result<PathBuf> {
        if !mount_path_is_safe(relative) {
            return Err(fail(
                codes::DESCRIPTOR_INVALID,
                "A save mount path is unsafe.",
            ));
        }
        let path = Path::new(relative);
        // The leaf may itself be the junction we must recover. Its parents
        // must not redirect cleanup into any other directory.
        let parent = confined(
            &self.dir,
            path.parent().unwrap_or(Path::new("")).to_str().unwrap(),
        )?;
        Ok(parent.join(path.file_name().unwrap()))
    }

    pub fn process_job(&self) -> Option<Arc<Job>> {
        self.job.clone()
    }

    pub fn install(&mut self, d: &GameDescriptor, server: &Path) -> Result<()> {
        self.install_with(d, server, Job::required)
    }

    fn install_with(
        &mut self,
        d: &GameDescriptor,
        server: &Path,
        create_job: impl FnOnce(&str) -> std::result::Result<Job, String>,
    ) -> Result<()> {
        if d.saves.mounts.is_empty() {
            return Ok(());
        }
        let runtime = fs::canonicalize(&self.dir)
            .map_err(|_| fail(codes::SPAWN_FAILED, "The runtime cannot be resolved."))?;
        let server = fs::canonicalize(server)
            .map_err(|_| fail(codes::SPAWN_FAILED, "The server folder cannot be resolved."))?;
        if runtime.starts_with(&server) || server.starts_with(&runtime) {
            return Err(fail(
                codes::DESCRIPTOR_INVALID,
                "Runtime and server folders must be separate when mounting saves.",
            ));
        }
        let mut paths = vec![];
        for mount in &d.saves.mounts {
            let link = self.link(&mount.runtime)?;
            let target = confined(&server, &mount.server)?;
            if fs::symlink_metadata(&link).is_ok() {
                return Err(fail(codes::SPAWN_FAILED, "The runtime already contains a save directory. Move it explicitly; Homerun Desktop will not replace it."));
            }
            fs::create_dir_all(&target)
                .map_err(|_| fail(codes::SPAWN_FAILED, "The save folder cannot be created."))?;
            fs::create_dir_all(link.parent().unwrap()).map_err(|_| {
                fail(
                    codes::SPAWN_FAILED,
                    "The runtime save mount parent cannot be created.",
                )
            })?;
            paths.push((link, target));
        }
        // Write-ahead: interruption at any point leaves enough information to
        // unlink only our locations before a future fetch, even for server B.
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name = format!(
            "Global\\HomerunSave-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // No mount or process exists if creation/configuration fails.
        let job = create_job(&name).map_err(|e| fail(codes::SPAWN_FAILED, e))?;
        let bytes = serde_json::to_vec(&Record {
            mounts: d.saves.mounts.clone(),
            job: name,
            version: self.version.clone(),
        })
        .unwrap();
        let mut record = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.journal)
            .map_err(|_| {
                fail(
                    codes::SPAWN_FAILED,
                    "The save mount recovery record cannot be created.",
                )
            })?;
        record
            .write_all(&bytes)
            .and_then(|_| record.sync_all())
            .map_err(|_| {
                fail(
                    codes::SPAWN_FAILED,
                    "The save mount recovery record cannot be saved.",
                )
            })?;
        self.job = Some(Arc::new(job));
        self.mounts = d.saves.mounts.clone();
        drop(record);
        for (link, target) in paths {
            platform::mount_dir(&link, &target).map_err(|e| fail(codes::SPAWN_FAILED, e))?;
            let actual = fs::canonicalize(&link)
                .map_err(|_| fail(codes::SPAWN_FAILED, "The save mount cannot be verified."))?;
            if actual
                != fs::canonicalize(&target)
                    .map_err(|_| fail(codes::SPAWN_FAILED, "The save target cannot be verified."))?
            {
                return Err(fail(
                    codes::SPAWN_FAILED,
                    "The save mount points at the wrong server.",
                ));
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        if let Some(job) = &self.job {
            job.terminate_and_wait()
                .map_err(|e| fail(codes::FETCH_FAILED, e))?;
        }
        for mount in &self.mounts {
            let link = self.link(&mount.runtime)?;
            match fs::symlink_metadata(&link) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Ok(_) if platform::is_mount(&link) => {
                    platform::unmount_dir(&link).map_err(|e| fail(codes::FETCH_FAILED, e))?;
                }
                _ => return Err(fail(codes::FETCH_FAILED, "A recorded save mount is now a real directory or cannot be inspected. No files were removed; repair it before updating.")),
            }
        }
        if !self.mounts.is_empty() {
            fs::remove_file(&self.journal).map_err(|_| {
                fail(
                    codes::FETCH_FAILED,
                    "The save mount recovery record cannot be removed.",
                )
            })?;
            self.mounts.clear();
        }
        Ok(())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err((_, message)) = self.cleanup() {
            // The journal stays for a fail-closed recovery on the next launch.
            eprintln!("Runtime save mount cleanup: {message}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn unconfirmed_tree_exit_preserves_junction_and_journal() {
        let root =
            std::env::temp_dir().join(format!("mounted-exit-failure-{}", std::process::id()));
        fs::create_dir_all(root.join("runtime/fake")).unwrap();
        fs::create_dir_all(root.join("server/world")).unwrap();
        fs::write(root.join("server/world/keep"), "world").unwrap();
        let link = root.join("runtime/fake/server");
        platform::mount_dir(&link, &root.join("server/world")).unwrap();
        let d: GameDescriptor = serde_json::from_value(serde_json::json!({"id":"fake"})).unwrap();
        let journal = root.join("runtime/.homerun-mounts-fake.json");
        fs::write(
            &journal,
            serde_json::to_vec(&Record {
                mounts: vec![Mount {
                    runtime: "server".into(),
                    server: "world".into(),
                }],
                job: "Global\\HomerunSave-test-unconfirmed".into(),
                version: None,
            })
            .unwrap(),
        )
        .unwrap();
        let result = Runtime::acquire_with(&d, &root.join("runtime"), None, |_| {
            Err("injected exit query failure".into())
        });
        assert!(
            result.is_err(),
            "unconfirmed tree exit allowed runtime reuse"
        );
        assert!(
            platform::is_mount(&link),
            "failed recovery unlinked a live game's save directory"
        );
        assert!(
            journal.exists(),
            "failed recovery discarded the ownership record"
        );
        platform::unmount_dir(&link).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("server/world/keep")).unwrap(),
            "world"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A journal from a vendor runtime names the version whose directory
    /// its mounts are in, and recovery looks there -- not in the version
    /// being launched now, and not in the game's own directory. A real
    /// directory where the record says a link is makes recovery refuse, which
    /// is how this can see where it looked.
    #[test]
    fn recovery_looks_in_the_version_directory_the_journal_names() {
        let root = std::env::temp_dir().join(format!("versioned-recovery-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("runtime/fake/1.4.5.8/server")).unwrap();
        let d: GameDescriptor = serde_json::from_value(serde_json::json!({"id":"fake"})).unwrap();
        let journal = root.join("runtime/.homerun-mounts-fake.json");
        let write = |version: Option<&str>| {
            fs::write(
                &journal,
                serde_json::to_vec(&Record {
                    mounts: vec![Mount {
                        runtime: "server".into(),
                        server: "world".into(),
                    }],
                    job: "Global\\HomerunSave-test-versioned".into(),
                    version: version.map(str::to_string),
                })
                .unwrap(),
            )
            .unwrap();
        };

        write(Some("1.4.5.8"));
        let result = Runtime::acquire_with(&d, &root.join("runtime"), Some("1.4.6.0"), |_| Ok(()));
        assert!(
            result.is_err(),
            "recovery did not look in the recorded version's directory"
        );
        assert!(
            root.join("runtime/fake/1.4.5.8/server").is_dir(),
            "nothing was removed"
        );

        // A journal from before versions means the game's own directory,
        // which has nothing at `server`: recovery completes.
        write(None);
        let runtime = Runtime::acquire_with(&d, &root.join("runtime"), Some("1.4.6.0"), |_| Ok(()))
            .expect("an old journal names the unversioned directory");
        assert!(runtime.dir.ends_with("fake/1.4.6.0") || runtime.dir.ends_with("fake\\1.4.6.0"));
        assert!(!journal.exists());
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn job_creation_failure_leaves_no_mount_or_recovery_record() {
        let root = std::env::temp_dir().join(format!("mounted-job-failure-{}", std::process::id()));
        fs::create_dir_all(root.join("runtime/fake")).unwrap();
        fs::create_dir_all(root.join("server/world")).unwrap();
        fs::write(root.join("server/world/keep"), "untouched").unwrap();
        let d: GameDescriptor = serde_json::from_value(serde_json::json!({
            "id":"fake", "saves":{"mounts":[{"runtime":"server", "server":"world"}]}
        }))
        .unwrap();
        let mut runtime = Runtime::acquire(&d, &root.join("runtime"), None).unwrap();
        let result = runtime.install_with(&d, &root.join("server"), |_| {
            Err("injected job creation failure".into())
        });
        assert!(
            result.is_err(),
            "job creation failure must refuse a mounted launch"
        );
        assert!(runtime.process_job().is_none());
        assert!(
            !root.join("runtime/fake/server").exists(),
            "failed ownership left an active mount"
        );
        assert!(
            !runtime.journal.exists(),
            "failed ownership left a reusable journal"
        );
        assert_eq!(
            fs::read_to_string(root.join("server/world/keep")).unwrap(),
            "untouched"
        );
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }
}

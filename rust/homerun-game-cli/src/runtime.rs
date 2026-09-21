//! Runtime ownership and recoverable save links. No updater runs through a link.
use crate::prepare::{confined, fail, Result};
use crate::protocol::codes;
use homerun_core::engine::{descriptor::Mount, validate::mount_path_is_safe, GameDescriptor};
use homerun_supervisor::platform::{self, RuntimeLease};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub struct Runtime {
    pub dir: PathBuf,
    journal: PathBuf,
    mounts: Vec<Mount>,
    // Keep the lease until AFTER Drop has removed the links.
    _lease: RuntimeLease,
}

impl Runtime {
    pub fn acquire(d: &GameDescriptor, root: &Path) -> Result<Self> {
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
        let mut owned = Self {
            dir: confined(&root, &d.id)?,
            journal: confined(&root, &format!(".homerun-mounts-{}.json", d.id))?,
            mounts: vec![],
            _lease: lease,
        };
        // Recover even if the new descriptor no longer declares these mounts.
        match fs::read(&owned.journal) {
            Ok(bytes) => {
                let mounts: Vec<Mount> = serde_json::from_slice(&bytes).map_err(|_| fail(codes::FETCH_FAILED,
                    "The saved runtime mount record cannot be read. Repair it before updating this game."))?;
                for mount in &mounts {
                    if !mount_path_is_safe(&mount.runtime) || !mount_path_is_safe(&mount.server) {
                        return Err(fail(
                            codes::FETCH_FAILED,
                            "The saved runtime mount record has an unsafe path.",
                        ));
                    }
                }
                owned.mounts = mounts;
                owned.cleanup()?;
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

    pub fn install(&mut self, d: &GameDescriptor, server: &Path) -> Result<()> {
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
        let bytes = serde_json::to_vec(&d.saves.mounts).unwrap();
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

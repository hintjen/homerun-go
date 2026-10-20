//! Process-tree ownership on Linux: an inherited tag, a process group, and a
//! parent-death signal.
//!
//! Linux has the thing that does this properly, a cgroup, but a runner cannot
//! count on making one. A cgroup it may write needs a delegated subtree, which
//! means a systemd user session that hands one out, and neither WSL nor a good
//! share of desktop sessions has that. So ownership is three weaker parts
//! that between them give the two properties `job.rs` asks for:
//!
//! - **The tag lists the tree.** Every member is spawned with
//!   [`OWNER_ENV`] set to the job's name. A process's starting environment is
//!   inherited by everything it starts and stays readable in
//!   `/proc/<pid>/environ`, re-parented or not. That is what lets a launcher
//!   exit without hiding its children, and what lets a later runner find a
//!   previous run's tree by the name its journal recorded.
//! - **The group is the kill for anything that cleared its environment.** The
//!   root leads a new process group, which also keeps a terminal's Ctrl+C,
//!   meant for the runner, from reaching the game first.
//! - **`PR_SET_PDEATHSIG` is the orphan guarantee.** A runner that is
//!   hard-killed runs no code, so the kernel has to do it: the root receives
//!   `SIGKILL` when the thread that spawned it exits. That thread is the one
//!   that waits on the child for its whole life (`run_streamed`), and the
//!   tunnel's is the runner's main loop, so it means the runner.
//!
//! # What this does not cover
//!
//! A descendant of the root is not signalled when the runner is hard-killed:
//! the parent-death signal is the root's alone. Windows' kill-on-close takes
//! the whole job. The descriptors point `launch` at the server binary and not
//! at a vendor's shell wrapper for this reason, so the root *is* the server.
//! A mounted runtime's next start finds anything left by its tag and ends it
//! before touching the links (`Job::recover`).
//!
//! A process that both clears its environment and leaves the group escapes
//! the listing. Nothing a vendor server has been seen to do looks like that.

use std::{
    fs, io,
    os::unix::{fs::MetadataExt, process::CommandExt},
    process::Command,
    time::{Duration, Instant},
};

/// The variable every member of an owned tree carries: the job's name.
pub(crate) const OWNER_ENV: &str = "HOMERUN_GAME_OWNER";

/// Mark `command` as the root of the tree named `tag`.
pub(crate) fn prepare(tag: &str, command: &mut Command) {
    let runner = std::process::id() as i32;
    command.env(OWNER_ENV, tag).process_group(0);
    // SAFETY: the closure runs between fork and exec, and only makes
    // async-signal-safe calls (prctl, getppid) on values copied in.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The runner may have died between the fork and the prctl, in
            // which case the signal it was meant to send has already been
            // missed. Refuse rather than start a game nothing owns.
            if libc::getppid() != runner {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// Every live process carrying `tag`, found by reading `/proc`.
///
/// Only this user's processes are read: a member is always started by the
/// runner, so one owned by somebody else is not a member. A zombie has no
/// environment left to read and is not listed, which is right for every
/// caller -- it holds no sockets and cannot be killed again.
pub(crate) fn members(tag: &str) -> Result<Vec<u32>, String> {
    let needle = format!("{OWNER_ENV}={tag}");
    // SAFETY: geteuid cannot fail.
    let me = unsafe { libc::geteuid() };
    let entries =
        fs::read_dir("/proc").map_err(|e| format!("Cannot inspect game process ownership: {e}"))?;
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        match entry.metadata() {
            Ok(meta) if meta.uid() == me => {}
            _ => continue,
        }
        // Gone between the listing and the read, or not readable: not ours
        // either way, since every member is this user's and dumpable.
        let Ok(environ) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        if environ
            .split(|b| *b == 0)
            .any(|var| var == needle.as_bytes())
        {
            pids.push(pid);
        }
    }
    Ok(pids)
}

/// `SIGKILL` every member, and the root's group while the root is one of them.
///
/// The group only while the root is alive and listed: a group id is the
/// leader's pid, and once the leader is gone that number can name someone
/// else's group.
pub(crate) fn kill_all(tag: &str, root: Option<u32>) -> Result<(), String> {
    let pids = members(tag)?;
    if let Some(root) = root.filter(|root| pids.contains(root)) {
        // SAFETY: a group this runner created, led by a live member.
        unsafe { libc::kill(-(root as i32), libc::SIGKILL) };
    }
    for pid in pids {
        // SAFETY: a process carrying this job's tag. ESRCH is ignored.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    Ok(())
}

/// Kill until nothing carrying `tag` is left, or ten seconds have passed.
///
/// Repeated rather than done once, because a member can fork between the
/// listing and the signal; its child carries the tag and is caught next time.
pub(crate) fn kill_and_wait(tag: &str, root: Option<u32>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        kill_all(tag, root)?;
        std::thread::sleep(Duration::from_millis(10));
        if members(tag)?.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                "The previous game process tree has not exited. Save links were retained.".into(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn unique(label: &str) -> String {
        format!(
            "HomerunTest-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    /// A launcher that starts the real server and exits is the case this
    /// exists for: the grandchild must still be listed, and killed, by tag.
    #[test]
    fn a_grandchild_whose_parent_exited_is_still_a_member() {
        let tag = unique("orphan");
        let mut command = Command::new("sh");
        command
            // The grandchild's output goes elsewhere: holding the pipe open
            // would keep `wait_with_output` waiting for it.
            .args(["-c", "sleep 120 >/dev/null 2>&1 & echo $!"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        prepare(&tag, &mut command);
        let output = command.spawn().unwrap().wait_with_output().unwrap();
        let grandchild: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();

        until("the grandchild to be listed", || {
            members(&tag).unwrap() == vec![grandchild]
        });
        kill_and_wait(&tag, None).unwrap();
        assert!(members(&tag).unwrap().is_empty());
    }

    #[test]
    fn the_root_leads_its_own_group() {
        let tag = unique("group");
        let mut command = Command::new("sleep");
        command.arg("120").stdin(Stdio::null());
        prepare(&tag, &mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id() as i32;
        // SAFETY: getpgid on our own child.
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        kill_all(&tag, Some(child.id())).unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_tag_nobody_carries_has_no_members() {
        assert!(members(&unique("empty")).unwrap().is_empty());
        kill_and_wait(&unique("empty"), None).unwrap();
    }

    /// The orphan guarantee. The signal follows the thread that spawned the
    /// root, so a thread that exits without waiting stands in for a runner
    /// that was killed.
    #[test]
    fn the_root_dies_with_the_thread_that_spawned_it() {
        let tag = unique("pdeathsig");
        let spawner = std::thread::spawn({
            let tag = tag.clone();
            move || {
                let mut command = Command::new("sleep");
                command.arg("120").stdin(Stdio::null());
                prepare(&tag, &mut command);
                let child = command.spawn().unwrap();
                // Leak the Child: no wait, so the thread exits with the root
                // still running and the kernel has to end it.
                std::mem::forget(child);
            }
        });
        spawner.join().unwrap();
        until("the root to be killed with its spawning thread", || {
            members(&tag).unwrap().is_empty()
        });
    }
}

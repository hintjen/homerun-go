//! Owning a child's whole process tree, on the one platform that needs it.
//!
//! # What was wrong
//!
//! A game was spawned with no creation flags and killed with
//! `taskkill /PID n /F`. Neither of those owns anything:
//!
//! - **The runner dying took nothing with it.** Electron crashes, Task
//!   Manager ends the runner, a detached runner is closed at quit — and the
//!   game carries on, holding its ports and writing to its save directory.
//!   The next start fails `port_unavailable`, and a start that *does* get
//!   past the preflight is a second server writing the same world.
//! - **The kill was one process.** A launcher-style executable — which is
//!   what a great many vendor servers ship — starts the real server and
//!   exits, so the pid the runner holds is not the pid doing the work. The
//!   stop ladder's last rung, the one that exists because a stop that can be
//!   refused is not a stop, ended a process that had already gone.
//!
//! # Why a Job Object and not something simpler
//!
//! On Unix this is a process group and a signal to `-pid`. Windows has no
//! equivalent: a process tree is not a group, parent links are not kept once
//! a parent exits, and `taskkill /T` walks exactly those links. A launcher
//! that exits is invisible to `/T` within milliseconds of starting.
//!
//! A Job Object is the only thing on Windows that *owns* a subtree rather
//! than describing one. Membership is inherited by every process a member
//! creates, it survives re-parenting, and it cannot be left.
//!
//! # One job per spawn, and the runner is not in it
//!
//! The obvious alternative is a single job that the runner itself lives in.
//! It closes the spawn-to-assignment window below, because children inherit
//! membership at creation — but it cannot be used for the kill rung, since
//! terminating that job would terminate the runner, and it entangles this
//! with how the desktop treats the runner at quit. The desktop now
//! **detaches** from the runner on a slow quit rather than killing it,
//! relying on it to finish its own stop ladder; a job containing the runner
//! is one more thing that has to be reasoned about when Electron exits.
//!
//! A job per spawned child has neither problem. Electron exiting does
//! nothing to it: the runner holds the handle, the ladder runs to the end,
//! and the job closes when the runner is finished with it and not before.
//!
//! # The two properties, and where each comes from
//!
//! - **Nothing is orphaned.** `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means
//!   that when the last handle to the job closes, everything still in it is
//!   terminated. The runner holds that handle for the child's whole life, so
//!   a runner that is hard-killed — which closes its handles whether it likes
//!   it or not — takes the game and every descendant with it. This is the
//!   property that needed no code in the runner at all, which is what makes
//!   it trustworthy: it works on the paths that never get to run.
//! - **The kill rung is a tree kill.** `TerminateJobObject` ends every
//!   process in the job, re-parented or not.
//!
//! # The window this does not close
//!
//! A child is assigned to its job immediately after `CreateProcess` returns,
//! not before it runs. In the microseconds between, the child is in no job,
//! so a grandchild started in that window would not be a member. For it to
//! matter, a vendor's server would have to spawn something before its image
//! has finished loading. It is stated rather than hidden because the fix —
//! `CREATE_SUSPENDED` and a resume that `std::process` gives no thread handle
//! for — trades a window nothing has ever fallen through for a failure mode
//! where the game never starts at all.
//!
//! # Everywhere else
//!
//! [`Job::kill_on_close`] returns `None` off Windows, so call sites carry no
//! `cfg`. Android and iOS keep exactly the behaviour they had: Android's
//! stop ladder ends in `SIGKILL` to a pid, which on a platform with process
//! groups and no re-parenting launchers is the right answer already.

use std::process::Child;

/// A child process and everything it goes on to start.
///
/// Dropping it terminates every process still inside. That is the point, and
/// it is why the handle is held for the child's whole life rather than closed
/// once the assignment is done.
pub struct Job {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: a job handle is a kernel object, and the three calls made on it
// here -- assign, terminate, close -- are safe to make from any thread. The
// stop watcher runs on its own thread and has to be able to reach the kill
// rung, which is the whole reason this crosses a thread at all.
#[cfg(windows)]
unsafe impl Send for Job {}
#[cfg(windows)]
unsafe impl Sync for Job {}

impl Job {
    /// A job that terminates its members when the last handle to it closes.
    ///
    /// `None` off Windows, and `None` if the job cannot be created — in which
    /// case the caller falls back to what it did before, because a server
    /// that starts without a job is worse supervised and a server that does
    /// not start is worse than that.
    #[cfg(windows)]
    pub fn kill_on_close() -> Option<Job> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: an unnamed job with default security, which is what the
        // two null arguments mean.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }
        let job = Job { handle };

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        // SAFETY: the handle is one we just created, the class matches the
        // struct being passed, and the length is that struct's own size.
        let set = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            // Without the limit the job owns nothing, and a job that owns
            // nothing is worse than none: it would look like supervision.
            return None;
        }
        Some(job)
    }

    #[cfg(not(windows))]
    pub fn kill_on_close() -> Option<Job> {
        None
    }

    /// Put a freshly spawned child, and everything it starts, in this job.
    ///
    /// False means the child is not a member and the caller is no worse off
    /// than before — on Windows 7 a process already in a job could not join a
    /// second one, and a desktop that puts the runner in its own job would
    /// hit exactly that. Nested jobs have been allowed since Windows 8.
    #[cfg(windows)]
    pub fn adopt(&self, child: &Child) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        // SAFETY: `as_raw_handle` is the live process handle std holds for
        // this child, valid for as long as the `Child` is.
        unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle()) != 0 }
    }

    #[cfg(not(windows))]
    pub fn adopt(&self, _child: &Child) -> bool {
        false
    }

    /// End every process in the job, re-parented or not.
    ///
    /// The stop ladder's last rung. Nothing here can be refused, which is the
    /// property that rung exists for.
    #[cfg(windows)]
    pub fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: a handle we own. The exit code is what every member is
        // reported as having exited with; 1 rather than 0 so a killed server
        // is not recorded as one that stopped cleanly.
        unsafe { TerminateJobObject(self.handle, 1) };
    }

    #[cfg(not(windows))]
    pub fn terminate(&self) {}
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // This is the orphan guarantee, and it is a side effect of closing a
        // handle rather than anything this process chooses to do -- which is
        // exactly why it survives the paths where this process chooses
        // nothing at all.
        // SAFETY: a handle we created and have not closed.
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off Windows there is nothing to own: a process group and a signal
    /// already do this, and Android's ladder ends in `SIGKILL`.
    #[test]
    #[cfg(not(windows))]
    fn there_is_no_job_where_the_platform_does_not_need_one() {
        assert!(Job::kill_on_close().is_none());
    }

    /// The two things every call site depends on, before any child is
    /// involved: a job can be made, and terminating an empty one is not a
    /// panic or a hang.
    #[test]
    #[cfg(windows)]
    fn a_job_can_be_created_and_terminated_while_empty() {
        let job = Job::kill_on_close().expect("Windows must be able to create a job object");
        job.terminate();
        drop(job);
    }

    /// The property the whole module exists for, at its smallest: a child in
    /// a job dies when the last handle to that job closes, with nothing
    /// asking it to.
    #[test]
    #[cfg(windows)]
    fn closing_the_last_handle_ends_what_is_in_the_job() {
        use std::process::{Command, Stdio};

        let job = Job::kill_on_close().expect("a job object");
        // `waitfor` blocks on a signal that never comes, and ships with
        // Windows. A sleep that long would outlive the suite if this failed.
        let mut child = Command::new("cmd")
            .args(["/c", "waitfor", "HomerunJobTest", "/t", "120"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd must be spawnable on Windows");
        assert!(job.adopt(&child), "the child did not join the job");
        assert!(
            child.try_wait().expect("still spawnable").is_none(),
            "the child was gone before the job was closed"
        );

        drop(job);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if child.try_wait().expect("waitable").is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        panic!("closing the job left its child running, which is the orphan this prevents");
    }
}

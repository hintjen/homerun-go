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
//! # Mounted saves require stronger ownership
//!
//! `Job::required` creates a named, mandatory job. The mounted path uses
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST` during `CreateProcessW`, assigning the
//! process before its initial thread runs. Creation or assignment failure
//! refuses launch. No post-spawn adoption window or unowned fallback exists.
//! A recovery record stores the name; recovery reopens the job, terminates it
//! and queries until ActiveProcesses is zero before unlinking or updating.
//! Normal cleanup drains the same job, including surviving descendants.
//! Query/open/termination errors or timeout preserve links and block reuse.
//!
//! # The legacy path's window
//!
//! For unmounted launches, a child is assigned to its job immediately after `CreateProcess` returns,
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
//! Mounted launches refuse off Windows until equivalent ownership exists.
//! [`Job::kill_on_close`] returns `None` off Windows, so legacy call sites carry no
//! `cfg`. Android and iOS keep exactly the behaviour they had: Android's
//! stop ladder ends in `SIGKILL` to a pid, which on a platform with process
//! groups and no re-parenting launchers is the right answer already.

use std::process::Child;

mod owned;
pub(crate) use owned::Process;

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
    /// case legacy unmounted callers retain their prior fallback. Mounted
    /// launches must use `required` and the atomic spawn path instead.
    #[cfg(windows)]
    pub fn kill_on_close() -> Option<Job> {
        Self::create(None).ok()
    }

    /// Descriptor network enforcement requires ownership before game code executes.
    pub fn required_process() -> Result<Job, String> {
        #[cfg(windows)]
        {
            Self::create(None).map_err(|e| format!("Cannot own the game process tree: {e}"))
        }
        #[cfg(not(windows))]
        {
            Err("Checked game process ownership requires Windows.".into())
        }
    }

    /// Current job membership, including descendants whose parent already exited.
    pub fn process_ids(&self) -> Result<Vec<u32>, String> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::{Foundation::ERROR_MORE_DATA, System::JobObjects::*};
            for capacity in [128usize, 1024, 8192, 65536] {
                // usize storage provides pointer alignment; the header is two DWORDs.
                let mut buffer = vec![0usize; 2 + capacity];
                let bytes = (buffer.len() * std::mem::size_of::<usize>()) as u32;
                let ptr = buffer
                    .as_mut_ptr()
                    .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
                let ok = unsafe {
                    QueryInformationJobObject(
                        self.handle,
                        JobObjectBasicProcessIdList,
                        ptr.cast(),
                        bytes,
                        std::ptr::null_mut(),
                    )
                };
                if ok != 0 {
                    let count = unsafe { (*ptr).NumberOfProcessIdsInList } as usize;
                    if unsafe { (*ptr).NumberOfAssignedProcesses } as usize > count {
                        continue;
                    }
                    if count > capacity {
                        return Err("Game process list exceeded its buffer.".into());
                    }
                    let list =
                        unsafe { std::slice::from_raw_parts((*ptr).ProcessIdList.as_ptr(), count) };
                    return list
                        .iter()
                        .map(|id| {
                            u32::try_from(*id)
                                .map_err(|_| "Invalid game process identifier.".into())
                        })
                        .collect();
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                    return Err(format!("Cannot inspect game process ownership: {err}"));
                }
            }
            Err("The game process tree is too large to inspect.".into())
        }
        #[cfg(not(windows))]
        {
            Err("Checked process ownership requires Windows.".into())
        }
    }

    /// A named job for a mounted runtime. Unlike the legacy API, failure is fatal.
    pub fn required(name: &str) -> Result<Job, String> {
        #[cfg(windows)]
        {
            Self::create(Some(name)).map_err(|e| format!("Cannot own the game's process tree: {e}"))
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Err("Save mounts require Windows process-tree ownership on this runner.".into())
        }
    }

    #[cfg(windows)]
    fn create(name: Option<&str>) -> std::io::Result<Job> {
        use windows_sys::Win32::{Foundation::ERROR_ALREADY_EXISTS, System::JobObjects::*};
        let wide = name.map(|s| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>());
        // SAFETY: optional NUL-terminated name, default non-inheritable security.
        let handle = unsafe {
            CreateJobObjectW(
                std::ptr::null(),
                wide.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            )
        };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let existed =
            std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32);
        let job = Job { handle };
        if name.is_some() && existed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "The process ownership name is already in use.",
            ));
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: live job, correctly sized information class and buffer.
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Reopen the exact previous job, terminate it and wait for zero members.
    /// A destroyed job has no surviving members. Any other open/query error
    /// blocks recovery; port availability and PID reuse are irrelevant here.
    pub fn recover(name: &str) -> Result<(), String> {
        if !name.starts_with("Global\\HomerunSave-") || name.contains('\0') {
            return Err("The saved process ownership name is invalid.".into());
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::{
                Foundation::ERROR_FILE_NOT_FOUND,
                System::{
                    JobObjects::*,
                    SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE},
                },
            };
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: valid string; the handle is owned here and never inherited.
            let handle = unsafe {
                OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, wide.as_ptr())
            };
            if handle.is_null() {
                let e = std::io::Error::last_os_error();
                return if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                    Ok(())
                } else {
                    Err(format!("Cannot inspect the previous game job: {e}"))
                };
            }
            Job { handle }.terminate_and_wait()
        }
        #[cfg(not(windows))]
        {
            Err(
                "Cannot establish that the previous mounted game has exited on this platform."
                    .into(),
            )
        }
    }

    /// Explicitly drain the tree before unlinking saves, including descendants
    /// whose launcher already exited. Kill-on-close alone is asynchronous.
    pub fn terminate_and_wait(&self) -> Result<(), String> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::*;
            // SAFETY: live job handle owned by self.
            if unsafe { TerminateJobObject(self.handle, 1) } == 0 {
                return Err(format!(
                    "Cannot terminate the previous game job: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                    unsafe { std::mem::zeroed() };
                // SAFETY: correctly sized output buffer for this information class.
                if unsafe {
                    QueryInformationJobObject(
                        self.handle,
                        JobObjectBasicAccountingInformation,
                        std::ptr::addr_of_mut!(info).cast(),
                        std::mem::size_of_val(&info) as u32,
                        std::ptr::null_mut(),
                    )
                } == 0
                {
                    return Err(format!(
                        "Cannot confirm game job exit: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                if info.ActiveProcesses == 0 {
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(
                        "The previous game process tree has not exited. Save links were retained."
                            .into(),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        #[cfg(not(windows))]
        {
            Err("Process-tree exit cannot be established on this platform.".into())
        }
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

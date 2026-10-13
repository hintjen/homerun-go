//! Interrupting a Windows server the way Ctrl+Break would, from a helper.
//!
//! # What is true, and what was believed
//!
//! This used to say a console control event could not reach a server spawned
//! with piped stdio, so `stop.via: interrupt` was refused on Windows. That is
//! wrong for a **console-subsystem** executable. `job/owned.rs` spawns with
//! `CREATE_NO_WINDOW`, and a console program started that way still gets a
//! console of its own -- a hidden one, holding nothing but the server. Pipes
//! replace its standard handles, not its console.
//!
//! It was established against a real dedicated server whose only clean
//! shutdown is a console interrupt (no console verb, no RCON, no admin API),
//! launched by `homerun-game launch` on Windows 11. A separate process
//! attached to the server's console and generated:
//!
//! - **`CTRL_C_EVENT`**: accepted, and ignored by the server for well over a
//!   minute. A process can be told to ignore Ctrl+C, the setting is inherited
//!   by its children, and a runner started from a background shell has it --
//!   so whether Ctrl+C lands depends on how the runner was started.
//! - **`CTRL_BREAK_EVENT`**: the server ran its quit path, saved its world and
//!   exited in three seconds. Ctrl+Break cannot be ignored that way.
//!
//! So the event is Ctrl+Break.
//!
//! # Why a helper process, and never this one
//!
//! `GenerateConsoleCtrlEvent` signals a console the *caller* is attached to,
//! and every process attached to it -- the caller included. Delivering one
//! means `FreeConsole`, `AttachConsole(server)`, generate. All three are
//! process-wide: the runner is multithreaded and has its own console (or
//! none) that its other threads may be writing to, and the event would reach
//! the runner too. In the experiment above the sending process was ended by
//! its own Ctrl+Break.
//!
//! So a short-lived helper does it, and the helper is whatever the linking
//! binary says it is ([`set_interrupt_helper`]). `homerun-game` registers
//! itself re-run with a hidden verb ([`CONSOLE_BREAK_VERB`]), which ends up in
//! [`console_break`]. This crate is linked by more than one binary --
//! `homerun-game`, `homerun-pumpkin-bin`, and the phones' FFI library -- so it
//! does not assume `current_exe()` understands the verb. A binary that
//! registers nothing cannot interrupt, and says so.
//!
//! The helper installs a handler that swallows every event before it
//! generates one. `SetConsoleCtrlHandler(NULL, TRUE)` is not enough: it
//! ignores Ctrl+C only, and the helper's own Ctrl+Break would end it before
//! it reports.
//!
//! # What still cannot be interrupted
//!
//! A **GUI-subsystem** server has no console at all. `AttachConsole` fails,
//! the helper says so by its exit code, and [`graceful_interrupt`] returns
//! `Err` so the ladder climbs to the terminate. Nothing a descriptor says can
//! tell the two apart, which is why `engine::validate` still warns.

use std::process::Command;
use std::sync::RwLock;

/// The hidden verb a helper binary answers. Not in any `--help`: a person has
/// no reason to type it, and its arguments are exactly one process id.
pub const CONSOLE_BREAK_VERB: &str = "__console-break";

/// Builds the command that interrupts `pid`: the helper binary with
/// [`CONSOLE_BREAK_VERB`] and the pid, or whatever a test stands in for it.
/// `Err` is a sentence for the stop's notes.
pub type InterruptHelper = fn(pid: u32) -> Result<Command, String>;

static HELPER: RwLock<Option<InterruptHelper>> = RwLock::new(None);

/// Name the program that delivers a console interrupt. Called once, early, by
/// a binary that answers [`CONSOLE_BREAK_VERB`]; a later call replaces it.
pub fn set_interrupt_helper(helper: InterruptHelper) {
    *HELPER.write().unwrap_or_else(|p| p.into_inner()) = Some(helper);
}

#[cfg_attr(not(windows), allow(dead_code))]
fn registered() -> Option<InterruptHelper> {
    *HELPER.read().unwrap_or_else(|p| p.into_inner())
}

/// The helper's exit codes. Not `1` or `101`, which a helper that failed in
/// some way nobody planned -- a panic, a loader error -- exits with.
pub mod exit {
    /// The event was generated.
    pub const SENT: i32 = 0;
    /// The arguments were not one process id.
    pub const USAGE: i32 = 64;
    /// The process exists and has no console: a windowed program.
    pub const NO_CONSOLE: i32 = 65;
    /// The helper could not protect itself from the event it is about to send.
    pub const NO_HANDLER: i32 = 66;
    /// Attached, and the event could not be generated.
    pub const NOT_SENT: i32 = 67;
    /// Not Windows. Nothing here calls the helper anywhere else.
    pub const UNSUPPORTED: i32 = 68;
    /// No such process: the server had already gone.
    pub const GONE: i32 = 69;
}

/// `Some` when `args` (without the program name) invoke the helper verb, with
/// the pid or why it is not one; `None` for every other command line, which
/// the binary handles as it always has.
pub fn console_break_args(args: &[String]) -> Option<Result<u32, String>> {
    let (verb, rest) = args.split_first()?;
    if verb != CONSOLE_BREAK_VERB {
        return None;
    }
    Some(match rest {
        [pid] => match pid.parse::<u32>() {
            // Zero is "every process on my console" to the API underneath and
            // `u32::MAX` is "my parent's console" to `AttachConsole`. Neither
            // is ever a server.
            Ok(0) | Ok(u32::MAX) | Err(_) => {
                Err(format!("{CONSOLE_BREAK_VERB} needs a process id."))
            }
            Ok(pid) => Ok(pid),
        },
        _ => Err(format!("{CONSOLE_BREAK_VERB} takes one process id.")),
    })
}

/// What a helper's exit code means, as a sentence for the stop's notes.
#[cfg_attr(not(windows), allow(dead_code))]
fn meaning(code: Option<i32>) -> Result<(), String> {
    match code {
        Some(exit::SENT) => Ok(()),
        Some(exit::NO_CONSOLE) => Err("the server is not a console program, so it cannot be \
             interrupted the way Ctrl+Break would."
            .into()),
        Some(exit::GONE) => Err("the server had already exited.".into()),
        Some(exit::UNSUPPORTED) => {
            Err("this computer cannot interrupt a server the way Ctrl+Break would.".into())
        }
        Some(exit::NO_HANDLER) | Some(exit::NOT_SENT) => {
            Err("the server's console did not accept the interrupt.".into())
        }
        _ => Err("the program that interrupts the server failed.".into()),
    }
}

/// Ask a Windows server to stop the way Ctrl+Break in its console would.
///
/// `Ok` means asked; whether it goes is the ladder's wait to find out.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn graceful_interrupt(pid: u32) -> Result<(), String> {
    interrupt_with(registered(), pid)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn interrupt_with(helper: Option<InterruptHelper>, pid: u32) -> Result<(), String> {
    if pid == 0 || pid == u32::MAX {
        return Err("the server has no process to interrupt.".into());
    }
    let Some(helper) = helper else {
        return Err("this program cannot interrupt a server the way Ctrl+Break would.".into());
    };
    imp::refuse_shared_console(pid)?;
    let mut command = helper(pid)?;
    imp::hide(&mut command);
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| "the program that interrupts the server could not be started.".to_string())?;
    // It does three system calls and exits. A helper that has not finished in
    // this long is stuck, and the ladder has a grace to get on with.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return meaning(status.code()),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the program that interrupts the server did not finish.".into());
            }
        }
    }
}

/// The helper's whole job: attach to `pid`'s console and send Ctrl+Break to
/// everything on it. Returns the exit code. Run only in a process that exists
/// for this -- see the module note for why.
pub fn console_break(pid: u32) -> i32 {
    imp::console_break(pid)
}

#[cfg(windows)]
mod imp {
    use super::exit;
    use std::process::Command;
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_INVALID_PARAMETER, FALSE, TRUE},
        System::Console::{
            AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleProcessList,
            SetConsoleCtrlHandler, CTRL_BREAK_EVENT,
        },
    };

    pub fn hide(command: &mut Command) {
        use std::os::windows::process::CommandExt;
        // The helper's own console, which it frees at once. Without this a
        // runner with no console would flash a window for it.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    /// A server sharing *this* process's console would take the event with
    /// this process on it. `owned.rs` never spawns one that way, but nothing
    /// else stops a future spawn path from doing so, and the cost of being
    /// wrong is the runner interrupting itself.
    pub fn refuse_shared_console(pid: u32) -> Result<(), String> {
        let mut ids = vec![0u32; 64];
        // SAFETY: a buffer of `len` u32s. Zero means this process has no
        // console, which shares nothing; a count above `len` means it did
        // not fit, and the call is repeated with room.
        let mut count = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
        if count as usize > ids.len() {
            ids.resize(count as usize, 0);
            count = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
        }
        let count = (count as usize).min(ids.len());
        if ids[..count].contains(&pid) {
            return Err(
                "the server shares this program's console, so interrupting it would \
                 interrupt this program too."
                    .into(),
            );
        }
        Ok(())
    }

    unsafe extern "system" fn swallow(_event: u32) -> i32 {
        TRUE
    }

    pub fn console_break(pid: u32) -> i32 {
        // SAFETY: plain Win32 calls with no pointers but a valid fn pointer.
        // The handler goes in first: the event reaches this process as soon
        // as it is generated, and the default handler would end it.
        unsafe {
            if SetConsoleCtrlHandler(Some(swallow), TRUE) == FALSE {
                return exit::NO_HANDLER;
            }
            FreeConsole();
            if AttachConsole(pid) == FALSE {
                return if GetLastError() == ERROR_INVALID_PARAMETER {
                    exit::GONE
                } else {
                    exit::NO_CONSOLE
                };
            }
            // Group 0: every process on that console -- the server, anything
            // it started there, and this helper, whose handler swallows it.
            if GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) == FALSE {
                return exit::NOT_SENT;
            }
        }
        exit::SENT
    }
}

#[cfg(not(windows))]
mod imp {
    use super::exit;
    use std::process::Command;

    // Reached only through `interrupt_with`, which off Windows nothing calls:
    // `platform::graceful_interrupt` is `SIGINT` there.
    #[allow(dead_code)]
    pub fn hide(_command: &mut Command) {}

    #[allow(dead_code)]
    pub fn refuse_shared_console(_pid: u32) -> Result<(), String> {
        Ok(())
    }

    pub fn console_break(_pid: u32) -> i32 {
        exit::UNSUPPORTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn every_other_command_line_is_left_alone() {
        for line in [
            &[][..],
            &["supervise"][..],
            &["launch", "valheim"][..],
            &["--features"][..],
            &["console-break", "12"][..],
        ] {
            assert!(console_break_args(&args(line)).is_none(), "{line:?}");
        }
    }

    #[test]
    fn the_verb_takes_exactly_one_process_id() {
        assert_eq!(
            console_break_args(&args(&[CONSOLE_BREAK_VERB, "4242"])),
            Some(Ok(4242))
        );
        for bad in [
            &[CONSOLE_BREAK_VERB][..],
            &[CONSOLE_BREAK_VERB, "4242", "7"][..],
            &[CONSOLE_BREAK_VERB, "-1"][..],
            &[CONSOLE_BREAK_VERB, "server"][..],
            &[CONSOLE_BREAK_VERB, "99999999999"][..],
        ] {
            let parsed = console_break_args(&args(bad)).expect("the verb was given");
            assert!(parsed.unwrap_err().ends_with('.'), "{bad:?}");
        }
    }

    /// Zero means "every process on my console" to `GenerateConsoleCtrlEvent`
    /// and `u32::MAX` means "my parent's console" to `AttachConsole`. Neither
    /// is ever a server's pid, and either would interrupt the wrong thing.
    #[test]
    fn the_pids_the_api_reads_as_wildcards_are_refused() {
        for pid in ["0", "4294967295"] {
            assert!(
                matches!(
                    console_break_args(&args(&[CONSOLE_BREAK_VERB, pid])),
                    Some(Err(_))
                ),
                "{pid}"
            );
            assert!(interrupt_with(None, pid.parse().unwrap()).is_err());
        }
    }

    #[test]
    fn every_exit_code_is_a_sentence_and_only_sent_is_ok() {
        assert_eq!(meaning(Some(exit::SENT)), Ok(()));
        for code in [
            Some(exit::USAGE),
            Some(exit::NO_CONSOLE),
            Some(exit::NO_HANDLER),
            Some(exit::NOT_SENT),
            Some(exit::UNSUPPORTED),
            Some(exit::GONE),
            Some(1),
            Some(101),
            None,
        ] {
            let err = meaning(code).unwrap_err();
            assert!(err.ends_with('.'), "{code:?}: {err}");
        }
    }

    /// A binary that never named a helper -- the Pumpkin binary, the phones'
    /// library -- refuses rather than running itself with a verb it does not
    /// know.
    #[test]
    fn with_no_helper_there_is_no_interrupt() {
        let err = interrupt_with(None, std::process::id()).unwrap_err();
        assert!(err.contains("Ctrl+Break"), "{err}");
    }

    #[cfg(not(windows))]
    #[test]
    fn the_helper_refuses_off_windows() {
        assert_eq!(console_break(std::process::id()), exit::UNSUPPORTED);
    }

    // ─── Windows: the real thing ─────────────────────────────────────────

    /// This test binary, re-run as the helper. libtest owns the command line,
    /// so the pid travels in the environment and the test calls the same
    /// `console_break` the `homerun-game` verb does.
    #[cfg(windows)]
    fn test_helper(pid: u32) -> Result<Command, String> {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "platform::console_interrupt::tests::i_am_the_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("HOMERUN_TEST_BREAK_PID", pid.to_string());
        Ok(command)
    }

    #[test]
    fn i_am_the_helper() {
        let Ok(pid) = std::env::var("HOMERUN_TEST_BREAK_PID") else {
            return;
        };
        std::process::exit(console_break(pid.parse().unwrap()));
    }

    /// A stand-in server. With `HOMERUN_TEST_BREAK_SERVER` it waits on stdin
    /// for a Ctrl+Break, says it saved, and exits 0.
    #[cfg(windows)]
    #[test]
    fn i_am_the_server() {
        if std::env::var("HOMERUN_TEST_BREAK_SERVER").is_err() {
            return;
        }
        use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT};
        unsafe extern "system" fn on_event(event: u32) -> i32 {
            if event == CTRL_BREAK_EVENT {
                use std::io::Write;
                println!("fixture: saved on break");
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            }
            0
        }
        // SAFETY: a valid handler for the life of the process.
        unsafe { SetConsoleCtrlHandler(Some(on_event), 1) };
        println!("fixture: ready");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(30));
        std::process::exit(3);
    }

    /// The server is spawned exactly as `job/owned.rs` spawns a game --
    /// `CREATE_NO_WINDOW`, piped stdio, inside a job -- and stopped by the
    /// interrupt alone.
    #[cfg(windows)]
    #[test]
    fn a_console_server_spawned_like_a_game_saves_and_exits_on_the_interrupt() {
        use std::io::{BufRead, BufReader};
        let job = crate::job::Job::required_process().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "platform::console_interrupt::tests::i_am_the_server",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("HOMERUN_TEST_BREAK_SERVER", "1");
        let mut server = job.spawn(&command).unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        let stdout = server.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let wait_for = |want: &str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut seen = Vec::new();
            while let Ok(line) =
                lines.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                // libtest prints "test ... " with no newline before the first line.
                if line.ends_with(want) {
                    return;
                }
                seen.push(line);
            }
            panic!("never saw {want:?}; saw {seen:?}");
        };
        wait_for("fixture: ready");

        let asked = std::time::Instant::now();
        interrupt_with(Some(test_helper), server.id()).expect("the interrupt was not sent");
        wait_for("fixture: saved on break");
        let status = loop {
            if let Some(status) = server.try_wait().unwrap() {
                break status;
            }
            assert!(
                asked.elapsed() < std::time::Duration::from_secs(5),
                "the server did not exit on the interrupt"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(status.code(), Some(0), "the server did not exit cleanly");
    }

    /// A windowed server has no console. Started detached, this test binary is
    /// in the same position, and the interrupt has to fail in a sentence so
    /// the ladder climbs to the terminate.
    #[cfg(windows)]
    #[test]
    fn a_process_with_no_console_cannot_be_interrupted() {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        let mut target = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::console_interrupt::tests::i_am_the_server",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("HOMERUN_TEST_BREAK_SERVER", "1")
            .creation_flags(DETACHED_PROCESS)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let result = interrupt_with(Some(test_helper), target.id());
        let _ = target.kill();
        let _ = target.wait();
        let err = result.unwrap_err();
        assert!(err.contains("not a console program"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn a_process_that_does_not_exist_is_said_to_have_gone() {
        // Windows pids are multiples of four, so this one never exists.
        let err = interrupt_with(Some(test_helper), 4_294_967_294).unwrap_err();
        assert!(err.contains("already exited"), "{err}");
    }
}

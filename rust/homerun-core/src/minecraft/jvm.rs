//! Starting and stopping a Java Minecraft server.
//!
//! Reference: `supervisor.js` — its `spawn` arguments and its `stopServer`
//! escalation.
//!
//! # What is here and what is not
//!
//! A JVM command line is half portable and half not. How much heap to give it,
//! that `-Xms` matches `-Xmx`, and that Minecraft's own main wants `nogui` are
//! true wherever the server runs. Which `libjvm.so` to `dlopen`, what
//! `LD_LIBRARY_PATH` must contain for a Termux-built runtime, and where a
//! temp directory may live are Android's alone, and none of that is here.
//!
//! The dividing line is the same one the rest of this crate draws: a decision
//! is shared, an effect is the platform's. Spawning is an effect. *How long to
//! wait before deciding a JVM is wedged* is not.

use serde::{Deserialize, Serialize};

/// The least heap worth starting with. Below this a world generates so slowly
/// that it reads as a hang.
pub const MIN_HEAP_MB: u32 = 512;

/// What a host means by asking for no particular heap.
///
/// Only reachable where there is no device ceiling either — a desktop that
/// said nothing. A phone that says nothing gets what its RAM allows, which is
/// a better answer than a fixed number.
const DEFAULT_HEAP_MB: u32 = 1024;

/// How much heap the JVM actually gets.
///
/// `device_total_mb` is `None` where there is no device ceiling to apply — a
/// desktop, which gives the player what they asked for.
///
/// On a phone the ceiling is a third of physical RAM, and it is not a
/// performance tuning: **Android kills the whole app under memory pressure,
/// not just the server**, so an over-generous heap does not cost you a server,
/// it costs you the app that was hosting it, mid-save.
///
/// **A `requested_mb` of zero means the host did not say**, the same
/// convention `port` uses across this crate's C surface — and it is not a
/// request for no memory at all. Read literally it clamps to [`MIN_HEAP_MB`],
/// which is how a 2.4 GB device ended up running a server on 512 MB while
/// 824 was available and safe. Silence gets the ceiling instead.
pub fn heap_mb(requested_mb: u32, device_total_mb: Option<u32>) -> u32 {
    let ceiling = device_total_mb
        .map(|total| (total / 3).max(MIN_HEAP_MB))
        .unwrap_or(u32::MAX);

    if requested_mb == 0 {
        return ceiling.min(device_total_mb.map_or(DEFAULT_HEAP_MB, |_| ceiling));
    }
    requested_mb.clamp(MIN_HEAP_MB, ceiling)
}

/// The JVM flags every platform passes, in order.
///
/// `-Xms` equals `-Xmx` deliberately. A server that grows its heap pauses to
/// do it, and on a device where the ceiling is already a third of RAM there is
/// nothing to be gained by starting lower.
pub fn heap_options(heap_mb: u32) -> Vec<String> {
    vec![format!("-Xmx{heap_mb}M"), format!("-Xms{heap_mb}M")]
}

/// What Minecraft's own main takes. No GUI on a server, and on a phone there
/// is not even a display to open one on.
pub const PROGRAM_ARGS: &[&str] = &["nogui"];

/// Written on every launch. There is no acceptance step anywhere in the
/// product and the server refuses to boot without it.
pub const EULA_FILE: &str = "eula.txt";
pub const EULA_CONTENTS: &str = "eula=true\n";

/// What PowerNukkitX's own main takes, and **every one of these is
/// load-bearing**.
///
/// Without `--skip-setup` a first boot runs an interactive setup wizard that
/// reads a language, a port and a gamemode off stdin. Without
/// `--accept-license` — even *with* `--skip-setup` — it still prints the LGPL
/// and waits for an answer. Either way a phone sits at `starting` forever with
/// a healthy process that will never announce itself. Read out of
/// `PowerNukkitX.java`, which branches on exactly these two.
///
/// `--disable-ansi` because the console is a pipe, not a terminal, and colour
/// codes in a buffer the UI renders are noise the parser then has to strip.
pub const NUKKIT_PROGRAM_ARGS: &[&str] = &[
    "--skip-setup",
    "--accept-license",
    "--disable-ansi",
    "--language",
    "eng",
];

/// Forced off at the command line as well as in the config.
///
/// PowerNukkitX ships Sentry auto bug reporting **on**. A player's phone does
/// not send crash reports to a third party, and a config key alone is not a
/// guarantee — a world restore can bring another device's config file with it.
pub const NUKKIT_JVM_OPTIONS: &[&str] = &["-DdisableSentry=true"];

/// What a host does to a JVM at one rung of the stop ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    /// Send [`STOP_COMMAND`] on the console. The only shutdown that saves.
    Console,
    /// Ask the process to exit — `SIGTERM`, or `Process.destroy`.
    Terminate,
    /// Take it out. `SIGKILL`, or `destroyForcibly`.
    Kill,
}

/// One rung: do this, then wait this long for the process to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rung {
    pub action: Action,
    /// How long to wait before climbing to the next rung. Zero on the last —
    /// there is nothing above it.
    pub wait_ms: u64,
}

/// The console command that shuts a Minecraft server down properly.
pub const STOP_COMMAND: &str = "stop";

/// How to stop a running JVM, in the order to try it.
///
/// `console` is false when there is nothing listening on stdin yet — a server
/// stopped while it was still booting.
///
/// # Why the first rung exists at all
///
/// `stop` saves the world *and* every online player's data, then exits. A
/// terminate skips that: on Windows it is `TerminateProcess`, which ends the
/// JVM without running its shutdown hook, and the world is never flushed — so
/// the on-stop backup captures a stale auto-save with, say, an out-of-date
/// inventory. The desktop learned that one the expensive way and its comment
/// says so.
///
/// # Why the waits are what they are
///
/// Thirty seconds is not how long a stop takes. A real save finishes well
/// inside it and the process exits then; the wait is a safety net for a wedged
/// JVM, sized so that a large world's save is never cut short. Eight seconds
/// after a terminate is the same idea with far less to do.
pub fn stop_ladder(console: bool) -> Vec<Rung> {
    let mut ladder = Vec::new();
    if console {
        ladder.push(Rung {
            action: Action::Console,
            wait_ms: 30_000,
        });
    }
    ladder.push(Rung {
        action: Action::Terminate,
        wait_ms: 8_000,
    });
    ladder.push(Rung {
        action: Action::Kill,
        wait_ms: 0,
    });
    ladder
}

/// How long a launch waits for the things it cannot hurry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    /// From spawning to the console saying it is accepting connections.
    ///
    /// Generous because a first launch generates a world, and a phone is slow
    /// at it. The cap exists so a wedged JVM reports rather than hangs for
    /// ever; it is not a target.
    pub start_timeout_ms: u64,
    /// How long a *restart* waits for the outgoing JVM before refusing.
    ///
    /// Refusing is the right failure: two JVMs in one server directory means
    /// two worlds writing over each other, which is worse than a start that
    /// did not happen.
    pub previous_exit_wait_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            start_timeout_ms: 300_000,
            previous_exit_wait_ms: 120_000,
        }
    }
}

/// Something a host could not do, worded for the player who asked.
///
/// Here rather than in each host for the same reason every other verdict in
/// this crate is: two apps refusing the same thing should say the same words.
/// Do not reword these at the call site — change them here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Refusal {
    /// A restart whose predecessor will not exit.
    PreviousServerBusy,
    /// The console never reported ready inside [`Limits::start_timeout_ms`].
    StartTimedOut,
    /// The jar's manifest names no `Main-Class`, so there is nothing to run.
    NoMainClass,
    /// A console command arrived with no console to take it.
    NotAcceptingCommands,
    /// This build ships no Java launcher.
    NoJavaRuntime,
    /// The runtime is present but incomplete — an interrupted unpack.
    BrokenJavaRuntime,
}

impl Refusal {
    pub fn text(self) -> &'static str {
        match self {
            Refusal::PreviousServerBusy => {
                "The previous server is still shutting down. Try again in a moment."
            }
            Refusal::StartTimedOut => "The server did not finish starting in time.",
            Refusal::NoMainClass => "That server jar has no Main-Class, so it cannot be started.",
            Refusal::NotAcceptingCommands => "The server is not accepting commands.",
            Refusal::NoJavaRuntime => {
                "This build has no Java launcher, so it cannot host a Java server."
            }
            Refusal::BrokenJavaRuntime => "The Java runtime is incomplete.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- heap --------------------------------------------------------------

    #[test]
    fn a_phone_never_gives_away_more_than_a_third_of_itself() {
        // 6 GB device, 4 GB asked for: a third, because the alternative is
        // Android killing the app that is hosting.
        assert_eq!(heap_mb(4096, Some(6144)), 2048);
    }

    #[test]
    fn what_was_asked_for_is_honoured_when_it_fits() {
        assert_eq!(heap_mb(1024, Some(6144)), 1024);
    }

    #[test]
    fn no_device_ceiling_means_the_player_gets_what_they_asked_for() {
        // The desktop, which has no reason to second-guess this.
        assert_eq!(heap_mb(8192, None), 8192);
    }

    #[test]
    fn the_floor_wins_over_a_ceiling_that_would_undercut_it() {
        // A 1 GB device: a third is 341 MB, which no server should start on.
        // Better to try and be killed than to refuse to start at all.
        assert_eq!(heap_mb(256, Some(1024)), MIN_HEAP_MB);
        assert_eq!(heap_mb(4096, Some(1024)), MIN_HEAP_MB);
    }

    /// The bug this convention exists to prevent: a host that sends nothing
    /// is not asking for as little as possible.
    #[test]
    fn asking_for_nothing_gets_what_the_device_can_afford() {
        // 2472 MB device: a third is 824, and 824 is what it should run on —
        // not the 512 floor a literal reading of zero produces.
        assert_eq!(heap_mb(0, Some(2472)), 824);
        // No ceiling to work from, so a fixed default rather than everything.
        assert_eq!(heap_mb(0, None), DEFAULT_HEAP_MB);
        // And a genuinely tiny device still cannot go below the floor.
        assert_eq!(heap_mb(0, Some(1024)), MIN_HEAP_MB);
    }

    #[test]
    fn xms_matches_xmx() {
        assert_eq!(heap_options(1024), vec!["-Xmx1024M", "-Xms1024M"]);
    }

    // --- stopping ----------------------------------------------------------

    #[test]
    fn a_stop_asks_before_it_terminates_and_terminates_before_it_kills() {
        let ladder = stop_ladder(true);
        assert_eq!(
            ladder.iter().map(|r| r.action).collect::<Vec<_>>(),
            vec![Action::Console, Action::Terminate, Action::Kill]
        );
        // The save gets the long window; nothing waits after the kill.
        assert_eq!(ladder[0].wait_ms, 30_000);
        assert_eq!(ladder[2].wait_ms, 0);
    }

    #[test]
    fn with_no_console_there_is_nothing_to_ask_so_it_starts_at_terminate() {
        let ladder = stop_ladder(false);
        assert_eq!(
            ladder.iter().map(|r| r.action).collect::<Vec<_>>(),
            vec![Action::Terminate, Action::Kill]
        );
    }

    #[test]
    fn every_ladder_ends_somewhere_it_cannot_be_ignored() {
        for console in [true, false] {
            let ladder = stop_ladder(console);
            assert_eq!(ladder.last().unwrap().action, Action::Kill);
            assert_eq!(ladder.last().unwrap().wait_ms, 0);
        }
    }

    // --- wording -----------------------------------------------------------

    #[test]
    fn every_refusal_is_a_sentence_a_player_could_read() {
        for refusal in [
            Refusal::PreviousServerBusy,
            Refusal::StartTimedOut,
            Refusal::NoMainClass,
            Refusal::NotAcceptingCommands,
            Refusal::NoJavaRuntime,
            Refusal::BrokenJavaRuntime,
        ] {
            let text = refusal.text();
            assert!(text.ends_with('.'), "{refusal:?}: {text}");
            assert!(
                text.chars().next().is_some_and(char::is_uppercase),
                "{refusal:?}: {text}"
            );
            // No jargon that only means something to whoever wrote it.
            for word in ["null", "errno", "exception", "SIGKILL"] {
                assert!(!text.contains(word), "{refusal:?} leaks {word}");
            }
        }
    }
}

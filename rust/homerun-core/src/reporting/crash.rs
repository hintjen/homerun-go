//! Why a server died — the local read of its last words, and the report that
//! goes to the API.
//!
//! Reference: `nativeServerManager.ts` — the `server-crashed` branch (its
//! `isCorruptJar` and `isPortConflict` checks, and the auto-restart between
//! them) and `reportCrash`.
//!
//! # Two answers to "why", and only one of them is fast
//!
//! [`crate::state::exit_state`] already decides *whether* an exit was a crash.
//! This decides **why**, twice over:
//!
//!  - [`report`] hands the console to `/api/service-error/`, which matches it
//!    against the KnownError table and answers with a `user_facing_message`.
//!    That table is edited without shipping an app, so it is the better
//!    answer — when it arrives. It needs a round trip and a network.
//!  - [`diagnose`] reads the same lines here and now. A phone tethered to the
//!    server it is hosting is often the phone with no other connectivity, and
//!    a player staring at "crashed" with no reason is the failure this
//!    prevents. It also runs *before* the report, because one of its answers
//!    is not a message at all — it is "do not tell the player anything yet,
//!    fix it and start again".
//!
//! Both, in that order, is what the desktop does. Neither replaces the other:
//! a host shows the local message immediately and the API's when it lands.
//!
//! # The retry is the part worth being careful about
//!
//! A corrupt launch jar is repairable — delete it, download it again, start
//! again — and the desktop repairs it silently. It also caps that at one
//! attempt, because a mod loader whose installer produces nothing looks
//! identical from the log, and each retry re-runs the same failing install.
//! Uncapped, that is an infinite loop; on a phone it is an infinite loop
//! holding a wake lock and a data connection.
//!
//! So the budget is a parameter here rather than a counter in a host. Two
//! hosts, two counters, and the one that forgot to reset it on a good launch
//! never retries while the one that forgot to increment it never stops. The
//! host keeps only the number; this decides what it means.
//!
//! ```text
//!   host: the console it captured, and how many retries it has already spent
//!   core: diagnose(…)  →  a cause, a message for the player, and what to do
//!   host: retry, or show the message — then report(…) either way
//! ```
//!
//! "Either way" is a deliberate difference from the desktop, which returns
//! early on the retry path and so never reports the crash it repaired. A
//! server whose download is corrupt every single time then looks, from the
//! API's side, like a server nobody ever started.

use super::Request;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// How many times a host may relaunch after a corrupt jar before it must give
/// up and say so. The desktop's `corruptRestarts < 1`.
///
/// The count is per **server** and belongs to the host, which resets it when a
/// launch reaches running — the desktop does that in `onServerFullyRunning`.
/// A budget that is never reset turns the second corrupt download of a
/// server's life into a dead end.
pub const CORRUPT_JAR_RETRY_BUDGET: u32 = 1;

/// The most console this crate will put in one report.
///
/// The desktop caps the *buffer* it keeps (`SESSION_LOG_LIMIT = 2000`) and so
/// never has to cap the request. A host here may hold a deeper console than
/// that, and a modded server logs tens of thousands of lines an hour — posting
/// all of it over a phone's data to explain a crash is a cost the player did
/// not agree to. The tail is the half that says what happened.
pub const MAX_REPORTED_LINES: usize = 2000;

/// And the most bytes, whatever those lines turn out to weigh.
///
/// A line count is not a size. One mod dumping a serialised world state, or a
/// stack trace arriving as a single line, makes 2000 lines arbitrarily large —
/// and the one thing a crash report has to do is *arrive*, over whatever
/// connection the phone happens to have. Same rule, and the same reason, as
/// the device websocket's `get-app-logs`: keep the tail, cut on a line
/// boundary, because half a line reads as a different message.
pub const MAX_REPORTED_BYTES: usize = 128 * 1024;

/// What the log says went wrong.
///
/// Not a classification of every crash — most crashes land as `None` and wait
/// for the API. These are the ones worth acting on locally: they either have a
/// fix the player can carry out, or a fix the host can carry out for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Cause {
    /// The launch jar is damaged or missing. Repairable, hence the budget.
    CorruptJar,
    /// Something else holds the port. Minecraft exits 0 for this, so without a
    /// cause it reads as a server that started and then simply vanished.
    PortInUse,
    /// The EULA was never accepted, so the server refused to run.
    EulaNotAccepted,
    /// The JVM could not reserve the heap it was asked for.
    HeapUnavailable,
    /// The server exhausted the heap it did get.
    OutOfMemory,
    /// A mod or plugin threw during initialisation.
    ModInitFailed,
    /// Unbounded recursion, which in practice means a mod.
    StackOverflow,
}

/// What the host should do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Recovery {
    /// Show the message and stay stopped.
    Report,
    /// Delete the launch jar, fetch it again, and start again. The message is
    /// a progress note, not an error — the player has not lost anything yet.
    ///
    /// A host that *cannot* relaunch (the desktop needs the start arguments it
    /// stashed, and drops this if they are gone) may downgrade this to
    /// [`Recovery::Report`]. Nothing may upgrade in the other direction.
    RedownloadAndRestart,
}

/// The local read of a crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnosis {
    pub cause: Cause,
    /// Written for the player who was hosting, not for whoever reads the logs
    /// afterwards. Carries no `[Homerun]` prefix — the desktop's does because
    /// it is printed into the console; a host that prints it there adds its
    /// own, and a host that puts it in a banner should not have to strip one.
    pub message: String,
    pub recovery: Recovery,
}

/// Said while a corrupt jar is being replaced, in place of the cause's own
/// message — nothing has failed from the player's side yet.
const REDOWNLOADING: &str =
    "The server's files were damaged. Downloading them again and restarting.";

impl Cause {
    /// Checked in this order, and the first hit wins.
    ///
    /// Corrupt jar leads because it is the only one that can be repaired, and
    /// because the desktop only looks for a port conflict once it has ruled it
    /// out. The specific causes precede [`Cause::ModInitFailed`] because
    /// "Failed to start the minecraft server" is printed *after* whatever
    /// actually failed — matching it first would replace a real reason with a
    /// restatement of the crash.
    const ORDER: [Cause; 7] = [
        Cause::CorruptJar,
        Cause::PortInUse,
        Cause::EulaNotAccepted,
        Cause::HeapUnavailable,
        Cause::OutOfMemory,
        Cause::StackOverflow,
        Cause::ModInitFailed,
    ];

    /// Substrings, matched case-sensitively, exactly as the desktop's
    /// `line.includes(…)` does. Case-sensitive on purpose: `Address already in
    /// use` is text the JVM emits verbatim, and loosening it is how a chat
    /// message ends up diagnosing a crash.
    fn patterns(self) -> &'static [&'static str] {
        match self {
            Cause::CorruptJar => &[
                "Invalid or corrupt jarfile",
                "Error: Unable to access jarfile",
            ],
            Cause::PortInUse => &[
                "FAILED TO BIND TO PORT",
                "Address already in use",
                "Perhaps a server is already running on that port",
            ],
            // Not in the desktop, which writes `eula=true` itself at install
            // and so believes this cannot happen. It can here: a world folder
            // imported from elsewhere, or restored from a backup taken before
            // the file existed, arrives without it and the server exits in two
            // lines that mean nothing to a player.
            Cause::EulaNotAccepted => &["agree to the EULA"],
            // Also not in the desktop, and the one a phone hits that a PC does
            // not: the heap is sized from the device's RAM, and a device that
            // has less to spare than it advertised fails here before a single
            // server line is printed. Distinct from OutOfMemory because the
            // advice is the opposite — ask for less, not more.
            Cause::HeapUnavailable => &[
                "Could not reserve enough space for object heap",
                "Error occurred during initialization of VM",
            ],
            // These three are the desktop's, from the Minecraft *client*
            // launcher's CRASH_PATTERNS in `ipcHandler.ts` — server-side lines
            // in a client-side table. Ported to where they belong; the two
            // display/GPU patterns beside them are genuinely client-only and
            // stayed behind.
            Cause::OutOfMemory => &["OutOfMemoryError"],
            Cause::StackOverflow => &["StackOverflowError"],
            Cause::ModInitFailed => &["Failed to start the minecraft server"],
        }
    }

    /// What a player is told. See [`Diagnosis::message`].
    fn message(self) -> &'static str {
        match self {
            // The desktop's "Server launcher could not be built — the mod
            // loader did not install correctly", said without the words
            // "launcher" and "mod loader" doing the explaining.
            Cause::CorruptJar => {
                "The server's files could not be prepared, even after downloading them again. \
                 Reinstall this server to fix it."
            }
            Cause::PortInUse => {
                "The server could not start because its port is already in use. \
                 Make sure no other Minecraft server is running, then try again."
            }
            Cause::EulaNotAccepted => {
                "The server stopped because Minecraft's end user licence agreement has not been \
                 accepted for it. Accept it in the server's settings, then start it again."
            }
            Cause::HeapUnavailable => {
                "This device could not spare as much memory as the server asked for. \
                 Lower the server's memory in its settings, then try again."
            }
            Cause::OutOfMemory => {
                "The server ran out of memory. Give it more memory in its settings, or lower the \
                 view distance, then try again."
            }
            Cause::StackOverflow => {
                "The server got stuck in a loop it could not escape. A mod or plugin is the \
                 likely cause."
            }
            Cause::ModInitFailed => {
                "A mod or plugin failed to load, so the server stopped. \
                 Check that everything installed is made for this game version."
            }
        }
    }
}

/// Read a crashed run's console.
///
/// `retries_used` is how many times this **server** has already been
/// relaunched for a corrupt jar since it last reached running. `None` means
/// nothing here recognised the crash, which is the common case and not a
/// failure — the host reports it and shows whatever the API answers.
pub fn diagnose<S: AsRef<str>>(lines: &[S], retries_used: u32) -> Option<Diagnosis> {
    let cause = Cause::ORDER
        .iter()
        .copied()
        .find(|cause| matches(lines, cause.patterns()))?;

    let retrying = cause == Cause::CorruptJar && retries_used < CORRUPT_JAR_RETRY_BUDGET;

    Some(Diagnosis {
        cause,
        message: if retrying {
            REDOWNLOADING.to_string()
        } else {
            cause.message().to_string()
        },
        recovery: if retrying {
            Recovery::RedownloadAndRestart
        } else {
            Recovery::Report
        },
    })
}

/// The crash report. Device-signed: a crash is a fact about a machine.
///
/// The API answers with a serialised ServiceErrorReport carrying its own
/// `user_facing_message`, which is worth forwarding to the UI — it is the
/// half of the diagnosis this crate cannot ship a fix to.
pub fn report<S: AsRef<str>>(server_id: &str, device_id: &str, lines: &[S]) -> Request {
    let from = lines.len().saturating_sub(MAX_REPORTED_LINES);
    let output = lines[from..]
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("\n");
    let output = tail_bytes(output);

    Request::post(
        "/api/service-error/",
        json!({
            "service": server_id,
            "device": device_id,
            "output": output,
        }),
    )
}

/// The last [`MAX_REPORTED_BYTES`], cut on a line boundary.
///
/// The scan for the boundary starts from a **char** boundary, not from the
/// byte offset itself. A Minecraft console carries UTF-8 — `§` colour codes,
/// player names, a MOTD — and slicing a `String` mid-character is one of the
/// few things in safe Rust that panics outright.
fn tail_bytes(text: String) -> String {
    if text.len() <= MAX_REPORTED_BYTES {
        return text;
    }
    let mut start = text.len() - MAX_REPORTED_BYTES;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let cut = text[start..]
        .find('\n')
        .map(|offset| start + offset + 1)
        .unwrap_or(start);
    format!("[earlier lines dropped]\n{}", &text[cut..])
}

fn matches<S: AsRef<str>>(lines: &[S], patterns: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line = line.as_ref();
        patterns.iter().any(|pattern| line.contains(pattern))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::{Auth, Method};

    /// A run that ended the way most runs end. Nothing here is a cause.
    const CLEAN_SHUTDOWN: &[&str] = &[
        "[20:37:28] [Server thread/INFO]: Done (3.244s)! For help, type \"help\"",
        "[20:41:02] [Server thread/INFO]: Notch joined the game",
        "[21:02:11] [Server thread/INFO]: Notch left the game",
        "[21:02:14] [Server thread/INFO]: Stopping the server",
        "[21:02:14] [Server thread/INFO]: Saving players",
        "[21:02:14] [Server thread/INFO]: Saving worlds",
        "[21:02:15] [Server thread/INFO]: Saving chunks for level 'ServerLevel[world]'/minecraft:overworld",
        "[21:02:16] [Server thread/INFO]: ThreadedAnvilChunkStorage: All dimensions are saved",
    ];

    // --- the retry budget --------------------------------------------------

    /// The boundary the whole budget exists for. Below it the host fixes the
    /// problem itself; at it, the player is told — and told something other
    /// than "restarting", because nothing is going to restart.
    #[test]
    fn a_corrupt_jar_is_repaired_once_and_then_explained() {
        let log = ["Error: Invalid or corrupt jarfile server.jar"];

        let first = diagnose(&log, 0).expect("a corrupt jar is recognised");
        assert_eq!(first.recovery, Recovery::RedownloadAndRestart);
        assert_eq!(first.cause, Cause::CorruptJar);

        let spent = diagnose(&log, CORRUPT_JAR_RETRY_BUDGET).expect("still a corrupt jar");
        assert_eq!(
            spent.recovery,
            Recovery::Report,
            "the budget ran out and this still asked the host to restart — \
             a loader that installs nothing crashes identically, so this loops forever"
        );
        assert_ne!(
            spent.message, first.message,
            "the player was told the server is restarting when it is not: {:?}",
            spent.message
        );
    }

    /// A host that lost count must not get an unbounded retry out of it.
    #[test]
    fn a_budget_already_overspent_never_asks_for_another_go() {
        for spent in [CORRUPT_JAR_RETRY_BUDGET, 2, 9, u32::MAX] {
            let diagnosis = diagnose(&["Error: Unable to access jarfile /data/server.jar"], spent)
                .expect("a missing jarfile is recognised");
            assert_eq!(
                diagnosis.recovery,
                Recovery::Report,
                "{spent} retries already spent and it asked for one more"
            );
        }
    }

    /// The desktop only looks for a port conflict once it has ruled out a
    /// corrupt jar, so a repairable crash is never reported as one the player
    /// has to act on. Both lines appear together whenever a relaunch races the
    /// dying process.
    #[test]
    fn a_repairable_crash_outranks_one_the_player_would_have_to_fix() {
        let log = [
            "[12:00:00] [main/ERROR]: FAILED TO BIND TO PORT!",
            "Error: Invalid or corrupt jarfile server.jar",
        ];
        let diagnosis = diagnose(&log, 0).unwrap();
        assert_eq!(diagnosis.cause, Cause::CorruptJar);
        assert_eq!(diagnosis.recovery, Recovery::RedownloadAndRestart);
    }

    // --- the causes --------------------------------------------------------

    /// Minecraft exits *cleanly* when it cannot bind, so with no cause this
    /// reads as a server that started fine and then quietly disappeared.
    #[test]
    fn every_way_the_jvm_says_the_port_is_taken() {
        for line in [
            "[12:00:00] [Server thread/WARN]: **** FAILED TO BIND TO PORT!",
            "[12:00:00] [Server thread/WARN]: The exception was: java.net.BindException: Address already in use",
            "[12:00:00] [Server thread/WARN]: Perhaps a server is already running on that port?",
        ] {
            let diagnosis = diagnose(&[line], 0).unwrap_or_else(|| panic!("unread: {line}"));
            assert_eq!(diagnosis.cause, Cause::PortInUse);
            assert_eq!(diagnosis.recovery, Recovery::Report);
        }
    }

    #[test]
    fn a_server_that_never_had_its_eula_accepted_says_so() {
        let log = [
            "[12:00:00] [main/WARN]: Failed to load eula.txt",
            "[12:00:00] [main/INFO]: You need to agree to the EULA in order to run the server. Go to eula.txt for more info.",
        ];
        let diagnosis = diagnose(&log, 0).expect("the EULA refusal is recognised");
        assert_eq!(diagnosis.cause, Cause::EulaNotAccepted);
    }

    /// Both are about memory and the advice is opposite: one device could not
    /// give what was asked for, the other server used up what it got. Telling
    /// a player to raise the setting that already exceeds the device is how a
    /// server becomes permanently unstartable.
    #[test]
    fn the_two_memory_failures_do_not_give_each_others_advice() {
        let refused = diagnose(
            &[
                "Error occurred during initialization of VM",
                "Could not reserve enough space for 4194304KB object heap",
            ],
            0,
        )
        .expect("a heap the device could not give is recognised");
        assert_eq!(refused.cause, Cause::HeapUnavailable);
        assert!(
            refused.message.contains("Lower"),
            "told to raise a heap the device already refused: {:?}",
            refused.message
        );

        let exhausted = diagnose(
            &["[12:00:00] [Server thread/ERROR]: java.lang.OutOfMemoryError: Java heap space"],
            0,
        )
        .expect("running out of heap is recognised");
        assert_eq!(exhausted.cause, Cause::OutOfMemory);
        assert!(
            exhausted.message.contains("more memory"),
            "told to lower the heap it ran out of: {:?}",
            exhausted.message
        );
    }

    /// "Failed to start the minecraft server" is printed *after* the thing
    /// that actually failed, so it must never be the answer when the log also
    /// says what that was.
    #[test]
    fn the_reason_beats_the_restatement_of_the_crash() {
        let log = [
            "[12:00:00] [main/ERROR]: java.lang.StackOverflowError",
            "[12:00:00] [main/ERROR]: Failed to start the minecraft server",
        ];
        assert_eq!(diagnose(&log, 0).unwrap().cause, Cause::StackOverflow);

        // On its own it is still the best available answer.
        let alone = ["[12:00:00] [main/ERROR]: Failed to start the minecraft server"];
        assert_eq!(diagnose(&alone, 0).unwrap().cause, Cause::ModInitFailed);
    }

    // --- the negative case -------------------------------------------------

    /// The expensive mistake is the other direction: a player who stopped a
    /// server being shown a crash cause invents a problem that does not exist,
    /// and a retry on a clean stop restarts a server nobody asked for.
    #[test]
    fn an_ordinary_run_has_no_cause_to_report() {
        assert_eq!(
            diagnose(CLEAN_SHUTDOWN, 0),
            None,
            "a clean shutdown was diagnosed as a crash"
        );
        assert_eq!(diagnose::<&str>(&[], 0), None, "an empty console");
    }

    /// Everything here is substring matching over lines a player can type, so
    /// chat and command output are the obvious way to forge a diagnosis.
    /// Nothing prevents that in general — but a diagnosis is only ever read
    /// from a run that already crashed, and these are the phrasings a live
    /// server prints while working perfectly.
    #[test]
    fn talking_about_a_crash_is_not_one() {
        let log = [
            "[12:00:00] [Server thread/INFO]: <Notch> my last server had FAILED TO BIND",
            "[12:00:00] [Server thread/INFO]: <Notch> is the jarfile ok?",
            "[12:00:00] [Server thread/INFO]: Notch issued server command: /say out of memory",
        ];
        assert_eq!(diagnose(&log, 0), None, "chat was read as a crash cause");
    }

    // --- the report --------------------------------------------------------

    #[test]
    fn the_crash_report_is_device_signed_and_carries_the_console() {
        let request = report("srv-1", "dev-9", &["first line", "second line"]);

        assert_eq!(request.method, Method::Post);
        assert_eq!(
            request.path, "/api/service-error/",
            "Django redirects a path without its trailing slash and the POST becomes a GET"
        );
        assert_eq!(
            request.auth,
            Auth::Device,
            "signed as the person rather than the machine — the API answers 403"
        );
        assert_eq!(request.body["service"], "srv-1");
        assert_eq!(request.body["device"], "dev-9");
        assert_eq!(request.body["output"], "first line\nsecond line");
    }

    /// A modded server logs tens of thousands of lines an hour. The desktop is
    /// saved by a 2000-line buffer; a host with a deeper console would post all
    /// of it over the player's mobile data.
    #[test]
    fn a_console_deeper_than_the_cap_reports_the_end_of_it() {
        let lines: Vec<String> = (0..MAX_REPORTED_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect();
        let request = report("srv-1", "dev-9", &lines);
        let output = request.body["output"].as_str().unwrap();

        assert_eq!(output.lines().count(), MAX_REPORTED_LINES);
        assert!(
            output.ends_with(&format!("line {}", MAX_REPORTED_LINES + 499)),
            "the crash itself was truncated away and the boot log kept instead"
        );
    }

    // --- what a host sees --------------------------------------------------

    /// These strings cross the FFI as JSON and hosts switch on them. Renaming
    /// a variant is a silent no-match on the other side, not a compile error.
    #[test]
    fn the_wire_names_hosts_switch_on_do_not_move() {
        let diagnosis = diagnose(&["Invalid or corrupt jarfile"], 0).unwrap();
        let wire = serde_json::to_value(&diagnosis).unwrap();
        assert_eq!(wire["cause"], "corruptJar");
        assert_eq!(wire["recovery"], "redownloadAndRestart");

        let back: Diagnosis = serde_json::from_value(wire).unwrap();
        assert_eq!(back, diagnosis);

        assert_eq!(serde_json::to_value(Cause::PortInUse).unwrap(), "portInUse");
        assert_eq!(serde_json::to_value(Recovery::Report).unwrap(), "report");
    }

    /// Bridge messages are read by players (CLAUDE.md, Conventions). The
    /// desktop's own wording leaked `server.jar` and "mod loader" into a
    /// player's face; these are the words that mean nothing to someone whose
    /// server just stopped.
    #[test]
    fn no_message_makes_the_player_read_an_operator_log() {
        let messages = Cause::ORDER
            .iter()
            .map(|cause| cause.message())
            .chain([REDOWNLOADING]);

        for message in messages {
            for jargon in [
                ".jar",
                "jarfile",
                "JVM",
                "heap space",
                "Error:",
                "Exception",
                "[Homerun]",
                "null",
            ] {
                assert!(
                    !message.contains(jargon),
                    "a player is being shown {jargon:?}: {message:?}"
                );
            }
            assert!(message.ends_with('.'), "not a sentence: {message:?}");
        }
    }

    /// A report is capped by size as well as by line count, keeps the tail,
    /// and cuts between lines rather than through one.
    #[test]
    fn an_enormous_log_is_cut_to_size_on_a_line_boundary() {
        // Few lines, each far too big — the case a line cap alone misses.
        let lines: Vec<String> = (0..40)
            .map(|n| format!("[12:00:00] [Server thread/INFO]: {n} {}", "x".repeat(8_000)))
            .collect();
        let request = report("srv", "dev", &lines);
        let output = request.body["output"].as_str().unwrap();

        assert!(
            output.len() <= MAX_REPORTED_BYTES + 64,
            "not capped: {} bytes",
            output.len()
        );
        assert!(output.starts_with("[earlier lines dropped]\n"));
        // The tail is what explains a crash, so the *last* line must survive
        // whole while the first is gone.
        assert!(output.ends_with(&format!("39 {}", "x".repeat(8_000))));
        assert!(!output.contains("]: 0 "));
        for line in output.lines().skip(1) {
            assert!(line.starts_with("[12:00:00]"), "cut mid-line: {line:.40?}");
        }
    }

    /// Slicing a `String` mid-character panics, and a console is full of
    /// multi-byte text — `§` colour codes, player names, a MOTD.
    #[test]
    fn a_cut_landing_inside_a_character_does_not_panic() {
        // `§` is two bytes, so a cut computed in bytes lands inside one of
        // them for half of all possible lengths.
        for pad in 0..4 {
            let line = format!("[12:00:00] §a{}§r joined\n", "é".repeat(4_000 + pad));
            let lines: Vec<String> = std::iter::repeat_n(line.clone(), 20).collect();
            let output = report("srv", "dev", &lines).body["output"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(output.len() <= MAX_REPORTED_BYTES + 64);
        }
    }
}

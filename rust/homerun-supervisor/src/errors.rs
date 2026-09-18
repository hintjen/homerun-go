//! The process-wide side of app error reporting.
//!
//! [`homerun_core::reporting::app_error`] decides; this holds the one piece
//! of state those decisions are made against, and does the two things a pure
//! crate cannot: touch a clock and touch a disk.
//!
//! # Why the ledger lives here and is not passed in
//!
//! Everywhere else in this crate, state the core needs is round-tripped
//! through the host — `lifecycle`, `history`, the launch plan. That works
//! because one host object owns each of those and calls in sequence.
//!
//! The error ledger has four producers, on four different threads, that never
//! see each other: the JVM's uncaught-exception handler, the WebView bridge,
//! the Rust panic hook, and the host's own reporting coroutine. Give each its
//! own copy and every cap in the core silently multiplies by four — "twenty
//! reports per session" becomes eighty, and the rate limit that exists to
//! stop a render loop from flooding the API stops doing so precisely when
//! four things are failing at once.
//!
//! So there is exactly one, behind a mutex, for the life of the process. The
//! core's decision functions still take `&mut Ledger`, which is what keeps
//! them testable on a laptop with no device attached.
//!
//! # Stash and drain: why a crash does not send its own report
//!
//! A dying thread cannot finish an HTTP request. Android's
//! `Thread.setDefaultUncaughtExceptionHandler` runs on the thread that is
//! about to be killed and hands off to `KillApplicationHandler` immediately
//! afterwards; `NSSetUncaughtExceptionHandler` offers less than that. A POST
//! started there is a POST that does not arrive, and the report you most want
//! is the one you would lose.
//!
//! So the fatal paths [`stash`] to disk and return, and the next launch
//! [`drain`]s. That also collapses two mechanisms into one: the Rust panic
//! hook already writes a file, and it has been writing it where nothing reads
//! it.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use homerun_core::reporting::app_error::{self, Context, Ledger, Occurrence, Severity, Source};
use homerun_core::reporting::Request;

use crate::crash;

/// How many stashed reports one launch will send.
///
/// The cap is a loop cut, not a budget. A crash that reproduces on every
/// launch would otherwise stash one file per launch forever, and a crash
/// inside the *drain* would stash one per file. Five is enough to see a
/// pattern; everything past it is deleted unread, which is the only
/// termination guarantee that does not depend on the bug being fixed.
const MAX_DRAIN: usize = 5;

/// Filenames. Two prefixes because two writers produce them: the panic hook
/// writes plain text it has always written, and [`stash`] writes JSON.
const PANIC_PREFIX: &str = "panic-";
const STASH_PREFIX: &str = "stash-";

/// What a stash file holds.
///
/// The context travels with the sighting because the two can be separated by
/// an app update: a crash on 0.4.2 that drains after the user takes 0.4.3
/// would otherwise be filed against a version it never ran on, and "is this
/// still happening after the fix" is exactly the question that would then get
/// the wrong answer.
#[derive(serde::Serialize, serde::Deserialize)]
struct Stashed {
    context: Option<Context>,
    occurrence: Occurrence,
}

static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
/// Breaks filename ties when two stashes land in the same nanosecond.
static STASH_SEQ: AtomicU64 = AtomicU64::new(0);

/// The one ledger.
///
/// A poisoned mutex is recovered rather than propagated. Poisoning means some
/// earlier caller panicked while holding it, which is exactly the moment the
/// next report matters most — refusing to report because reporting once went
/// wrong is the worst of the available behaviours.
fn ledger() -> MutexGuard<'static, Ledger> {
    LEDGER
        .get_or_init(|| Mutex::new(Ledger::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Point app-level crash artefacts at a host-owned directory.
pub fn attach(data_dir: &str) -> Value {
    let dir = crash::set_app_crash_dir(data_dir);
    json!({ "dir": dir.to_string_lossy() })
}

/// Record one sighting and hand back whatever the core decided.
///
/// The `request` is null when the core held it. That is not an error and the
/// host must not treat it as one: holding is the common case by design, and a
/// host that logged a warning per hold would reproduce the flood in the log
/// that the ledger just prevented in the network.
pub fn report(context: &Context, seen: &Occurrence) -> Value {
    match app_error::observe(&mut ledger(), context, seen) {
        app_error::Verdict::Send {
            request,
            fingerprint,
        } => json!({
            "request": request,
            "fingerprint": fingerprint,
            "held": Value::Null,
        }),
        app_error::Verdict::Hold {
            fingerprint,
            reason,
        } => json!({
            "request": Value::Null,
            "fingerprint": fingerprint,
            "held": reason.as_str(),
        }),
    }
}

/// Write one sighting to disk for the next launch to send.
///
/// Deliberately does not consult the ledger. A stash is what a dying process
/// does, and a rate limit that silences the last thing a process ever said is
/// a rate limit applied at the wrong end — [`drain`] runs the ledger over
/// these on the way out, where holding one costs nothing.
pub fn stash(context: &Context, seen: &Occurrence) -> Result<Value, String> {
    let Some(dir) = crash::app_crash_dir() else {
        // Not an error. A host that never attached has nowhere to put this,
        // and saying so beats failing a call made from a crash handler.
        return Ok(json!({ "stashed": false, "reason": "no crash directory" }));
    };

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = STASH_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{STASH_PREFIX}{stamp}-{seq}.json"));

    let body = serde_json::to_string(&Stashed {
        context: Some(context.clone()),
        occurrence: seen.clone(),
    })
    .map_err(|e| e.to_string())?;
    fs::write(&path, body).map_err(|e| format!("could not stash a report: {e}"))?;

    Ok(json!({ "stashed": true, "path": path.to_string_lossy() }))
}

/// Read, delete and convert everything the previous launch left behind.
///
/// # Delete before parse
///
/// Each file is removed *before* its contents are looked at, and that
/// ordering is the whole safety argument. A stashed report that makes this
/// crate panic while it is being read would, on any other ordering, be read
/// again on the next launch and panic again — a crash loop whose cause is the
/// crash reporter. A file that is already gone cannot do that, and the cost
/// of getting it wrong in this direction is one lost report.
pub fn drain(context: &Context) -> Value {
    let Some(dir) = crash::app_crash_dir() else {
        return json!({ "requests": [], "found": 0 });
    };

    let mut files = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_report(path))
            .collect::<Vec<_>>(),
        Err(_) => return json!({ "requests": [], "found": 0 }),
    };
    // Oldest first: both prefixes embed a timestamp, so the name sorts the
    // way the crashes happened.
    files.sort();
    let found = files.len();

    let mut requests: Vec<Request> = Vec::new();
    for (n, path) in files.into_iter().enumerate() {
        let body = fs::read_to_string(&path).ok();
        let _ = fs::remove_file(&path);

        // Past the cap the file is still deleted, just never read.
        if n >= MAX_DRAIN {
            continue;
        }
        let Some(body) = body else { continue };
        let Some((stashed, seen)) = parse(&path, &body) else {
            continue;
        };

        // A panic file carries no context; the live one is the best available
        // and is at worst the version that failed to start.
        let attributed = stashed.as_ref().unwrap_or(context);

        if let app_error::Verdict::Send { request, .. } =
            app_error::observe(&mut ledger(), attributed, &seen)
        {
            requests.push(request);
        }
    }

    json!({ "requests": requests, "found": found })
}

fn is_report(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with(PANIC_PREFIX) || name.starts_with(STASH_PREFIX))
        .unwrap_or(false)
}

/// Turn one file back into a sighting.
///
/// A stash is JSON this crate wrote. A panic file is the plain text the hook
/// has written since long before this module existed, and its shape is fixed
/// by [`crate::crash`]: a message line, a blank line, `backtrace:`, then the
/// backtrace.
fn parse(path: &Path, body: &str) -> Option<(Option<Context>, Occurrence)> {
    let name = path.file_name()?.to_str()?;

    if name.starts_with(STASH_PREFIX) {
        let stashed: Stashed = serde_json::from_str(body).ok()?;
        return Some((stashed.context, stashed.occurrence));
    }

    let (message, stack) = match body.split_once("\n\nbacktrace:\n") {
        Some((message, backtrace)) => (message.trim(), Some(backtrace.trim().to_string())),
        None => (body.trim(), None),
    };

    Some((
        None,
        Occurrence {
            source: Source::Native,
            severity: Severity::Fatal,
            kind: "panic".to_string(),
            message: message.to_string(),
            stack,
            at_ms: stamp_from(name),
            ..Occurrence::default()
        },
    ))
}

/// The unix seconds the panic hook put in the filename, as milliseconds.
///
/// The crash's own time, not the drain's. The report says when the app died
/// rather than when it next started, and those can be days apart.
fn stamp_from(name: &str) -> i64 {
    name.trim_start_matches(PANIC_PREFIX)
        .trim_end_matches(".txt")
        .parse::<i64>()
        .unwrap_or(0)
        .saturating_mul(1000)
}

/// Forget everything sent this session. Tests only.
///
/// The ledger is process-global by design, which is exactly what makes it
/// leak between tests: `sent` accumulates across every test in the binary and
/// eventually trips `SESSION_MAX`, so a test asserting that a first sighting
/// sends starts failing based on how many tests ran before it.
#[cfg(test)]
pub(crate) fn reset_ledger() {
    *ledger() = Ledger::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh, isolated crash directory, and the process-global lock that
    /// keeps two tests from sharing one.
    fn scratch(name: &str) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = crash::test_guard();
        reset_ledger();
        let root = std::env::temp_dir().join(format!(
            "homerun-app-errors-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let dir = crash::set_app_crash_dir(&root);
        (dir, guard)
    }

    fn context() -> Context {
        Context {
            device_id: "device-1".into(),
            session: "session-1".into(),
            platform: "android".into(),
            app_version: "0.4.2".into(),
            api_url: "https://api.gethomerun.app".into(),
            ..Context::default()
        }
    }

    fn occurrence(message: &str) -> Occurrence {
        Occurrence {
            source: Source::Host,
            severity: Severity::Fatal,
            kind: "kotlin.IllegalStateException".into(),
            message: message.into(),
            at_ms: 1_755_640_000_000,
            ..Occurrence::default()
        }
    }

    #[test]
    fn a_report_comes_back_with_a_request_the_host_can_send() {
        let (_dir, _guard) = scratch("report");
        let answer = report(&context(), &occurrence("first"));

        assert!(answer["request"].is_object(), "{answer}");
        assert_eq!(answer["request"]["path"], "/api/app-error/");
        assert_eq!(answer["request"]["auth"], "device");
        assert!(answer["held"].is_null(), "{answer}");
        assert!(!answer["fingerprint"].as_str().unwrap().is_empty());
    }

    #[test]
    fn one_ledger_is_shared_across_callers() {
        // The property the whole module exists for: a second caller must see
        // the first caller's send, or every cap multiplies by the number of
        // threads that report.
        let (_dir, _guard) = scratch("shared");
        let seen = occurrence("shared ledger");

        let first = report(&context(), &seen);
        assert!(first["request"].is_object());

        let second = report(&context(), &seen);
        assert!(second["request"].is_null(), "{second}");
        assert_eq!(second["held"], "cooldown");
        assert_eq!(second["fingerprint"], first["fingerprint"]);
    }

    #[test]
    fn a_stash_is_written_and_drained_as_a_request() {
        let (dir, _guard) = scratch("roundtrip");
        stash(&context(), &occurrence("stashed and drained")).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        let drained = drain(&context());
        assert_eq!(drained["found"], 1);
        let requests = drained["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["path"], "/api/app-error/");
        assert!(requests[0]["body"]["message"]
            .as_str()
            .unwrap()
            .contains("stashed and drained"));
    }

    #[test]
    fn draining_removes_every_file_it_looked_at() {
        let (dir, _guard) = scratch("removes");
        for n in 0..3 {
            stash(&context(), &occurrence(&format!("crash {n}"))).unwrap();
        }
        drain(&context());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn a_panic_file_from_the_hook_becomes_a_native_report() {
        let (dir, _guard) = scratch("panic");
        fs::write(
            dir.join("panic-1755640000.txt"),
            "the native core exploded (at server.rs:42)\n\nbacktrace:\n   0: homerun_core::boom",
        )
        .unwrap();

        let drained = drain(&context());
        let body = &drained["requests"][0]["body"];
        assert_eq!(body["source"], "native");
        assert_eq!(body["severity"], "fatal");
        assert_eq!(body["kind"], "panic");
        assert!(body["message"].as_str().unwrap().contains("exploded"));
        assert!(body["stack"].as_str().unwrap().contains("homerun_core"));
        // The time the app died, not the time it next started.
        assert_eq!(body["lastSeenMs"], 1_755_640_000_000i64);
    }

    #[test]
    fn a_flood_of_stashed_reports_is_capped_and_the_rest_deleted_unread() {
        // The loop cut. A crash that reproduces every launch must not leave a
        // directory that grows without bound, and must not be re-read.
        let (dir, _guard) = scratch("cap");
        for n in 0..40 {
            stash(&context(), &occurrence(&format!("crash {n}"))).unwrap();
        }

        let drained = drain(&context());
        assert_eq!(drained["found"], 40);
        assert!(drained["requests"].as_array().unwrap().len() <= MAX_DRAIN);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "files past the cap must be deleted, not left for next launch"
        );
    }

    #[test]
    fn an_unreadable_stash_is_dropped_rather_than_retried() {
        let (dir, _guard) = scratch("garbage");
        fs::write(dir.join("stash-1-0.json"), "{ not json at all").unwrap();

        let drained = drain(&context());
        assert_eq!(drained["found"], 1);
        assert!(drained["requests"].as_array().unwrap().is_empty());
        // Gone regardless: a file that cannot be parsed must not be parsed
        // again next launch.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn files_the_module_did_not_write_are_left_alone() {
        let (dir, _guard) = scratch("foreign");
        fs::write(dir.join("something-else.txt"), "not ours").unwrap();

        let drained = drain(&context());
        assert_eq!(drained["found"], 0);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn draining_with_nothing_stashed_is_quiet() {
        let (_dir, _guard) = scratch("empty");
        let drained = drain(&context());
        assert_eq!(drained["found"], 0);
        assert!(drained["requests"].as_array().unwrap().is_empty());
    }

    #[test]
    fn stashing_without_a_directory_is_not_a_failure() {
        // A crash handler is the worst place to surface an error. Answering
        // "no" beats returning one.
        let _guard = crash::test_guard();
        reset_ledger();
        crash::clear_app_crash_dir();

        let answer = stash(&context(), &occurrence("nowhere to go")).unwrap();
        assert_eq!(answer["stashed"], false, "{answer}");

        // Draining is equally quiet with nowhere to look.
        let drained = drain(&context());
        assert_eq!(drained["found"], 0);
    }
}

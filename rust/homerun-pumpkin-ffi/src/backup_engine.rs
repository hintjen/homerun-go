//! A restic-compatible backup engine, linked rather than spawned.
//!
//! # Why this exists at all
//!
//! Android runs the restic binary. iOS cannot spawn a process, so the same job
//! is done by a library compiled into the app. `rustic_core` is
//! restic-format-compatible in both directions, which is the whole point: a
//! world backed up from a phone has to be restorable from the desktop and the
//! other way round, against one repository.
//!
//! # What this module is not allowed to decide
//!
//! Anything. Whether to restore, whether the lease permits a launch, what a
//! failure means, what goes in the `backup-state` body — all of that is
//! `homerun-core::backup`, reached by the host through `Core`. This module
//! opens repositories and moves bytes and reports what happened. Keeping the
//! judgement out is what lets a host that spawns a binary and a host that links
//! a library reach the same answers.
//!
//! # The two things a linked engine does differently
//!
//! **There is no exit code.** The host passes no `exitCode` to
//! `backup.classify`, so restic's exit-3 "completed with warnings" is
//! unreachable and `Failure::succeeded()` can never be true. A snapshot came
//! back or it did not; that is the success test. Warnings are carried in the
//! reply so the host can say something useful, not so it can call a failure a
//! success.
//!
//! **rustic does no repository locking.** It neither writes a lock nor
//! notices one a desktop restic client left. The backup lease — which the API
//! owns and `backup::lease_decision` interprets — is the only thing keeping two
//! devices out of one repository at once.

use std::str::FromStr;
use std::time::Instant;

use rustic_backend::BackendOptions;
use rustic_core::{
    repofile::SnapshotFile, BackupOptions, ConfigOptions, Credentials, ForgetGroups, Grouped,
    KeepOptions, KeyOptions, LocalDestination, LsOptions, PathList, Progress, ProgressBars,
    ProgressType, Repository, RepositoryOptions, RestoreOptions, RusticProgress,
    SnapshotGroupCriterion, SnapshotOptions,
};
use serde_json::{json, Value};

use crate::backup_job::job;
use homerun_core::backup::RepoConfig;

/// restic's default `--group-by`, which Android inherits by not passing the
/// flag. Retention must group the same way on every client or one device's
/// forget pass deletes another device's history.
const GROUP_BY: &str = "host,paths";

pub fn available() -> bool {
    true
}

/// The newest snapshot in the repository, reduced to the shape
/// `backup.restoreDecision` reads, or JSON null if there is none.
pub fn latest_snapshot(request: &str) -> String {
    let request: Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(e) => return failure("The backup request could not be read.", e.to_string(), false),
    };

    let Some(_guard) = JobGuard::claim() else {
        return failure(
            "A backup is already running.",
            "another backup job holds the slot",
            false,
        );
    };

    match read_latest(&request) {
        Ok(value) => json!({ "ok": true, "snapshot": value }).to_string(),
        Err(e) => failure("The backup could not be reached.", e, false),
    }
}

/// Run one backup or restore to completion. Blocks, for minutes.
pub fn run(request: &str) -> String {
    let request: Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(e) => return failure("The backup request could not be read.", e.to_string(), false),
    };

    let Some(_guard) = JobGuard::claim() else {
        return failure(
            "A backup is already running.",
            "another backup job holds the slot",
            false,
        );
    };

    let started = Instant::now();
    let operation = text(&request, "operation").unwrap_or_else(|| "backup".to_string());

    let outcome = match operation.as_str() {
        "backup" => back_up(&request),
        "restore" => restore(&request),
        other => Err(format!("unknown backup operation \"{other}\"")),
    };

    let warnings = job().take_warnings();
    let seconds = started.elapsed().as_secs_f64();

    match outcome {
        Ok(done) => json!({
            "ok": true,
            "snapshotId": done.snapshot_id,
            "bytes": done.bytes,
            "durationSeconds": seconds,
            "warnings": warnings,
        })
        .to_string(),
        Err(message) => {
            let cancelled = job().cancelled();
            let player = if cancelled {
                "The backup was stopped before it finished."
            } else if operation == "restore" {
                "The world could not be restored."
            } else {
                "The world could not be backed up."
            };
            let mut reply = json!({
                "ok": false,
                "error": player,
                "message": message,
                "cancelled": cancelled,
                "durationSeconds": seconds,
                "warnings": warnings,
            });
            // Belt and braces: a caller that ignores `ok` and reads
            // `snapshotId` must not find one on a failure.
            reply["snapshotId"] = Value::Null;
            reply.to_string()
        }
    }
}

struct Done {
    snapshot_id: Option<String>,
    bytes: u64,
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

fn back_up(request: &Value) -> Result<Done, String> {
    let config = repo_config(request)?;
    let source = required(request, "sourceDir")?;
    let device_id = required(request, "deviceId")?;

    job().begin("[Backup] Opening the backup repository…");
    let repo = open_or_init(request, &config)?;
    check_cancelled()?;

    job().begin("[Backup] Backing up the world…");
    let repo = repo.to_indexed_ids().map_err(stringify)?;

    // The device id becomes the snapshot's hostname. This is load-bearing in
    // two directions: the API resolves `pushed_by` from it, and
    // `backup::restore_decision` compares it against this device to decide
    // whether someone else wrote the newest snapshot. Wrong here and a device
    // restores over its own work on its next launch.
    //
    // `command` is set because the default reads `std::env::args_os()`, which
    // on a phone is the app binary's path and tells nobody anything.
    let snapshot = SnapshotFile::from_options(
        &SnapshotOptions::default()
            .host(device_id)
            .command("homerun (ios)".to_string()),
    )
    .map_err(stringify)?;

    let paths = PathList::from_string(&source).map_err(stringify)?;
    let saved = repo
        .backup(&BackupOptions::default(), &paths, snapshot)
        .map_err(stringify)?;

    let bytes = saved.summary.as_ref().map(|s| s.data_added).unwrap_or(0);
    let snapshot_id = saved.id.to_hex().to_string();

    // Retention runs after the snapshot exists, and its failure is not the
    // backup's failure — the world is safe either way, and the host has
    // already been told so.
    if !job().cancelled() && config.keep.is_meaningful() {
        job().begin("[Backup] Applying the backup retention policy…");
        if let Err(e) = forget(&repo, &config) {
            job().warn(format!("retention did not run: {e}"));
        }
    }

    Ok(Done {
        snapshot_id: Some(snapshot_id),
        bytes,
    })
}

fn restore(request: &Value) -> Result<Done, String> {
    let config = repo_config(request)?;
    let snapshot_id = required(request, "snapshotId")?;
    let target = required(request, "targetDir")?;
    let server_id = required(request, "serverId")?;

    job().begin("[Backup] Opening the backup repository…");
    let repo = open_or_init(request, &config)?;
    check_cancelled()?;

    job().begin("[Backup] Reading the backed-up world…");
    // Full index in memory. That is the memory ceiling of this whole
    // subsystem, and it is why the host restores before the server thread
    // starts rather than alongside a running world.
    let repo = repo.to_indexed().map_err(stringify)?;

    let snapshot = repo
        .get_snapshot_from_str(&snapshot_id, |_| true)
        .map_err(stringify)?;

    // The selector has to be the path the *writing* device recorded, not this
    // device's. A desktop's snapshot says `/home/you/.homerun/servers/<id>` or
    // `C:\Users\You\...\servers\<id>`; a phone's says something under its own
    // container. Restoring cross-device is the entire point of this feature,
    // so resolving it from our own path would work only for the one case that
    // needs it least.
    //
    // Resolved here rather than in the host because this is where the snapshot
    // is. `recorded_basename` picks the recorded path belonging to this server
    // and `internal_path` folds a drive letter so the `SNAP:PATH` split cannot
    // land on the wrong colon — both written for exactly this and, until now,
    // used by nobody.
    let recorded = snapshot
        .paths
        .iter()
        .find(|path| homerun_core::backup::recorded_basename(path) == Some(server_id.as_str()))
        .ok_or_else(|| {
            format!(
                "that backup does not contain this server (it holds: {})",
                snapshot.paths.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
    let selector = homerun_core::backup::internal_path(recorded);

    let node = repo
        .node_from_snapshot_and_path(&snapshot, &selector)
        .map_err(stringify)?;

    // `ls` is `Clone` precisely because the streamer is consumed twice: once
    // to plan and once to carry the plan out.
    let entries = repo.ls(&node, &LsOptions::default()).map_err(stringify)?;

    // The trailing slash is how LocalDestination is told this is a directory.
    let destination = LocalDestination::new(&format!("{}/", target.trim_end_matches('/')), true, false)
        .map_err(stringify)?;

    // `no_ownership` because an app sandbox cannot chown to the uid a desktop
    // recorded, and `delete: false` because a desktop-written snapshot has a
    // different file set — deleting everything not in it would take the jar
    // and the host's own bookkeeping with it. The host clears the world
    // directories itself, immediately before this call.
    let options = RestoreOptions::default().no_ownership(true);

    check_cancelled()?;
    let plan = repo
        .prepare_restore(&options, entries.clone(), &destination, false)
        .map_err(stringify)?;
    let bytes = plan.restore_size;

    job().begin("[Backup] Restoring the world…");
    repo.restore(plan, &options, entries, &destination)
        .map_err(stringify)?;

    Ok(Done {
        snapshot_id: Some(snapshot_id),
        bytes,
    })
}

fn read_latest(request: &Value) -> Result<Value, String> {
    let config = repo_config(request)?;

    // Open-only. Asking what the newest snapshot is must never *create* a
    // repository: this runs on every launch, before the restore decision, and
    // initialising from a read path would mean merely starting a server writes
    // to the backend. A repository that does not exist yet is not an error —
    // it is a server whose first backup has not happened, which the core reads
    // as "nothing to compare against".
    let Some(repo) = open_existing(request, &config)? else {
        return Ok(Value::Null);
    };

    let mut snapshots = repo.get_all_snapshots().map_err(stringify)?;
    // Documented as unsorted, and it means it. restic's CLI sorted for us.
    snapshots.sort_by(|a, b| a.time.cmp(&b.time));

    Ok(snapshots.last().map_or(Value::Null, |s| {
        json!({
            "id": s.id.to_hex().to_string(),
            "time": s.time.to_string(),
            "host": s.hostname,
            "paths": s.paths.iter().cloned().collect::<Vec<_>>(),
        })
    }))
}

/// Apply the API's retention policy, then forget what it does not keep.
///
/// Pruning is deliberately not run here. It rewrites and re-uploads pack
/// files, which is unbounded work inside the few foreground seconds iOS gives
/// us. Forgetting alone is cheap — it deletes snapshot files — and the packs
/// it orphans are harmless until something prunes. The desktop does.
fn forget<S: rustic_core::Open>(repo: &Repository<S>, config: &RepoConfig) -> Result<(), String> {
    let keep = KeepOptions::default()
        .keep_last(config.keep.last.map(|v| v as i32))
        .keep_hourly(config.keep.hourly.map(|v| v as i32))
        .keep_daily(config.keep.daily.map(|v| v as i32));

    let criterion = SnapshotGroupCriterion::from_str(GROUP_BY).map_err(stringify)?;
    let grouped = Grouped::from_items(repo.get_all_snapshots().map_err(stringify)?, criterion);

    let ids = ForgetGroups::from_grouped_snapshots_with_retention(
        grouped,
        &keep,
        &rustic_core::jiff::Zoned::now(),
    )
    .map_err(stringify)?
    .into_forget_ids();

    if ids.is_empty() {
        return Ok(());
    }
    repo.delete_snapshots(&ids).map_err(stringify)
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

/// Open the repository, creating it if this is its first backup.
///
/// Initialising through `config_id()` rather than by trying and reading the
/// error is the advantage a library has here: Android has to match restic's
/// prose — "already initialized" from one backend, "config file already
/// exists" from the REST server — because a spawned binary only gives it an
/// exit code and some text.
fn open_or_init(
    request: &Value,
    config: &RepoConfig,
) -> Result<Repository<rustic_core::OpenStatus>, String> {
    if let Some(repo) = open_existing(request, config)? {
        return Ok(repo);
    }
    job().note("[Backup] Preparing a new backup repository…");
    connect(request, config)?
        .init(
            &Credentials::password(&config.restic_password),
            &KeyOptions::default(),
            &ConfigOptions::default(),
        )
        .map_err(stringify)
}

/// Open the repository, or `None` if there is not one there yet.
///
/// Distinguishing "no repository" from "could not reach the repository" is
/// what stops a first launch reporting a backend failure it has not had.
fn open_existing(
    request: &Value,
    config: &RepoConfig,
) -> Result<Option<Repository<rustic_core::OpenStatus>>, String> {
    let repo = connect(request, config)?;
    if repo.config_id().map_err(stringify)?.is_none() {
        return Ok(None);
    }
    repo.open(&Credentials::password(&config.restic_password))
        .map(Some)
        .map_err(stringify)
}

/// Build the backend and attach progress. Does not authenticate.
fn connect(request: &Value, config: &RepoConfig) -> Result<Repository<()>, String> {
    let backends = BackendOptions::default()
        .repository(config.repo.clone())
        .to_backends()
        .map_err(stringify)?;

    let mut options = RepositoryOptions::default();
    // Set explicitly: the default resolves through the XDG/`dirs` dance, which
    // on iOS lands somewhere we do not control. Caches/ is purgeable by the OS,
    // which is correct for a cache and would be catastrophic for anything else.
    if let Some(cache_dir) = text(request, "cacheDir") {
        options = options.cache_dir(std::path::PathBuf::from(cache_dir));
    }

    Repository::new_with_progress(&options, &backends, JobProgressBars).map_err(stringify)
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// Feeds rustic's progress into the job the host is polling.
#[derive(Debug, Clone, Copy)]
struct JobProgressBars;

impl ProgressBars for JobProgressBars {
    fn progress(&self, progress_type: ProgressType, prefix: &str) -> Progress {
        if !prefix.is_empty() {
            job().set_phase_title(prefix);
        }
        Progress::new(JobProgress {
            // Only a byte progress moves the counters. rustic runs several at
            // once — a counter for scanning, a spinner, a byte progress for the
            // transfer — and letting all of them write would make the
            // percentage jump between unrelated denominators.
            counts_bytes: matches!(progress_type, ProgressType::Bytes),
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct JobProgress {
    counts_bytes: bool,
}

impl RusticProgress for JobProgress {
    fn is_hidden(&self) -> bool {
        false
    }

    fn set_length(&self, len: u64) {
        if self.counts_bytes {
            job().set_total(len);
        }
    }

    fn set_title(&self, title: &str) {
        job().set_phase_title(title);
    }

    fn inc(&self, inc: u64) {
        if self.counts_bytes {
            job().advance(inc);
        }
    }

    fn finish(&self) {}
}

// ---------------------------------------------------------------------------
// The log sink
// ---------------------------------------------------------------------------

/// Route rustic's warnings into the job.
///
/// Without this a linked engine has *no* message to hand `backup.classify`:
/// rustic reports a skipped or unreadable entry through `log::warn!` and
/// returns a complete snapshot regardless, so the log is the only place that
/// says something was left out.
struct JobLog;

impl log::Log for JobLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            job().warn(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// Install the sink, once. Failing means something else already claimed the
/// global logger, which is not worth refusing to back up over.
fn install_log_sink() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if log::set_boxed_logger(Box::new(JobLog)).is_ok() {
            log::set_max_level(log::LevelFilter::Warn);
        }
    });
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

/// Holds the job slot for as long as one operation runs, and gives it back
/// however the operation ends — including a panic, which `guarded` catches
/// above us but which would otherwise leave the slot claimed for ever.
struct JobGuard;

impl JobGuard {
    fn claim() -> Option<Self> {
        if job().claim() {
            install_log_sink();
            Some(Self)
        } else {
            None
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        job().release();
    }
}

fn check_cancelled() -> Result<(), String> {
    if job().cancelled() {
        Err("cancelled".to_string())
    } else {
        Ok(())
    }
}

fn repo_config(request: &Value) -> Result<RepoConfig, String> {
    let raw = request
        .get("repo")
        .ok_or_else(|| "the request carried no repository".to_string())?;
    serde_json::from_value(raw.clone()).map_err(|e| format!("bad repository config: {e}"))
}

fn text(request: &Value, key: &str) -> Option<String> {
    request
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn required(request: &Value, key: &str) -> Result<String, String> {
    text(request, key).ok_or_else(|| format!("the request needs {key}"))
}

/// rustic's errors are multi-line reports with a "help" section. Useful in a
/// log and far too much for one line, so the display form is flattened.
fn stringify(error: impl std::fmt::Display) -> String {
    error
        .to_string()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn failure(player: &str, message: impl Into<String>, cancelled: bool) -> String {
    json!({
        "ok": false,
        "error": player,
        "message": message.into(),
        "cancelled": cancelled,
    })
    .to_string()
}

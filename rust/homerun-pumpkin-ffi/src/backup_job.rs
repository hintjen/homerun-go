//! The one backup or restore this process is running, and how to watch it.
//!
//! # Why this is separate from the engine
//!
//! Compiled on every platform, engine or not. The host polls progress and asks
//! to cancel through the same C surface whatever is underneath, and a build
//! with no engine still has to answer those calls rather than fail to link.
//!
//! # Why polling
//!
//! Same shape as the server console: the engine writes into a buffer and the
//! host reads by cursor. A callback into Swift would fire from a rayon worker
//! thread, forcing a hop per call and leaving us to manage a context pointer's
//! lifetime across a cancelled job. Polling has neither problem, and the host
//! already has a timer doing exactly this for the console.
//!
//! # Cancellation is cooperative, and coarse
//!
//! There is no way to interrupt a transfer already inside the engine — rustic
//! exposes no cancellation hook, and unwinding out of a progress callback
//! would panic through rayon. So [`BackupJob::cancelled`] is checked at phase
//! boundaries only. A cancel during the (network-bound) open and index phases
//! lands quickly; one during the transfer lands when the transfer ends.
//!
//! That is enough for what it is for. iOS gives a backgrounded app about five
//! seconds' warning, and the useful thing to do with them is report the backup
//! failed so the lease closes — not to stop the work.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::log_buffer::LogBuffer;

/// How many progress lines to keep. Smaller than the console's: these are
/// coarse phase announcements, not a server's output.
const LOG_CAPACITY: usize = 200;

/// How many distinct warnings to carry back in a reply.
const MAX_WARNINGS: usize = 20;

static JOB: OnceLock<BackupJob> = OnceLock::new();

pub fn job() -> &'static BackupJob {
    JOB.get_or_init(BackupJob::default)
}

pub struct BackupJob {
    inner: Mutex<Inner>,
    /// Set by the host, read by the engine at phase boundaries.
    cancel: AtomicBool,
    /// One at a time. Not because the engine could not manage two, but because
    /// the progress surface is a single cursor and two jobs would interleave
    /// into it with no way to tell them apart.
    running: AtomicBool,
}

struct Inner {
    logs: LogBuffer,
    phase: String,
    current: u64,
    total: u64,
    /// What the engine complained about without giving up.
    ///
    /// Collected because a linked engine has no exit code, so the *text* is
    /// the only signal that a snapshot came back with something skipped —
    /// which is the thing restic's exit 3 tells the Android host.
    warnings: Vec<String>,
}

/// What the host reads each tick.
#[derive(Debug)]
pub struct Progress {
    pub lines: Vec<String>,
    pub cursor: u64,
    /// True when lines were lost between this call and the last — the host
    /// shows a gap rather than pretending the console is complete.
    pub dropped: bool,
    pub phase: String,
    pub current: u64,
    /// Zero means "not known yet", which is most of a backup's scanning phase.
    pub total: u64,
}

impl Default for BackupJob {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                logs: LogBuffer::new(LOG_CAPACITY),
                phase: String::new(),
                current: 0,
                total: 0,
                warnings: Vec::new(),
            }),
            cancel: AtomicBool::new(false),
            running: AtomicBool::new(false),
        }
    }
}

impl BackupJob {
    /// A poisoned lock means a previous job panicked mid-update. The counters
    /// are a progress display; recovering and carrying on is strictly better
    /// than propagating a panic across the FFI boundary.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the job slot, or report that something else holds it.
    ///
    /// Clears the cancel flag and the previous run's counters, so a cancel
    /// asked for after a job already finished cannot kill the next one.
    pub fn claim(&self) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.cancel.store(false, Ordering::SeqCst);
        let mut inner = self.lock();
        inner.phase.clear();
        inner.current = 0;
        inner.total = 0;
        inner.warnings.clear();
        true
    }

    pub fn release(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Enter a named phase, resetting the counters it will report against.
    pub fn begin(&self, phase: impl Into<String>) {
        let phase = phase.into();
        let mut inner = self.lock();
        inner.logs.push(phase.clone());
        inner.phase = phase;
        inner.current = 0;
        inner.total = 0;
    }

    /// A line for the player's console, with no counter attached.
    pub fn note(&self, line: impl Into<String>) {
        self.lock().logs.push(line);
    }

    pub fn set_total(&self, total: u64) {
        self.lock().total = total;
    }

    pub fn advance(&self, by: u64) {
        let mut inner = self.lock();
        inner.current = inner.current.saturating_add(by);
    }

    pub fn set_phase_title(&self, title: impl Into<String>) {
        self.lock().phase = title.into();
    }

    /// Record something the engine complained about but carried on past.
    ///
    /// Bounded: a repository with thousands of unreadable files would
    /// otherwise grow this without limit, and nobody reads the ten-thousandth
    /// one. The count is what matters after the first few.
    pub fn warn(&self, line: impl Into<String>) {
        let line = line.into();
        let mut inner = self.lock();
        if inner.warnings.len() < MAX_WARNINGS {
            inner.warnings.push(line.clone());
        }
        inner.logs.push(line);
    }

    /// Take the warnings, leaving none behind. Called once when a job ends.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().warnings)
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn progress_since(&self, cursor: u64) -> Progress {
        let inner = self.lock();
        let slice = inner.logs.since(cursor);
        Progress {
            lines: slice.lines,
            cursor: slice.cursor,
            dropped: slice.dropped,
            phase: inner.phase.clone(),
            current: inner.current,
            total: inner.total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own job rather than the singleton, so they do not
    /// fight over one global cursor.
    fn fresh() -> BackupJob {
        BackupJob::default()
    }

    #[test]
    fn a_second_job_cannot_claim_the_slot() {
        let job = fresh();
        assert!(job.claim());
        assert!(!job.claim(), "two jobs ran at once");
        job.release();
        assert!(job.claim(), "the slot was not released");
    }

    #[test]
    fn claiming_clears_a_cancel_left_by_the_previous_job() {
        let job = fresh();
        assert!(job.claim());
        job.request_cancel();
        assert!(job.cancelled());
        job.release();

        // The danger this guards: a cancel that arrived as the last job was
        // ending would otherwise kill the next one before it started.
        assert!(job.claim());
        assert!(!job.cancelled(), "a stale cancel survived into a new job");
    }

    #[test]
    fn progress_reports_lines_once_and_then_advances_the_cursor() {
        let job = fresh();
        job.begin("backing up");
        job.note("scanning");

        let first = job.progress_since(0);
        assert_eq!(first.lines, vec!["backing up".to_string(), "scanning".to_string()]);
        assert_eq!(first.phase, "backing up");

        let second = job.progress_since(first.cursor);
        assert!(second.lines.is_empty(), "lines were replayed: {:?}", second.lines);
    }

    #[test]
    fn a_new_phase_resets_the_counters_it_reports_against() {
        let job = fresh();
        job.begin("backing up");
        job.set_total(100);
        job.advance(60);
        assert_eq!(job.progress_since(0).current, 60);

        // Otherwise the next phase inherits a percentage from the last one and
        // the bar appears to start part-full.
        job.begin("applying retention");
        let p = job.progress_since(0);
        assert_eq!((p.current, p.total), (0, 0));
        assert_eq!(p.phase, "applying retention");
    }

    #[test]
    fn advancing_past_the_end_saturates_rather_than_wrapping() {
        let job = fresh();
        job.begin("backing up");
        job.advance(u64::MAX);
        job.advance(10);
        assert_eq!(job.progress_since(0).current, u64::MAX);
    }
}

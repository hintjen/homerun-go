//! Backups: what to restore, when to refuse to launch, and what a failure means.
//!
//! Reference: `nativeServerManager.ts` — `runPreLaunchRestore:2992`,
//! `runOnStopBackup:2837`, the lease gate at `startServer:1049`,
//! `reportBackupState:3120`; `resticBackup.ts` for retention and the error
//! classifiers; and `api/docs/native-restic-backup.md` for the API half.
//!
//! # Why this is worth sharing
//!
//! The failure mode is silent data loss, and it is a *decision* failure rather
//! than an execution one. Restore the wrong thing and a player's afternoon is
//! gone; launch while another device is mid-backup and two worlds diverge with
//! no error anywhere. Those decisions are small, pure, and exactly the sort of
//! thing that drifts when written three times.
//!
//! # No engine, no I/O
//!
//! Nothing here runs restic, opens a socket, or touches a file. It takes what
//! the host already knows — the API's backup block, the snapshot list, whether
//! a world exists on disk — and answers. That is what lets the same answers
//! serve a host that spawns a binary and a host that links a library, which is
//! the difference between iOS having backups and not.

use serde::{Deserialize, Serialize};

/// The credential block the API hands down on a server fetch (`get_backup`).
///
/// Assembled server-side by `build_restic_repo_url`, so nothing here builds a
/// URL — a client that assembled its own would be a second place for the
/// rest-server topology to live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    /// `rest:https://<volume>:<http_pw>@<host>/<volume>/`
    pub repo: String,
    /// The repository's encryption passphrase.
    pub restic_password: String,
    #[serde(default)]
    pub keep: Retention,
}

/// How much history to keep. **Chosen by the API, not here.**
///
/// The policies are a *union*: a snapshot survives if any rule keeps it. That
/// is why `last` matters — native backups happen on stop, so several can land
/// in one hour, and `hourly` alone would thin them to one. The API's current
/// answer is `{30, 24, 30}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Retention {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hourly: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily: Option<u32>,
}

impl Retention {
    /// True when at least one rule is set. Applying an empty policy would ask
    /// the engine to forget everything, so a caller must check this first.
    pub fn is_meaningful(&self) -> bool {
        self.last.is_some() || self.hourly.is_some() || self.daily.is_some()
    }
}

/// One snapshot, reduced to what a decision needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    /// RFC 3339, as the engine reported it. Compared as text only for equality
    /// — ordering comes from the engine's own list order, never from parsing.
    pub time: String,
    /// **The writing device's id.** Clients back up with the device id as the
    /// snapshot hostname, and the API resolves `pushed_by` from it. This is the
    /// whole basis of the handoff decision below.
    pub host: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// What to do with the local world before launching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum RestoreDecision {
    /// A dashboard restore pinned a specific snapshot. Unconditional, and
    /// one-shot: the API clears the pin on the next running ack.
    Rollback { snapshot_id: String },
    /// Pull the newest snapshot over the local world.
    RestoreLatest {
        snapshot_id: String,
        reason: RestoreReason,
    },
    /// Keep what is on disk.
    Skip { reason: SkipReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestoreReason {
    /// Another device wrote the newest snapshot — the friend-hosting handoff.
    /// Their world wins over this device's stale copy.
    AnotherDeviceIsNewer,
    /// This device wrote it, but there is no world here any more: a fresh
    /// install or a wipe, where the backup is the only copy left.
    LocalWorldMissing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkipReason {
    /// No snapshots, or the repository could not be reached. Starting with
    /// local data is deliberate: a device with no signal must still be able to
    /// host, and the lease is the real concurrency guard.
    NoSnapshotAvailable,
    /// This device wrote the newest snapshot and still has the world, so local
    /// is authoritative and ahead.
    LocalIsAuthoritative,
}

/// Decide what to restore before launch.
///
/// `pinned` is `RESTORE_FROM_SNAPSHOT`, staged by a dashboard restore.
/// `latest` is the newest snapshot, or `None` when the repository is empty or
/// unreachable — those are deliberately the same answer.
/// `has_local_world` is whether a world exists on disk.
///
/// # Divergence from the desktop, and why
///
/// The desktop asks whether the server directory is *entirely empty*
/// (`isServerDirEmpty`: `readdirSync(dir).length === 0`). Mobile writes
/// `eula.txt`, `server.properties`, `ops.json`, `whitelist.json`,
/// `homerun-jar.json` and the server jar itself **before** launch, so that test
/// is never true there — and the `LocalWorldMissing` branch would never fire.
/// A wiped-and-reinstalled phone would silently start an empty world instead
/// of restoring its own backup.
///
/// So the question asked here is the desktop's *intent*, using the desktop's
/// own predicate for it: `hasLocalWorld`, which checks that `world/` or
/// `worlds/` is non-empty. On desktop the two agree in every case that matters,
/// because a directory with no files also has no world.
pub fn restore_decision(
    pinned: Option<&str>,
    latest: Option<&Snapshot>,
    my_device_id: &str,
    has_local_world: bool,
) -> RestoreDecision {
    if let Some(id) = pinned.map(str::trim).filter(|s| !s.is_empty()) {
        return RestoreDecision::Rollback {
            snapshot_id: id.to_string(),
        };
    }

    let Some(latest) = latest else {
        return RestoreDecision::Skip {
            reason: SkipReason::NoSnapshotAvailable,
        };
    };

    if latest.host != my_device_id {
        return RestoreDecision::RestoreLatest {
            snapshot_id: latest.id.clone(),
            reason: RestoreReason::AnotherDeviceIsNewer,
        };
    }

    if has_local_world {
        RestoreDecision::Skip {
            reason: SkipReason::LocalIsAuthoritative,
        }
    } else {
        RestoreDecision::RestoreLatest {
            snapshot_id: latest.id.clone(),
            reason: RestoreReason::LocalWorldMissing,
        }
    }
}

/// Whether this device may start, given who holds the backup lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum LeaseDecision {
    Launch,
    /// Another device is mid-backup. Refusing is the point: launching would
    /// start a second world from a snapshot that is still being written.
    Blocked {
        device: String,
    },
    /// The user accepted the data-loss warning and took the lease. The API
    /// clears the other device's repository lock on the running ack.
    Forced {
        taken_from: String,
    },
}

/// Decide whether the backup lease permits a launch.
///
/// The lease is API-managed and has no acquire call: it opens on the `stopped`
/// ack when the stopping device sets `backup_in_progress`, and closes when that
/// device reports `backup-state` — **complete or failed** — or on the next
/// `running` ack.
///
/// That last clause matters on mobile. A phone that is swipe-closed or killed
/// mid-backup never reports, so the lease stays open until that device runs
/// again, and `force` is the only way past. It is not a lie to the user — the
/// data really is at risk — but the common cause is a dead phone, not a
/// concurrent host.
pub fn lease_decision(
    lease_device: Option<&str>,
    my_device_id: &str,
    force: bool,
) -> LeaseDecision {
    match lease_device.map(str::trim).filter(|s| !s.is_empty()) {
        None => LeaseDecision::Launch,
        Some(holder) if holder == my_device_id => LeaseDecision::Launch,
        Some(holder) if force => LeaseDecision::Forced {
            taken_from: holder.to_string(),
        },
        Some(holder) => LeaseDecision::Blocked {
            device: holder.to_string(),
        },
    }
}

/// Whether there is anything worth backing up.
///
/// Guards the on-stop backup. A server that never generated a world — a launch
/// that failed early — must not push an empty snapshot over a good one, which
/// would look exactly like a successful backup and quietly become the newest.
pub fn should_back_up(has_local_world: bool) -> bool {
    has_local_world
}

/// The directory name a snapshot's recorded path ends in.
///
/// Snapshots record an absolute path from whichever device wrote them, so a
/// Windows desktop's snapshot restored onto a phone arrives as
/// `C:\Users\me\...\servers\<id>`. The host restores into a scratch directory
/// and then locates this subtree by name, which is what makes a cross-platform
/// handoff work at all.
///
/// Handles both separators, because the path's origin is not the restoring
/// platform's.
pub fn recorded_basename(path: &str) -> Option<&str> {
    path.split(['/', '\\']).rfind(|s| !s.is_empty())
}

/// A recorded path in the engine's internal selector form: forward slashes,
/// with a Windows drive folded into the first segment.
///
/// `C:\Users\me\srv` → `/C/Users/me/srv`, and `/home/me/srv` unchanged. This is
/// what a `<snapshot>:<subfolder>` selector matches against, so getting it
/// wrong silently selects nothing rather than failing.
pub fn internal_path(path: &str) -> String {
    let forward = path.replace('\\', "/");
    let bytes = forward.as_bytes();

    // `C:/rest` → `/C/rest`
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        return format!("/{}/{}", &forward[..1], &forward[3..]);
    }
    if forward.starts_with('/') {
        forward
    } else {
        format!("/{forward}")
    }
}

/// What went wrong, normalised across engines.
///
/// Hosts speak different dialects — a spawned binary reports an exit code and
/// text, a linked library returns typed errors — so each normalises to this
/// before asking what it means. Keeping the *meanings* here is the point;
/// keeping the dialects out is what makes that possible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Failure {
    /// A 401 from the rest-server. The API mints repository credentials
    /// immediately but provisions the htpasswd entry asynchronously, so a
    /// brand-new server's first backup can beat its own account into existence.
    /// Retry; it is normally sub-second.
    AuthRace,
    /// The repository is locked by a run on *this* device that died without
    /// releasing it. Safe to clear and retry.
    StaleLocalLock,
    /// Locked by another device, which may be a live backup. Never clear this;
    /// surface it.
    LockedByOther {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    /// The operation succeeded and something unreadable was skipped.
    ///
    /// Android reaches this on every backup: the snapshot is complete and
    /// verified, but restic cannot read an xattr on an ancestor directory
    /// (`/data/data`) and exits 3. A host treating non-zero as failure reports
    /// every backup broken while every backup worked.
    CompletedWithWarnings,
    /// Worth another attempt — a dropped connection, a timeout.
    Transient,
    /// Everything else. Surface it and stop.
    Fatal,
}

impl Failure {
    /// Whether retrying the same operation could plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::AuthRace | Self::StaleLocalLock | Self::Transient
        )
    }

    /// Whether the operation actually produced a snapshot.
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::CompletedWithWarnings)
    }
}

/// Classify an engine failure.
///
/// `exit_code` is `Some` for a spawned binary and `None` for a linked library.
/// `message` is whatever the engine said, stdout and stderr together.
/// `my_host` is the hostname the engine records in its locks, so a lock this
/// device left can be told from a lock another device holds.
pub fn classify(exit_code: Option<i32>, message: &str, my_host: &str) -> Failure {
    let text = message.to_ascii_lowercase();

    // Ordering matters: a locked repository also exits non-zero, and the
    // auth race also produces a message. Check the specific before the general.
    if text.contains("401") || text.contains("unauthorized") {
        return Failure::AuthRace;
    }

    if text.contains("already locked") || text.contains("unable to create lock") {
        return match lock_holder(message) {
            Some((host, pid)) if host.eq_ignore_ascii_case(my_host) => {
                let _ = pid;
                Failure::StaleLocalLock
            }
            Some((host, pid)) => Failure::LockedByOther {
                host: Some(host.to_string()),
                pid: Some(pid),
            },
            // Locked, but the message did not say by whom. Assume someone else:
            // clearing a lock we cannot attribute risks corrupting a live run.
            None => Failure::LockedByOther {
                host: None,
                pid: None,
            },
        };
    }

    match exit_code {
        // restic's "completed with warnings". The snapshot exists.
        Some(3) => Failure::CompletedWithWarnings,
        Some(0) | None if text.is_empty() => Failure::Fatal,
        _ if is_transient(&text) => Failure::Transient,
        _ => Failure::Fatal,
    }
}

fn is_transient(lowercase: &str) -> bool {
    [
        "connection reset",
        "connection refused",
        "timeout",
        "timed out",
        "temporary failure",
        "eof",
        "broken pipe",
    ]
    .iter()
    .any(|needle| lowercase.contains(needle))
}

/// `"...already locked by PID 1234 on HOSTNAME by user@..."` → `("HOSTNAME", 1234)`.
fn lock_holder(message: &str) -> Option<(&str, u32)> {
    let lower = message.to_ascii_lowercase();
    let start = lower.find("already locked by pid ")? + "already locked by pid ".len();
    let rest = &message[start..];

    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let pid: u32 = digits.parse().ok()?;

    let after = &rest[digits.len()..];
    let on = after.to_ascii_lowercase().find(" on ")? + " on ".len();
    let host = after[on..].split_whitespace().next()?;
    if host.is_empty() {
        return None;
    }
    Some((host, pid))
}

/// The body of `POST /api/server/<id>/backup-state/`.
///
/// Reporting is what releases the backup lease — for `Backup`, and only on a
/// terminal status. A host that finishes a backup and does not report leaves
/// every other device locked out until its own next launch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateReport {
    pub operation: Operation,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Uploaded for a backup, downloaded for a restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_bps: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Backup,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Complete,
    Failed,
}

impl StateReport {
    /// A successful transfer, with throughput derived rather than passed in —
    /// the two must agree, and computing it here means they cannot disagree.
    pub fn complete(
        operation: Operation,
        snapshot_id: Option<String>,
        bytes: u64,
        duration_seconds: f64,
    ) -> Self {
        Self {
            operation,
            status: Status::Complete,
            snapshot_id,
            error: None,
            bytes: Some(bytes),
            duration_seconds: Some(duration_seconds),
            speed_bps: (duration_seconds > 0.0).then(|| bytes as f64 / duration_seconds),
        }
    }

    pub fn failed(
        operation: Operation,
        snapshot_id: Option<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            operation,
            status: Status::Failed,
            snapshot_id,
            error: Some(error.into()),
            bytes: None,
            duration_seconds: None,
            speed_bps: None,
        }
    }

    /// Whether reporting this releases the backup lease.
    ///
    /// Backups hold the lease; restores never do. Both statuses release it —
    /// a failed backup that held the lease forever would strand the server.
    pub fn releases_lease(&self) -> bool {
        self.operation == Operation::Backup
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str, host: &str) -> Snapshot {
        Snapshot {
            id: id.into(),
            time: "2026-08-10T12:00:00Z".into(),
            host: host.into(),
            paths: vec!["/data/servers/abc".into()],
        }
    }

    // ─── the restore decision ───────────────────────────────────────────────

    /// The friend-hosting handoff: their world wins over our stale copy.
    #[test]
    fn another_devices_snapshot_is_restored() {
        let latest = snapshot("s1", "device-b");
        assert_eq!(
            restore_decision(None, Some(&latest), "device-a", true),
            RestoreDecision::RestoreLatest {
                snapshot_id: "s1".into(),
                reason: RestoreReason::AnotherDeviceIsNewer,
            }
        );
    }

    /// We wrote it and we still have the world, so we are ahead of it.
    #[test]
    fn our_own_snapshot_does_not_overwrite_a_live_world() {
        let latest = snapshot("s1", "device-a");
        assert_eq!(
            restore_decision(None, Some(&latest), "device-a", true),
            RestoreDecision::Skip {
                reason: SkipReason::LocalIsAuthoritative,
            }
        );
    }

    /// The wiped-device case, and the reason this takes `has_local_world`
    /// rather than the desktop's is-the-directory-empty test: on mobile the
    /// directory always has files by launch time, so that test never fires and
    /// a reinstalled phone would start an empty world.
    #[test]
    fn our_own_snapshot_is_restored_when_the_world_is_gone() {
        let latest = snapshot("s1", "device-a");
        assert_eq!(
            restore_decision(None, Some(&latest), "device-a", false),
            RestoreDecision::RestoreLatest {
                snapshot_id: "s1".into(),
                reason: RestoreReason::LocalWorldMissing,
            }
        );
    }

    /// Offline tolerance: no repository must never block hosting.
    #[test]
    fn no_snapshot_means_start_with_local_data() {
        assert_eq!(
            restore_decision(None, None, "device-a", true),
            RestoreDecision::Skip {
                reason: SkipReason::NoSnapshotAvailable,
            }
        );
        assert_eq!(
            restore_decision(None, None, "device-a", false),
            RestoreDecision::Skip {
                reason: SkipReason::NoSnapshotAvailable,
            }
        );
    }

    /// A dashboard rollback overrides every other consideration, including a
    /// live local world — that is what the user asked for.
    #[test]
    fn a_pinned_snapshot_wins_over_everything() {
        let latest = snapshot("newest", "device-a");
        assert_eq!(
            restore_decision(Some("older"), Some(&latest), "device-a", true),
            RestoreDecision::Rollback {
                snapshot_id: "older".into()
            }
        );
        // And still applies with no snapshot list at all.
        assert_eq!(
            restore_decision(Some("older"), None, "device-a", true),
            RestoreDecision::Rollback {
                snapshot_id: "older".into()
            }
        );
    }

    /// An empty or whitespace pin is not a pin. The API clears it by writing
    /// an empty value, and treating that as a snapshot id would restore
    /// nothing, loudly.
    #[test]
    fn a_blank_pin_is_ignored() {
        let latest = snapshot("s1", "device-a");
        for blank in ["", "   "] {
            assert_eq!(
                restore_decision(Some(blank), Some(&latest), "device-a", true),
                RestoreDecision::Skip {
                    reason: SkipReason::LocalIsAuthoritative,
                },
                "pin {blank:?}"
            );
        }
    }

    /// The snapshot shape restic 0.18.0 actually emits on Android, after the
    /// host maps `hostname` to `host`. Captured from a real device run rather
    /// than written from the documentation, because the field that matters —
    /// who wrote it — is the one a co-host restores over live work if we read
    /// it wrong.
    #[test]
    fn a_real_snapshot_from_a_device_parses_and_decides() {
        let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "id": "e9f8c5d760847000d8dc5d47924e06bba2fa7d91e8887495edec38b17f383b01",
            "time": "2026-08-10T19:09:22.3199328Z",
            "host": "device-a",
            "paths": ["/data/data/app.gethomerun.mobile.debug/files/e2e/server"]
        }))
        .expect("restic's shape must parse");

        // The writing device sees its own work as authoritative.
        assert_eq!(
            restore_decision(None, Some(&snapshot), "device-a", true),
            RestoreDecision::Skip {
                reason: SkipReason::LocalIsAuthoritative
            }
        );

        // Any other device restores it — the handoff.
        assert!(matches!(
            restore_decision(None, Some(&snapshot), "device-b", true),
            RestoreDecision::RestoreLatest {
                reason: RestoreReason::AnotherDeviceIsNewer,
                ..
            }
        ));

        // And the recorded path yields the directory to relocate.
        assert_eq!(recorded_basename(&snapshot.paths[0]), Some("server"));
    }

    // ─── the lease ──────────────────────────────────────────────────────────

    #[test]
    fn no_lease_permits_launch() {
        assert_eq!(
            lease_decision(None, "device-a", false),
            LeaseDecision::Launch
        );
        assert_eq!(
            lease_decision(Some(""), "device-a", false),
            LeaseDecision::Launch
        );
    }

    #[test]
    fn our_own_lease_does_not_block_us() {
        assert_eq!(
            lease_decision(Some("device-a"), "device-a", false),
            LeaseDecision::Launch
        );
    }

    #[test]
    fn another_devices_lease_blocks_launch() {
        assert_eq!(
            lease_decision(Some("device-b"), "device-a", false),
            LeaseDecision::Blocked {
                device: "device-b".into()
            }
        );
    }

    #[test]
    fn force_takes_the_lease_and_names_who_lost_it() {
        assert_eq!(
            lease_decision(Some("device-b"), "device-a", true),
            LeaseDecision::Forced {
                taken_from: "device-b".into()
            }
        );
    }

    // ─── the on-stop guard ──────────────────────────────────────────────────

    /// A launch that died before generating a world must not push an empty
    /// snapshot over a good one — it would become the newest and look fine.
    #[test]
    fn a_server_with_no_world_is_never_backed_up() {
        assert!(!should_back_up(false));
        assert!(should_back_up(true));
    }

    // ─── paths ──────────────────────────────────────────────────────────────

    #[test]
    fn a_recorded_path_yields_its_directory_name() {
        assert_eq!(recorded_basename("/data/servers/abc"), Some("abc"));
        assert_eq!(
            recorded_basename(r"C:\Users\me\Homerun\servers\abc"),
            Some("abc")
        );
        assert_eq!(recorded_basename("/data/servers/abc/"), Some("abc"));
        assert_eq!(recorded_basename(""), None);
    }

    /// The case that makes this necessary: a Windows desktop's snapshot being
    /// restored onto a phone.
    #[test]
    fn a_windows_path_folds_its_drive_into_the_first_segment() {
        assert_eq!(internal_path(r"C:\Users\me\srv"), "/C/Users/me/srv");
        assert_eq!(internal_path("C:/Users/me/srv"), "/C/Users/me/srv");
    }

    #[test]
    fn a_posix_path_is_unchanged() {
        assert_eq!(internal_path("/home/me/srv"), "/home/me/srv");
        assert_eq!(
            internal_path("/data/data/app/files/servers/x"),
            "/data/data/app/files/servers/x"
        );
    }

    #[test]
    fn a_relative_path_is_rooted() {
        assert_eq!(internal_path("servers/abc"), "/servers/abc");
    }

    // ─── failure classification ─────────────────────────────────────────────

    #[test]
    fn a_401_is_the_provisioning_race() {
        assert_eq!(
            classify(Some(1), "server returned 401", "me"),
            Failure::AuthRace
        );
        assert_eq!(classify(None, "Unauthorized", "me"), Failure::AuthRace);
        assert!(Failure::AuthRace.is_retryable());
    }

    /// Our own stale lock may be cleared; another device's must not be.
    #[test]
    fn a_lock_is_attributed_before_it_is_cleared() {
        let ours = "repository is already locked by PID 4242 on MY-PC by me";
        assert_eq!(classify(Some(1), ours, "my-pc"), Failure::StaleLocalLock);

        let theirs = "repository is already locked by PID 99 on OTHER-PC by them";
        assert_eq!(
            classify(Some(1), theirs, "my-pc"),
            Failure::LockedByOther {
                host: Some("OTHER-PC".into()),
                pid: Some(99),
            }
        );
    }

    /// A lock we cannot attribute is treated as someone else's. Clearing a
    /// live run's lock is the one mistake here that corrupts a repository.
    #[test]
    fn an_unattributed_lock_is_never_ours() {
        let vague = "unable to create lock in backend";
        assert_eq!(
            classify(Some(1), vague, "my-pc"),
            Failure::LockedByOther {
                host: None,
                pid: None
            }
        );
        assert!(!classify(Some(1), vague, "my-pc").is_retryable());
    }

    /// The Android case, verified on device: the snapshot is complete and
    /// hash-verified, and restic still exits 3 because it could not read an
    /// xattr on `/data/data`. Treating this as failure reports every backup
    /// broken while every backup worked.
    #[test]
    fn exit_three_is_success_with_warnings() {
        let msg = "error: can not obtain extended attribute user.serial for /data/data\n\
                   snapshot 9b890179 saved\n\
                   Warning: at least one source file could not be read";
        let verdict = classify(Some(3), msg, "localhost");
        assert_eq!(verdict, Failure::CompletedWithWarnings);
        assert!(
            verdict.succeeded(),
            "the snapshot exists and must be reported complete"
        );
    }

    #[test]
    fn network_trouble_is_retryable() {
        for msg in [
            "connection reset by peer",
            "context deadline exceeded: timeout",
            "unexpected EOF",
        ] {
            assert_eq!(classify(Some(1), msg, "me"), Failure::Transient, "{msg}");
            assert!(classify(Some(1), msg, "me").is_retryable());
        }
    }

    #[test]
    fn anything_else_is_fatal_and_not_retried() {
        let verdict = classify(Some(1), "wrong password or no key found", "me");
        assert_eq!(verdict, Failure::Fatal);
        assert!(!verdict.is_retryable());
        assert!(!verdict.succeeded());
    }

    // ─── the state report ───────────────────────────────────────────────────

    #[test]
    fn a_complete_report_derives_its_own_throughput() {
        let report = StateReport::complete(Operation::Backup, Some("s1".into()), 2_000_000, 4.0);
        assert_eq!(report.speed_bps, Some(500_000.0));
        assert_eq!(report.status, Status::Complete);
        assert!(report.error.is_none());
    }

    /// A zero-duration transfer must not divide by zero and report infinity.
    #[test]
    fn an_instant_transfer_reports_no_speed_rather_than_infinity() {
        let report = StateReport::complete(Operation::Restore, None, 1_000, 0.0);
        assert_eq!(report.speed_bps, None);
    }

    /// A failed backup must still release the lease, or the server is stranded
    /// until that device runs again.
    #[test]
    fn a_failed_backup_still_releases_the_lease() {
        let report = StateReport::failed(Operation::Backup, None, "disk full");
        assert!(report.releases_lease());
        assert_eq!(report.status, Status::Failed);
    }

    /// Restores hold no lease, so reporting one releases nothing.
    #[test]
    fn a_restore_never_touches_the_lease() {
        assert!(!StateReport::complete(Operation::Restore, None, 1, 1.0).releases_lease());
        assert!(!StateReport::failed(Operation::Restore, None, "x").releases_lease());
    }

    /// The wire shape the API expects. A renamed field is accepted silently by
    /// the endpoint and lost, so it is pinned.
    #[test]
    fn the_report_serialises_as_the_api_expects() {
        let json = serde_json::to_value(StateReport::complete(
            Operation::Backup,
            Some("abc".into()),
            10,
            2.0,
        ))
        .unwrap();

        assert_eq!(json["operation"], "backup");
        assert_eq!(json["status"], "complete");
        assert_eq!(json["snapshot_id"], "abc");
        assert_eq!(json["bytes"], 10);
        assert_eq!(json["duration_seconds"], 2.0);
        assert_eq!(json["speed_bps"], 5.0);
        assert!(json.get("error").is_none(), "absent fields must be omitted");
    }

    // ─── retention ──────────────────────────────────────────────────────────

    /// The API's current policy, and the shape it arrives in.
    #[test]
    fn the_api_backup_block_parses() {
        let config: RepoConfig = serde_json::from_value(serde_json::json!({
            "repo": "rest:https://vol:pw@host/vol/",
            "restic_password": "secret",
            "keep": { "last": 30, "hourly": 24, "daily": 30 }
        }))
        .unwrap();

        assert_eq!(config.keep.last, Some(30));
        assert_eq!(config.keep.hourly, Some(24));
        assert_eq!(config.keep.daily, Some(30));
        assert!(config.keep.is_meaningful());
    }

    /// An empty policy would ask the engine to forget everything.
    #[test]
    fn an_empty_retention_policy_is_refused_by_the_caller() {
        assert!(!Retention::default().is_meaningful());
        let partial = Retention {
            last: Some(1),
            ..Default::default()
        };
        assert!(partial.is_meaningful());
    }

    #[test]
    fn a_backup_block_without_keep_still_parses() {
        let config: RepoConfig = serde_json::from_value(serde_json::json!({
            "repo": "rest:https://host/vol/",
            "restic_password": "secret"
        }))
        .unwrap();
        assert!(!config.keep.is_meaningful());
    }
}

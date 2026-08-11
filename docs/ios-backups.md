# iOS Backups

## Overview

A world played on a phone has to become the newest snapshot, or the next
desktop launch restores over it and that session is gone. That is the failure
this exists to prevent, and it is cross-device by nature: one repository, many
clients, restic's format on all of them.

iOS cannot spawn a process, so where Android runs the restic binary this host
links `rustic_core` into the app. That is the only difference. Every
*decision* — what to restore and why, whether the lease permits a launch, what
a failure means, the exact `backup-state` body — is `homerun-core::backup`,
reached through `Core`. The host gathers facts and carries out answers.

Two consequences of linking rather than spawning are worth knowing before
reading anything else:

- **There is no exit code.** `Failure::succeeded()` is reachable only from
  restic's exit 3, so on this platform a failure can never classify as
  success. A snapshot came back or it did not.
- **rustic does no repository locking.** It neither writes a lock nor notices
  one a desktop client left. The backup lease is the only thing keeping two
  devices out of one repository.

## The lease, and the rule everything else follows from

The API opens the backup lease when this device acks `stopped` with
`backup_in_progress`, and closes it when this device reports `backup-state` —
success or failure alike. **The lease has no timeout.**

A device that claims it and never reports locks every other device out of that
world until its own next `running` ack. The escape hatch is a force-launch,
which shows the user a data-loss warning for what is usually just a phone that
got closed.

So the rule is: **every precondition is decided before the ack, and after the
ack every path reports.** `PumpkinBackend.finish` answers "will we back up"
once, into one variable, and that variable is what the ack carries.
`BackupManager.backupAfterStop` takes non-optional parameters so there is no
"nothing to do here" early return available to write — Android decides the
same question in two places and leaks the lease on two paths as a result.

## `BackupManager.swift`

Three moments, matching the desktop and Android.

**Before launch — restore.** Runs in `PumpkinBackend.start`, after the
directory exists and *before* the engine thread starts: a restore loads the
whole repository index into memory, and doing that beside a running Pumpkin is
how a phone gets jetsammed. A failure aborts the launch, which is correct —
starting on a world we have been told is stale is the divergence this prevents.

Failure to *list* snapshots is not a failure to launch. No signal, a backend
hiccup and an empty repository are indistinguishable from here and all mean the
same thing to the core: nothing to compare against. A phone on a train must
still be able to host.

**Before launch — the lease gate.** In `BridgeRouter+Server.nativeServerStart`,
before `backend.start`. A blocked launch returns `{success: false, error: …}`
with a sentence for the player, *and* emits
`native-server-backup-lease-blocked`. The player can wait or re-issue the start
with `force: true`.

**After a clean stop — the snapshot.** In `PumpkinBackend.finish`, which is the
single teardown funnel for both a stop and a crash. A crash is never backed up:
the world was not shut down cleanly, and pushing it over a good snapshot is how
a corrupted save spreads.

### Why the world is moved aside, not deleted

A restore that fails halfway must not take the world with it. Android deletes
`world/` and then moves the restored copy into place, which leaves a window —
a crash, a full disk, a dropped connection — where the player has neither copy.

This renames instead. Same volume, one inode operation, and the old world is
still there to put back if the restore fails. It does mean both copies exist at
once, which on a phone with a large world is a real constraint: transient disk
in exchange for never being the reason someone lost a world.

### Foreground, with the screen awake

iOS suspends a backgrounded app within seconds and a world upload is minutes.
There is no background mode that covers this, and declaring one we cannot
honour is an App Review rejection — which is why `Info.plist` has no
`UIBackgroundModes` and says so.

So the backup runs while the app is in front of the player, with progress on
screen and `isIdleTimerDisabled` set so a screen lock does not start the
suspension clock. A background-task assertion is taken anyway, unconditionally
and at the start rather than on `didEnterBackground` — by then the seconds it
buys are already gone. Its expiry handler has about five seconds, which is not
enough to finish and is exactly enough to cancel and let the report close the
lease.

### The outbox

`HostStore.pendingBackupReports`. The report body is written to disk *before*
the POST is attempted and cleared when it succeeds; `AppDelegate` flushes
leftovers at launch.

Android has no equivalent — it fires the report into a coroutine and forgets.
On a foreground service that mostly survives. Here it would be a request racing
app suspension, which is the exact case that drops one, and a dropped report
leaves the lease open with no timeout.

## `FFI/BackupFFI.swift`

Separate from `HomerunFFI` because these calls behave differently from
everything else crossing that boundary. `homerun_core_call` and the server
getters answer instantly; two of these open TLS connections and block for
minutes.

`run` and `latestSnapshot` go on **dedicated threads**, not `Task.detached` —
an iOS cooperative-pool thread gets 512 KB and the engine's tree walk and
worker pool do not fit. The same lesson, and the same silent stack overflow, as
the server thread. `Task` does not let you set a stack size.

Progress is polled by cursor, exactly like the server console. A callback would
fire from a rayon worker thread and leave a context pointer's lifetime to
manage across a cancelled job.

## The one thing that is resolved in Rust, not here

The restore selector. A snapshot records the *writing* device's absolute path —
a desktop's looks like `/home/you/.homerun/servers/<id>` or
`C:\Users\You\…\servers\<id>`, nothing like an iOS container path. Building a
selector from *our* path would work only for the case that needs it least.

So the host passes a `serverId` and the engine resolves it from the snapshot's
own `paths`, using `backup::recorded_basename` to pick the right one and
`backup::internal_path` to fold a drive letter so a `SNAP:PATH` selector cannot
split on the wrong colon. Both helpers were written for exactly this and, until
this landed, used by nobody.

## File map

| File | Role |
|---|---|
| `BackupManager.swift` | The three moments, the outbox, and the foreground assertion. |
| `FFI/BackupFFI.swift` | The engine over the C ABI; threads, progress polling, cancellation. |
| `FFI/Core.swift` | `backup.*` wrappers — every decision goes through here. |
| `PumpkinBackend.swift` | Restore in `start`; the ack and the on-stop backup in `finish`. |
| `BridgeRouter+Server.swift` | The lease gate, and building `BackupContext`. |
| `HostStore.swift` | The pending-report outbox. |
| `rust/homerun-pumpkin-ffi/src/backup_engine.rs` | The engine. See [`ffi.md`](./ffi.md#backups--backup_jobrs-backup_enginers). |

## Triage

**A server will not start, and says another device is backing it up.** Working
as intended: another device holds the lease. Re-issue `native-server-start`
with `force: true` to take it, accepting the data-loss warning. If no other
device is actually running, the lease was leaked — look for a device that
stopped a server and never reported `backup-state`.

**Backups never run.** Check `homerun_backup_available()` in the launch log; 0
means the build has no engine (the `backup-engine` cargo feature is off — see
`scripts/targets.js`). If it is 1, check that the API is sending a `backup`
block on `GET /api/server/<id>/`; without one this host runs without backups by
design.

**A world "reverted" after a launch.** The snapshot's hostname is not this
device's registry id, so `restore_decision` believed another device wrote the
newest snapshot and restored over local work. The hostname is set from
`deviceId` at backup time; if it is wrong, everything downstream is.

**A backup is reported failed but the world looks backed up.** A linked engine
cannot report "completed with warnings" — that verdict is reachable only from
restic's exit 3. Check `warnings` in the engine reply.

**A restore is refused with "that backup does not contain this server".** The
snapshot holds a different server's directory. The message lists what it does
hold.

**Everything is slow and the screen keeps dimming.** `isIdleTimerDisabled` is
only set for the duration of a backup. If a backup is not running, that is not
this.

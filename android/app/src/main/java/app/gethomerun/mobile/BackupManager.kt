package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.serialization.json.JsonObject
import java.io.File

/**
 * The backup lifecycle around a server run.
 *
 * Three moments, matching the desktop's `nativeServerManager`:
 *
 *  - **before launch** — restore if another device's world is newer, or if
 *    ours is gone ([restoreBeforeLaunch])
 *  - **before launch** — refuse to start while another device is mid-backup
 *    ([leaseAllows])
 *  - **after a clean stop** — snapshot the world ([backupAfterStop])
 *
 * # Nothing here decides anything
 *
 * Every judgement is `homerun-core::backup`, reached through [Core]: what to
 * restore and why, whether the lease permits a launch, whether a failure is
 * retryable, and the exact body of the state report. This class gathers facts
 * — is there a world on disk, what does the repository hold — and carries out
 * the answer. That is what lets iOS reuse the same rules with a linked engine
 * instead of a spawned one.
 *
 * # Why the lease matters more than it looks
 *
 * The API opens it when a device says it is about to back up, and closes it
 * when that device reports the outcome. There is no timeout. So a phone that
 * is swipe-closed mid-backup leaves the lease open until its own next launch,
 * and the *other* device sees a data-loss warning for what is really a dead
 * phone. That is a known gap while backups are on-stop only.
 */
class BackupManager(
    private val context: Context,
    private val engine: ResticEngine = ResticEngine(context),
) {

    /**
     * Whether the world directory has anything in it.
     *
     * This is the desktop's `hasLocalWorld`, and it is deliberately *not* the
     * desktop's "is the server directory empty" test. Mobile writes
     * `eula.txt`, `server.properties`, `ops.json` and the jar before launch,
     * so the directory is never empty here — using that test would mean a
     * wiped device silently starts an empty world instead of restoring its own
     * backup.
     */
    fun hasLocalWorld(dir: File): Boolean =
        WORLD_DIRS.any { name ->
            File(dir, name).let { it.isDirectory && (it.list()?.isNotEmpty() == true) }
        }

    /**
     * Whether this device may start, given who holds the lease.
     *
     * Returns null to launch, or a player-facing sentence explaining why not.
     */
    fun leaseBlockedReason(
        settings: HomerunApi.ServerSettings,
        deviceId: String,
        force: Boolean,
    ): String? =
        when (val decision = Core.leaseDecision(settings.backupLeaseDevice, deviceId, force)) {
            is Core.Lease.Launch -> null
            is Core.Lease.Forced -> {
                Log.w(TAG, "took the backup lease from ${decision.takenFrom}")
                null
            }
            is Core.Lease.Blocked ->
                "Another device is still backing this world up. Wait for it to finish, " +
                    "or start anyway to take over — which may lose that backup."
        }

    /**
     * Restore before launch, if the core says to.
     *
     * Throws only when a restore was required and failed: launching over a
     * world we were told is stale would quietly diverge two devices, which is
     * the failure this whole subsystem exists to prevent. Everything else —
     * no repository, no snapshots, no signal — falls through to the local
     * world, because a device with no connectivity must still be able to host.
     */
    suspend fun restoreBeforeLaunch(
        serverId: String,
        dir: File,
        settings: HomerunApi.ServerSettings,
        deviceId: String,
        onLog: (String) -> Unit,
    ) {
        val repo = ResticEngine.Repo.from(settings.backup) ?: return
        if (!engine.isAvailable) {
            Log.i(TAG, "no backup engine in this build — starting with local data")
            return
        }

        val quiet: (String) -> Unit = { Log.d(TAG, it) }
        val latest = runCatching { engine.latestSnapshot(repo, dir, quiet) }.getOrNull()

        when (val decision = Core.restoreDecision(
            pinned = settings.restoreFromSnapshot,
            latest = latest,
            deviceId = deviceId,
            hasLocalWorld = hasLocalWorld(dir),
        )) {
            is Core.Restore.Skip -> Log.i(TAG, "$serverId: keeping local world (${decision.reason})")

            is Core.Restore.Rollback -> {
                onLog("[Backup] Rolling back to snapshot ${decision.snapshotId}…")
                pull(serverId, repo, dir, decision.snapshotId, onLog)
                onLog("[Backup] Rollback complete.")
            }

            is Core.Restore.Latest -> {
                onLog(
                    if (decision.reason == "anotherDeviceIsNewer")
                        "[Backup] Restoring the latest world (backed up by another device)…"
                    else "[Backup] No world here — restoring from backup…"
                )
                pull(serverId, repo, dir, decision.snapshotId, onLog)
                onLog("[Backup] World restored.")
            }
        }
    }

    /**
     * Restore a snapshot and move the world into place.
     *
     * restic records absolute paths from the writing device, so a desktop's
     * snapshot arrives as `C:\Users\…\servers\<id>`. Restoring into a scratch
     * directory and relocating by name is what makes a Windows→Android handoff
     * work at all.
     */
    private suspend fun pull(
        serverId: String,
        repo: ResticEngine.Repo,
        dir: File,
        snapshotId: String,
        onLog: (String) -> Unit,
    ) {
        val scratch = File(context.cacheDir, "restore-$serverId").apply {
            deleteRecursively()
            mkdirs()
        }

        val outcome = engine.restore(repo, dir, snapshotId, scratch) { Log.d(TAG, it); Unit }
        report(serverId, "restore", outcome)
        if (!outcome.ok) {
            scratch.deleteRecursively()
            throw ServerBackendException.Engine(
                "The world could not be restored, so the server was not started."
            )
        }

        val source = findServerTree(scratch, serverId)
        if (source == null) {
            scratch.deleteRecursively()
            throw ServerBackendException.Engine(
                "The restored backup did not contain this server's world."
            )
        }

        WORLD_DIRS.forEach { File(dir, it).deleteRecursively() }
        source.listFiles()?.forEach { entry ->
            val destination = File(dir, entry.name)
            destination.deleteRecursively()
            if (!entry.renameTo(destination)) entry.copyRecursively(destination, overwrite = true)
        }
        scratch.deleteRecursively()
    }

    /**
     * Locate the server's own directory inside a restored tree.
     *
     * The recorded path's last segment is the server id, so that is what is
     * searched for — the intervening directories are the writing device's and
     * mean nothing here.
     */
    private fun findServerTree(root: File, serverId: String): File? {
        root.walkTopDown().maxDepth(MAX_RESTORE_DEPTH).forEach { candidate ->
            if (candidate.isDirectory && candidate.name == serverId) return candidate
        }
        // Fall back to any directory that looks like a world, in case the
        // recorded id and ours differ (a server moved between accounts).
        root.walkTopDown().maxDepth(MAX_RESTORE_DEPTH).forEach { candidate ->
            if (candidate.isDirectory && WORLD_DIRS.any { File(candidate, it).isDirectory }) {
                return candidate
            }
        }
        return null
    }

    /**
     * Snapshot the world after a clean stop.
     *
     * The caller must already have told the API `backup_in_progress`, which is
     * what opened the lease — so this **must** report an outcome either way,
     * or the lease never closes.
     */
    suspend fun backupAfterStop(
        serverId: String,
        dir: File,
        settings: HomerunApi.ServerSettings,
        deviceId: String,
        onLog: (String) -> Unit,
    ) {
        val repo = ResticEngine.Repo.from(settings.backup) ?: return
        if (!engine.isAvailable) return

        if (!Core.shouldBackUp(hasLocalWorld(dir))) {
            onLog("[Backup] No world to back up — skipping, to protect the existing backup.")
            // Still reported: the lease was opened on the stop ack and only a
            // backup-state report closes it.
            report(serverId, "backup", ResticEngine.Outcome(
                ok = false, snapshotId = null, bytes = 0, durationSeconds = 0.0,
                error = "no world to back up",
            ))
            return
        }

        onLog("[Backup] Backing up the world…")
        val quiet: (String) -> Unit = { Log.d(TAG, it) }
        engine.initIfNeeded(repo, dir, quiet)

        val outcome = engine.backup(repo, dir, dir, deviceId, quiet)
        report(serverId, "backup", outcome)

        if (outcome.ok) {
            onLog("[Backup] Backup complete.")
            runCatching {
                engine.forget(repo, dir, settings.backup?.get("keep") as? JsonObject, quiet)
            }.onFailure { Log.w(TAG, "prune failed: ${it.message}") }
        } else {
            onLog("[Backup] Backup failed: ${outcome.error}")
        }
    }

    /** Report an outcome, which for a backup is what releases the lease. */
    private fun report(serverId: String, operation: String, outcome: ResticEngine.Outcome) {
        val report = Core.backupReport(
            operation = operation,
            snapshotId = outcome.snapshotId,
            error = outcome.error,
            bytes = outcome.bytes,
            durationSeconds = outcome.durationSeconds,
        )
        DeviceRegistry.reportBackupState(serverId, report.body)
    }

    private companion object {
        const val TAG = "HomerunBackup"

        /** What the desktop counts as a world. */
        val WORLD_DIRS = listOf("world", "worlds")

        /**
         * A restored tree nests one directory per path segment of the writing
         * device — `/data/data/app/files/servers/<id>` is six. Windows adds a
         * drive segment. Ten is comfortably past any real layout without
         * walking an unbounded tree.
         */
        const val MAX_RESTORE_DEPTH = 10
    }
}

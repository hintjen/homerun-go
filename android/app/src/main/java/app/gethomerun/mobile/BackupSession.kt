package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import java.io.File

/**
 * The backup lifecycle around one server's run, for either backend.
 *
 * [BackupManager] is already engine-neutral — it takes a directory — but the
 * *lifecycle* around it is not trivial and is what a second backend would get
 * wrong: hold the context past the config's scope, restore before anything
 * reads the world, skip the backup on a crash, report `backupInProgress` so
 * the host keeps the foreground service alive, and announce completion even
 * when the backup was cancelled.
 *
 * The order matters and is the same on both:
 *
 *   cancelSuperseded(id)          // a relaunch supersedes a running backup
 *   restore(id, dir, ctx)         // before the engine reads the world
 *   hold(id, ctx)                 // the exit handler will need it
 *   ... the server runs and exits ...
 *   val due = claim(id, outcome)  // null when there is nothing to back up
 *   transition(..., backupInProgress = due != null)
 *   if (due != null) runAfterStop(id, due)
 */
class BackupSession(
    context: Context,
    private val scope: CoroutineScope,
    /** Where a server's files live, as the owning backend spells it. */
    private val dataDir: (String) -> File,
    /** Write a line to the server's console; `[Backup]` lines are ordinary output. */
    private val note: (String, String) -> Unit,
    /**
     * The backend's `onBackupFinished`, read when it fires rather than
     * captured — `ServerHost` assigns it after construction.
     */
    private val onFinished: () -> ((String) -> Unit)?,
    /**
     * Console housekeeping once the backup has written its last line.
     *
     * The log pump outlives the engine on both backends precisely so `[Backup]`
     * lines still reach the UI, so only the session knows when it may stop.
     */
    private val finishConsole: suspend (String) -> Unit,
) {
    private val backups = BackupManager(context)

    /** Contexts for servers that are running, kept until they exit. */
    private val pending = java.util.concurrent.ConcurrentHashMap<String, BackupContext>()

    /**
     * On-stop backups still running, so a relaunch can cancel one.
     *
     * A backup outlives the server it backs up — restic reads `world/` long
     * after the engine is gone — so a start arriving meanwhile has to cancel it
     * rather than race it for the directory.
     */
    private val jobs = java.util.concurrent.ConcurrentHashMap<String, Job>()

    /** Cancel a running backup because this device is relaunching the server. */
    fun cancelSuperseded(serverId: String) {
        val job = jobs.remove(serverId) ?: return
        if (!job.isActive) return
        Log.i(TAG, "$serverId: cancelling the on-stop backup — this device is relaunching")
        note(serverId, "[Backup] Starting again — the backup in progress was cancelled.")
        job.cancel()
    }

    /**
     * Bring back a newer snapshot before anything reads the world.
     *
     * If another device holds a newer one, its world wins over this device's
     * stale copy. A failure here stops the launch rather than starting a server
     * on a world we were told is out of date — quietly diverging two devices is
     * the failure this exists to prevent, so this deliberately does not catch.
     */
    suspend fun restore(serverId: String, dir: File, ctx: BackupContext?) {
        ctx ?: return
        backups.restoreBeforeLaunch(
            serverId = serverId,
            dir = dir,
            settings = ctx.settings,
            deviceId = ctx.deviceId,
            onLog = { note(serverId, it) },
        )
    }

    /** Keep what the exit handler will need, once the caller's config is gone. */
    fun hold(serverId: String, ctx: BackupContext?) {
        ctx?.let { pending[serverId] = it }
    }

    /**
     * The backup this exit is owed, or null.
     *
     * Null on a crash — a world a server died on is not one to push over a good
     * snapshot — and null when there is no local world to read, which is also
     * what stops a server that never got as far as generating one from
     * uploading an empty repository.
     */
    fun claim(serverId: String, outcome: String): BackupContext? =
        pending.remove(serverId)
            ?.takeIf { outcome != "crashed" && backups.hasLocalWorld(dataDir(serverId)) }

    /** Forget a held context without backing anything up. */
    fun discard(serverId: String) {
        pending.remove(serverId)
    }

    /**
     * Run the on-stop backup, and announce when it is done either way.
     *
     * The completion callback is on the job rather than at the end of the block
     * so a *cancelled* backup announces itself too: the host uses it to decide
     * the engine may finally be reclaimed, and a cancellation that stayed
     * silent would pin it in the foreground for the life of the process.
     */
    fun runAfterStop(serverId: String, ctx: BackupContext) {
        val job = scope.launch {
            runCatching {
                backups.backupAfterStop(
                    serverId = serverId,
                    dir = dataDir(serverId),
                    settings = ctx.settings,
                    deviceId = ctx.deviceId,
                    onLog = { note(serverId, it) },
                )
            }.onFailure {
                if (it is CancellationException) throw it
                Log.w(TAG, "on-stop backup failed for $serverId: ${it.message}")
            }
            // The backup's own last line, then the pump's work is done.
            // Cancellation skips this deliberately: the only thing that cancels
            // a backup is a relaunch, and that starts its own.
            finishConsole(serverId)
        }
        jobs[serverId] = job
        job.invokeOnCompletion {
            jobs.remove(serverId, job)
            onFinished()?.invoke(serverId)
        }
    }

    private companion object {
        const val TAG = "HomerunBackupSession"
    }
}

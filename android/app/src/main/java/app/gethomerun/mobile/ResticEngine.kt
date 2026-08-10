package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import java.io.File

/**
 * restic, spawned as a child process.
 *
 * # Why a child process on Android
 *
 * restic is a Go binary and Android can exec one, so it runs outside the app:
 * a backup that runs out of memory or wedges kills a child rather than the
 * server and the UI. iOS cannot spawn anything, so it will need a linked
 * engine — which is exactly why nothing in this file decides anything. Every
 * judgement (what to restore, whether the lease permits a launch, what a
 * failure means) is `homerun-core::backup`, reached through [Core], so the two
 * platforms cannot answer differently.
 *
 * # Two things learned on a device, both of which break backups silently
 *
 * **`TMPDIR` must point inside app-private storage.** restic writes its pack
 * files to the temp directory, and Android's default resolves outside the
 * sandbox — every backup dies with `permission denied` on a path nobody
 * recognises.
 *
 * **Exit 3 is success.** restic cannot read an xattr on `/data/data`, an
 * ancestor of every path we back up, so it warns and exits 3 on *every*
 * Android backup while the snapshot is complete and hash-verified. There is no
 * flag to suppress it: `--exclude-xattr` exists only on `restore`. A host that
 * treats non-zero as failure reports every backup broken while every backup
 * worked. [Core.classifyBackupFailure] knows this; do not re-decide it here.
 */
class ResticEngine(private val context: Context) {

    /** Ships in `jniLibs` and is exec'd from `nativeLibraryDir` — the only
     *  directory API 29+ permits exec from, and the packager only puts
     *  `lib*.so` there. It is a Go executable, not a library. */
    private fun binary(): File? =
        File(context.applicationInfo.nativeLibraryDir, BINARY).takeIf { it.canExecute() }

    /** True when this build ships restic for the device's ABI. */
    val isAvailable: Boolean get() = binary() != null

    /**
     * The repository, as the API handed it down.
     *
     * Neither field is ever logged: `repo` embeds HTTP basic-auth credentials
     * and `password` is the repository's encryption passphrase.
     */
    data class Repo(val url: String, val password: String) {
        companion object {
            /** From `get_backup`, or null when the server has no volume. */
            fun from(block: JsonObject?): Repo? {
                val url = block?.get("repo")?.jsonPrimitive?.contentOrNull ?: return null
                val password = block["restic_password"]?.jsonPrimitive?.contentOrNull ?: return null
                return Repo(url, password)
            }
        }
    }

    data class Outcome(
        val ok: Boolean,
        val snapshotId: String?,
        val bytes: Long,
        val durationSeconds: Double,
        val error: String?,
    )

    /**
     * Run one restic command.
     *
     * Returns the raw exit code and output; classification is the core's, not
     * ours. Never throws for a non-zero exit — that is an outcome, not an
     * exception, and exit 3 is a *success* outcome.
     */
    private suspend fun run(
        repo: Repo,
        dir: File,
        args: List<String>,
        onLog: (String) -> Unit,
    ): Triple<Int, String, String> = withContext(Dispatchers.IO) {
        val binary = binary() ?: return@withContext Triple(
            -1, "", "This build ships no backup engine."
        )

        val tmp = File(dir, "restic-tmp").apply { mkdirs() }
        val cache = File(context.cacheDir, "restic").apply { mkdirs() }

        val process = ProcessBuilder(listOf(binary.absolutePath) + args)
            .directory(dir)
            .also { builder ->
                builder.environment().apply {
                    put("RESTIC_REPOSITORY", repo.url)
                    put("RESTIC_PASSWORD", repo.password)
                    put("RESTIC_CACHE_DIR", cache.absolutePath)
                    // See the class docs. Without this, every backup fails.
                    put("TMPDIR", tmp.absolutePath)
                }
            }
            .start()

        // Both streams are drained concurrently. restic's --json progress can
        // fill a pipe buffer, and a full pipe blocks the process forever.
        val out = StringBuilder()
        val err = StringBuilder()
        val stderr = Thread {
            process.errorStream.bufferedReader().forEachLine { line ->
                synchronized(err) { err.appendLine(line) }
                onLog(line)
            }
        }.apply { start() }

        process.inputStream.bufferedReader().forEachLine { line ->
            synchronized(out) { out.appendLine(line) }
            onLog(line)
        }
        stderr.join()
        val code = process.waitFor()
        Triple(code, out.toString(), err.toString())
    }

    /** Create the repository if it does not exist. Harmless when it does. */
    suspend fun initIfNeeded(repo: Repo, dir: File, onLog: (String) -> Unit) {
        val (code, out, err) = run(repo, dir, listOf("init"), onLog)
        if (code != 0 && !"$out$err".contains("already initialized", ignoreCase = true)) {
            Log.i(TAG, "init said: ${err.trim().takeLast(200)}")
        }
    }

    /**
     * Back up `source`, returning what to report.
     *
     * `--host` carries the **device id**, which is how the API resolves
     * `pushed_by` and how every other device decides whose world is newest. It
     * is not cosmetic: get it wrong and a co-host restores over live work.
     */
    suspend fun backup(
        repo: Repo,
        dir: File,
        source: File,
        deviceId: String,
        onLog: (String) -> Unit,
    ): Outcome {
        val started = System.nanoTime()
        val (code, out, err) = run(
            repo, dir,
            listOf("backup", source.absolutePath, "--host", deviceId, "--json"),
            onLog,
        )
        val seconds = (System.nanoTime() - started) / 1e9

        val summary = out.lineSequence()
            .mapNotNull { runCatching { parser.parseToJsonElement(it).jsonObject }.getOrNull() }
            .lastOrNull { it["message_type"]?.jsonPrimitive?.contentOrNull == "summary" }

        val verdict = Core.classifyBackupFailure(code, "$out\n$err", deviceId)
        val ok = code == 0 || verdict.succeeded

        return Outcome(
            ok = ok,
            snapshotId = summary?.get("snapshot_id")?.jsonPrimitive?.contentOrNull,
            bytes = summary?.get("data_added")?.jsonPrimitive?.longOrNull ?: 0,
            durationSeconds = seconds,
            error = if (ok) null else err.trim().takeLast(400).ifEmpty { "backup failed (${verdict.kind})" },
        )
    }

    /**
     * The newest snapshot in the repository, or null.
     *
     * `--no-lock` throughout: a stale lock left by a crashed run must never
     * block a *read*, and the lease is the real concurrency guard.
     */
    suspend fun latestSnapshot(repo: Repo, dir: File, onLog: (String) -> Unit): JsonObject? {
        val (code, out, _) = run(
            repo, dir,
            listOf("snapshots", "--json", "--latest", "1", "--no-lock"),
            onLog,
        )
        if (code != 0) return null

        val array = runCatching {
            parser.parseToJsonElement(out.lineSequence().first { it.trimStart().startsWith("[") })
        }.getOrNull()

        val snapshot = (array as? kotlinx.serialization.json.JsonArray)
            ?.firstOrNull()?.jsonObject ?: return null

        // Reduced to what the core's decision needs, with restic's field names
        // mapped to the core's.
        return kotlinx.serialization.json.buildJsonObject {
            snapshot["id"]?.let { put("id", it) }
            snapshot["time"]?.let { put("time", it) }
            snapshot["hostname"]?.let { put("host", it) }
            snapshot["paths"]?.let { put("paths", it) }
        }
    }

    /**
     * Restore a snapshot into `target`.
     *
     * The recorded path comes from whichever device wrote it — a Windows
     * desktop's snapshot arrives as `C:\Users\...` — so the host restores into
     * a scratch directory and the caller relocates the subtree by name.
     */
    suspend fun restore(
        repo: Repo,
        dir: File,
        snapshotId: String,
        target: File,
        onLog: (String) -> Unit,
    ): Outcome {
        val started = System.nanoTime()
        val (code, out, err) = run(
            repo, dir,
            listOf("restore", snapshotId, "--target", target.absolutePath, "--no-lock"),
            onLog,
        )
        val seconds = (System.nanoTime() - started) / 1e9
        val verdict = Core.classifyBackupFailure(code, "$out\n$err", "")
        val ok = code == 0 || verdict.succeeded

        return Outcome(
            ok = ok,
            snapshotId = snapshotId,
            bytes = 0,
            durationSeconds = seconds,
            error = if (ok) null else err.trim().takeLast(400).ifEmpty { "restore failed (${verdict.kind})" },
        )
    }

    /**
     * Apply the retention policy the API chose.
     *
     * Skipped entirely when the policy is empty — an empty `forget` asks
     * restic to keep nothing.
     */
    suspend fun forget(repo: Repo, dir: File, keep: JsonObject?, onLog: (String) -> Unit) {
        val args = mutableListOf("forget", "--prune")
        keep?.get("last")?.jsonPrimitive?.contentOrNull?.let { args += listOf("--keep-last", it) }
        keep?.get("hourly")?.jsonPrimitive?.contentOrNull?.let { args += listOf("--keep-hourly", it) }
        keep?.get("daily")?.jsonPrimitive?.contentOrNull?.let { args += listOf("--keep-daily", it) }
        if (args.size == 2) {
            Log.i(TAG, "no retention policy from the API — skipping prune")
            return
        }
        run(repo, dir, args, onLog)
    }

    private companion object {
        const val TAG = "HomerunBackup"
        const val BINARY = "librestic.so"
        val parser = Json { ignoreUnknownKeys = true }
    }
}

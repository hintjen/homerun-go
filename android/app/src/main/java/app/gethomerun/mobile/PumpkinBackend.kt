package app.gethomerun.mobile

import android.app.ActivityManager
import android.content.Context
import android.os.Process
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import java.io.File
import java.time.Instant

/**
 * [ServerBackend] over the Rust FFI, in-process via JNI.
 *
 * Shared with iOS in spirit — the crate underneath is the same — but the
 * threading and the polling are Android's problem, and they are what this
 * class is mostly made of:
 *
 *  - `nativeStart` blocks for the server's whole lifetime, so it runs on its
 *    own 16 MB-stack thread and [start] waits for the state to turn *running*
 *    rather than for the call to return.
 *  - The engine pushes console output into a bounded ring buffer with a
 *    monotonic cursor. Nothing calls us when a line arrives, so a poller
 *    drains it and re-emits as events.
 *
 * One server at a time, enforced in the crate and again here — the engine
 * distinguishes worlds by process CWD, so a second concurrent run would
 * quietly share the first one's world.
 */
class PumpkinBackend(
    private val context: Context,
    private val scope: CoroutineScope,
) : ServerBackend {

    override val kind = "pumpkin"

    private val json = Json { ignoreUnknownKeys = true }

    /** Console cursor. Per-run: the buffer resets when the engine restarts. */
    private var logCursor = 0L

    private var pumpJob: Job? = null
    private var engineThread: Thread? = null

    private var currentServerId: String? = null
    private var currentPort: Int? = null

    /**
     * The last state announced to the UI, not a second opinion about what the
     * server is doing — `homerun-core::lifecycle` owns that. Kept because the
     * core drops a finished run, so it cannot answer "what became of it".
     */
    private var lastAnnounced: ServerState = ServerState.STOPPED

    /**
     * This run's graph, kept by `homerun-core::metrics`. One per run: a graph
     * covers a session, so [start] resets it rather than this being reused.
     */
    private val metrics = Core.Metrics()
    private var perfJob: Job? = null

    override var onStateChanged: ((String, ServerState, Boolean) -> Unit)? = null
    override var onLog: ((String, String) -> Unit)? = null
    override var onPlayersChanged: ((String) -> Unit)? = null

    // Pumpkin has no tunnel of its own yet, so this never fires here.
    override var onNetworkError: ((String, String) -> Unit)? = null

    /**
     * Never fires here either: this backend runs no on-stop backup. That path
     * belongs to [JavaServerBackend], and this one exists for builds that ship
     * no JRE — so a stop here really does mean the device is idle.
     */
    override var onBackupFinished: ((String) -> Unit)? = null

    // -----------------------------------------------------------------------
    // Storage
    // -----------------------------------------------------------------------

    /**
     * `filesDir` is app-private and survives updates. Not `cacheDir` — the
     * system may delete that under storage pressure, and it would take the
     * player's world with it.
     */
    private fun dataDir(serverId: String): File =
        File(context.filesDir, "servers/$serverId").apply { mkdirs() }

    override fun create(serverId: String) {
        dataDir(serverId)
    }

    override fun delete(serverId: String) {
        // Running, coming up or winding down — the core's question, same as
        // `native-server-active-ids` answers.
        if (serverId in ServerHost.lifecycle.activeIds()) {
            throw ServerBackendException.AlreadyRunning(serverId)
        }
        dataDir(serverId).deleteRecursively()
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    override suspend fun start(serverId: String, config: ServerConfig) {
        if (!NativeServer.available) {
            throw ServerBackendException.Engine("The server engine failed to load on this device.")
        }
        // Admission was decided by the core before this was called, in the
        // bridge's start handler — see `homerun-core::lifecycle`.

        val port = (config.extra["port"] as? Int) ?: DEFAULT_PORT
        currentServerId = serverId
        currentPort = port
        logCursor = 0

        startLogPump(serverId)
        startPerfSampler(serverId)

        engineThread = NativeServer.startBlocking(
            serverId,
            dataDir(serverId).absolutePath,
            port,
        ) { result ->
            // The engine has exited — cleanly or not. Report before tearing
            // the pump down so the final lines still reach the UI.
            val ok = parse(result)["ok"]?.jsonPrimitive?.booleanOrNull ?: false
            val error = parse(result)["error"]?.jsonPrimitive?.contentOrNull
            scope.launch {
                drainLogs(serverId)
                stopLogPump()
                stopPerfSampler()
                // Whether this was a stop or a fall-over is the core's call,
                // from whether one was asked for — the engine's own `ok` says
                // only that it unwound cleanly. Exit 0 stands in for a clean
                // unwind; there is no process code to report here.
                val verdict = ServerHost.lifecycle.exited(serverId, if (ok) 0 else 1)
                if (!ok && error != null) Log.e(TAG, "engine exited: $error")
                transition(
                    serverId,
                    if (verdict.state == "crashed") ServerState.CRASHED else ServerState.STOPPED,
                )
                currentServerId = null
                currentPort = null
            }
        }
        // The engine is up; from here its exit needs judging.
        ServerHost.lifecycle.spawned(serverId)

        // `start` is contracted to return once the server accepts connections,
        // and the bridge has no timeout, so waiting here is correct. The cap
        // exists only so a wedged engine reports rather than hangs forever.
        val deadline = System.currentTimeMillis() + START_TIMEOUT_MS
        while (System.currentTimeMillis() < deadline) {
            when (status(serverId)) {
                ServerState.RUNNING -> {
                    ServerHost.lifecycle.consoleReady(serverId)
                    transition(serverId, ServerState.RUNNING)
                    return
                }
                ServerState.CRASHED -> throw ServerBackendException.Engine(
                    "The server stopped unexpectedly while starting."
                )
                else -> delay(POLL_MS)
            }
        }
        throw ServerBackendException.Engine("The server did not finish starting in time.")
    }

    /**
     * [graceful] is unused: the engine's own stop always saves before it
     * unwinds, and there is no second, harsher way to ask it. The parameter
     * stays so every backend answers the same question.
     */
    override suspend fun stop(serverId: String, graceful: Boolean) {
        if (currentServerId != serverId) throw ServerBackendException.NotRunning(serverId)
        transition(serverId, ServerState.STOPPING)
        // Blocking, and it waits for a world save — never on the main thread.
        val result = withContext(Dispatchers.IO) { NativeServer.nativeStop() }
        val ok = parse(result)["ok"]?.jsonPrimitive?.booleanOrNull ?: false
        if (!ok) {
            val error = parse(result)["error"]?.jsonPrimitive?.contentOrNull
            throw ServerBackendException.Engine(error ?: "The server could not be stopped.")
        }
    }

    /**
     * The core's list, not the engine's.
     *
     * [status] asks the engine directly, which is the right primary source for
     * a launch waiting to see `running` — but it is a *third* answer to "what
     * is this device hosting", after the core's and the last announced state.
     * This one goes to the API as `instances` and to the UI on reconnect, so
     * it has to be the same one everything else uses.
     */
    override val runningServerIds: List<String>
        get() = ServerHost.lifecycle.runningIds()


    // -----------------------------------------------------------------------
    // Introspection
    // -----------------------------------------------------------------------

    override fun status(serverId: String): ServerState {
        if (!NativeServer.available || currentServerId != serverId) return ServerState.STOPPED
        val wire = parse(NativeServer.nativeState())["state"]?.jsonPrimitive?.contentOrNull
        return ServerState.entries.firstOrNull { it.wire == wire } ?: ServerState.STOPPED
    }

    override fun players(serverId: String): PlayerRoster? {
        if (currentServerId != serverId) return null
        val raw = NativeServer.nativePlayers()
        if (raw.trim() == "null") return null
        val obj = parse(raw)
        val list = obj["players"]?.jsonArray?.map {
            val p = it.jsonObject
            PlayerRoster.Player(
                name = p["name"]?.jsonPrimitive?.contentOrNull.orEmpty(),
                uuid = p["uuid"]?.jsonPrimitive?.contentOrNull,
            )
        } ?: emptyList()
        return PlayerRoster(list, obj["max"]?.jsonPrimitive?.intOrNull)
    }

    override fun uptime(serverId: String): Instant? {
        if (currentServerId != serverId) return null
        val ms = parse(NativeServer.nativeStats())["startedAtMs"]?.jsonPrimitive?.longOrNull
        return ms?.let(Instant::ofEpochMilli)
    }

    /**
     * The engine runs in this process, so process memory is the honest figure
     * — there is no separate server process to measure.
     *
     * Resident set, not `Debug.getNativeHeapAllocatedSize()`: the heap figure
     * is the allocator's own bookkeeping, where RSS is what the OS accounts
     * against this app and what it kills on. It is also what the graph is now
     * drawn from, and a gauge that disagrees with the graph beside it is worse
     * than either number alone.
     *
     * The ceiling is still `largeMemoryClass`, so RSS — which counts more
     * than the heap — can read over 100% of it. Pre-existing on the JVM path.
     * iOS reports its own equivalent, the limit the app is killed for
     * exceeding, so the two platforms' gauges ask the same question even
     * though neither number is directly comparable to the other's.
     */
    override fun memoryUsage(serverId: String): MemoryUsage? {
        if (currentServerId != serverId) return null
        val manager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        return MemoryUsage(
            usedKb = ProcMetrics.residentKb(Process.myPid().toLong())?.toInt(),
            maxMb = manager.largeMemoryClass,
        )
    }

    /**
     * The most recent rate the core worked out, which is the graph's last
     * point. Never measured on demand: a percentage is a difference between
     * two moments, and letting a UI poll choose those two is how the graph and
     * the gauge end up disagreeing.
     *
     * Null until two readings exist, which renders as "unavailable".
     */
    override fun cpuUsage(serverId: String): Double? {
        if (currentServerId != serverId) return null
        return metrics.samples().lastOrNull()?.cpuPercent
    }

    override fun port(serverId: String): Int? =
        if (currentServerId == serverId) currentPort else null

    override fun logs(serverId: String, cursor: Int): LogSlice {
        if (currentServerId != serverId) return LogSlice(emptyList(), cursor)
        val obj = parse(NativeServer.nativeLogsSince(cursor.toLong()))
        val lines = obj["lines"]?.jsonArray?.mapNotNull { it.jsonPrimitive.contentOrNull } ?: emptyList()
        val next = obj["cursor"]?.jsonPrimitive?.longOrNull ?: cursor.toLong()
        return LogSlice(lines, next.toInt())
    }

    override suspend fun command(serverId: String, command: String) {
        if (currentServerId != serverId) throw ServerBackendException.NotRunning(serverId)
        val result = withContext(Dispatchers.IO) { NativeServer.nativeCommand(command) }
        val ok = parse(result)["ok"]?.jsonPrimitive?.booleanOrNull ?: false
        if (!ok) {
            throw ServerBackendException.Engine(
                parse(result)["error"]?.jsonPrimitive?.contentOrNull ?: "Command failed."
            )
        }
    }

    override fun perfHistory(serverId: String): List<PerfSample> =
        if (currentServerId != serverId) emptyList()
        else metrics.samples().map {
            PerfSample(it.t, it.memUsedMb, it.cpuPercent, it.playerCount)
        }

    // -----------------------------------------------------------------------
    // Log pump
    // -----------------------------------------------------------------------

    private fun startLogPump(serverId: String) {
        stopLogPump()
        pumpJob = scope.launch(Dispatchers.IO) {
            while (true) {
                drainLogs(serverId)
                pollPlayers(serverId)
                delay(POLL_MS)
            }
        }
    }

    private fun stopLogPump() {
        pumpJob?.cancel()
        pumpJob = null
    }

    private fun drainLogs(serverId: String) {
        val obj = parse(NativeServer.nativeLogsSince(logCursor))
        val lines = obj["lines"]?.jsonArray?.mapNotNull { it.jsonPrimitive.contentOrNull } ?: return
        val dropped = obj["dropped"]?.jsonPrimitive?.booleanOrNull ?: false
        if (dropped) {
            // The buffer is bounded; say so rather than let the console look
            // like it simply skipped ahead.
            onLog?.invoke(serverId, "[Homerun] …earlier console output was dropped…")
        }
        logCursor = obj["cursor"]?.jsonPrimitive?.longOrNull ?: logCursor
        for (line in lines) onLog?.invoke(serverId, line)
    }

    private var lastPlayerCount = -1

    private fun pollPlayers(serverId: String) {
        val count = players(serverId)?.players?.size ?: 0
        if (count != lastPlayerCount) {
            lastPlayerCount = count
            onPlayersChanged?.invoke(serverId)
        }
    }

    // -----------------------------------------------------------------------
    // Perf sampler
    // -----------------------------------------------------------------------

    /**
     * Feed the core a reading for as long as this run lasts.
     *
     * Its own job rather than a second passenger on the log pump, which ticks
     * every second: the history crosses JNI *by value*, so offering there
     * would ship up to 360 samples in each direction thirty times over for one
     * kept point. The pump keeps the roster watch, which does need to be
     * prompt. Same shape as [JavaServerBackend]'s sampler, so the two Android
     * backends' numbers describe the same window.
     *
     * The engine runs **in this process**, so the process to measure is the
     * app itself — unlike the JVM backend, which has a child to point at.
     */
    private fun startPerfSampler(serverId: String) {
        stopPerfSampler()
        metrics.reset()
        perfJob = scope.launch(Dispatchers.IO) {
            val pid = Process.myPid().toLong()
            while (true) {
                metrics.record(
                    atMs = System.currentTimeMillis(),
                    memUsedKb = ProcMetrics.residentKb(pid),
                    cpuSeconds = ProcMetrics.cpuSeconds(pid),
                    playerCount = players(serverId)?.players?.size,
                )
                // Re-read every pass: it doubles once the graph fills.
                delay(metrics.intervalMs())
            }
        }
    }

    private fun stopPerfSampler() {
        perfJob?.cancel()
        perfJob = null
    }

    private fun transition(serverId: String, state: ServerState) {
        // The same guard the JVM backend has, and for the same reason: a
        // launch still catching up must not announce `running` for a server
        // already on its way down. This backend was missing it.
        //
        // The core answers *may this be said*; the check below answers *have
        // we already said it*, which is about the event stream rather than the
        // server, and is this file's own business.
        if (!ServerHost.lifecycle.mayAnnounce(serverId, state.wire)) return
        if (lastAnnounced == state) return
        lastAnnounced = state
        onStateChanged?.invoke(serverId, state, false)
    }

    private fun parse(raw: String): JsonObject =
        runCatching { json.parseToJsonElement(raw).jsonObject }.getOrElse { JsonObject(emptyMap()) }

    private companion object {
        const val TAG = "HomerunBackend"
        const val DEFAULT_PORT = 25565
        const val POLL_MS = 1000L
        const val START_TIMEOUT_MS = 120_000L
    }
}

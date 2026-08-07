package app.gethomerun.mobile

import android.app.ActivityManager
import android.content.Context
import android.os.Debug
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
    private var lastState: ServerState = ServerState.STOPPED

    private val perf = ArrayDeque<PerfSample>()

    override var onStateChanged: ((String, ServerState) -> Unit)? = null
    override var onLog: ((String, String) -> Unit)? = null
    override var onPlayersChanged: ((String) -> Unit)? = null

    data class PerfSample(val t: Long, val memUsedMb: Int?, val cpuPercent: Int?, val playerCount: Int?)

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
        if (currentServerId == serverId && lastState != ServerState.STOPPED) {
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
        val running = runningServerIds
        if (running.isNotEmpty()) {
            if (running.contains(serverId)) throw ServerBackendException.AlreadyRunning(serverId)
            throw ServerBackendException.AnotherServerRunning(running.first())
        }

        val port = (config.extra["port"] as? Int) ?: DEFAULT_PORT
        currentServerId = serverId
        currentPort = port
        logCursor = 0
        perf.clear()

        startLogPump(serverId)

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
                val end = if (ok) ServerState.STOPPED else ServerState.CRASHED
                if (!ok && error != null) Log.e(TAG, "engine exited: $error")
                transition(serverId, end)
                currentServerId = null
                currentPort = null
            }
        }

        // `start` is contracted to return once the server accepts connections,
        // and the bridge has no timeout, so waiting here is correct. The cap
        // exists only so a wedged engine reports rather than hangs forever.
        val deadline = System.currentTimeMillis() + START_TIMEOUT_MS
        while (System.currentTimeMillis() < deadline) {
            when (status(serverId)) {
                ServerState.RUNNING -> {
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

    override suspend fun stop(serverId: String) {
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

    override val runningServerIds: List<String>
        get() {
            val id = currentServerId ?: return emptyList()
            return if (status(id) == ServerState.RUNNING) listOf(id) else emptyList()
        }

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
     * — there is no separate server process to measure. Native heap rather
     * than JVM heap: the server is Rust.
     */
    override fun memoryUsage(serverId: String): MemoryUsage? {
        if (currentServerId != serverId) return null
        val manager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        return MemoryUsage(
            usedKb = (Debug.getNativeHeapAllocatedSize() / 1024).toInt(),
            maxMb = manager.largeMemoryClass,
        )
    }

    /**
     * Not reported. Per-process CPU needs sampling `/proc/self/stat` over an
     * interval, and a wrong number here becomes a wrong graph in the UI —
     * null renders as "unavailable", which is true.
     */
    override fun cpuUsage(serverId: String): Double? = null

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

    /** Recent performance samples, newest last. */
    fun perfHistory(serverId: String): List<PerfSample> =
        if (currentServerId == serverId) perf.toList() else emptyList()

    // -----------------------------------------------------------------------
    // Log pump
    // -----------------------------------------------------------------------

    private fun startLogPump(serverId: String) {
        stopLogPump()
        pumpJob = scope.launch(Dispatchers.IO) {
            while (true) {
                drainLogs(serverId)
                sample(serverId)
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

    private fun sample(serverId: String) {
        val roster = players(serverId)
        val count = roster?.players?.size ?: 0
        if (count != lastPlayerCount) {
            lastPlayerCount = count
            onPlayersChanged?.invoke(serverId)
        }
        perf.addLast(
            PerfSample(
                t = System.currentTimeMillis(),
                memUsedMb = memoryUsage(serverId)?.usedKb?.div(1024),
                cpuPercent = null,
                playerCount = count,
            )
        )
        // Same 30-minute window the desktop sampler keeps.
        while (perf.size > PERF_SAMPLES) perf.removeFirst()
    }

    private fun transition(serverId: String, state: ServerState) {
        if (lastState == state) return
        lastState = state
        onStateChanged?.invoke(serverId, state)
    }

    private fun parse(raw: String): JsonObject =
        runCatching { json.parseToJsonElement(raw).jsonObject }.getOrElse { JsonObject(emptyMap()) }

    private companion object {
        const val TAG = "HomerunBackend"
        const val DEFAULT_PORT = 25565
        const val POLL_MS = 1000L
        const val START_TIMEOUT_MS = 120_000L
        const val PERF_SAMPLES = 1800
    }
}

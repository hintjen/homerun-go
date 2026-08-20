package app.gethomerun.mobile

import android.app.ActivityManager
import android.content.Context
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.add
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import java.io.File
import java.time.Instant

/**
 * [ServerBackend] running Pumpkin as a **child process**.
 *
 * # Why not in-process, when the library can link it
 *
 * `homerun-pumpkin-ffi` can compile Pumpkin straight into the app, and on iOS
 * it must, because that platform cannot spawn anything. Android can, and every
 * consequence of not doing so is one this app was paying for:
 *
 *  - An engine fault took the **whole app** down — WebView, bridge, foreground
 *    service. `catch_unwind` can hold that line for a Rust panic and for
 *    nothing else.
 *  - Memory could only ever be reported as this process, because there was no
 *    other process to measure. The number included the browser engine.
 *  - The engine picks its world by `set_current_dir`, so that choice was
 *    global to the app.
 *  - stdout and stderr had to be captured with a permanent, process-wide
 *    `dup2`, after which the host's own printing landed in the game console.
 *
 * So this backend now looks much more like [JavaServerBackend] than it used
 * to: it composes an [JavaProcess.Invocation] and hands it to the same
 * supervisor, which owns the state machine, the stop ladder, the console and
 * the sampling for a child process of any kind. The binary is
 * `libpumpkin.so` — named that way because Android packages only `lib*.so`
 * from `jniLibs`, and API 29+ execs only from `nativeLibraryDir`.
 *
 * One server at a time, enforced in the crate and again in `ServerHost`.
 */
class PumpkinBackend(
    private val context: Context,
    private val scope: CoroutineScope,
) : ServerBackend {

    override val kind = "pumpkin"

    /**
     * A child process now, not a linked library.
     *
     * This is *not* what decides that Pumpkin runs vanilla only — that follows
     * from the engine and is `homerun-core::minecraft::hosting`'s call, keyed
     * on the game type. Reading it off this field is what broke the moment
     * Pumpkin stopped being the linked one.
     */
    override val engine = "spawned"

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

    override var onStateChanged: ((String, ServerState, Boolean) -> Unit)? = null
    override var onLog: ((String, String) -> Unit)? = null
    override var onPlayersChanged: ((String) -> Unit)? = null
    override var onNetworkError: ((String, String) -> Unit)? = null
    override var onBackupFinished: ((String) -> Unit)? = null

    /**
     * The gateway tunnel and the backup lifecycle, both shared with
     * [JavaServerBackend].
     *
     * Neither is engine-specific: a tunnel forwards a TCP port and restic
     * reads a `world/` directory, and which program wrote that directory is
     * not something either can tell. They are shared because they were briefly
     * *not* — this backend shipped without them, so a Pumpkin server ran
     * unreachable and never backed anything up, while every surface reported
     * it healthy.
     */
    private val tunnel = TunnelSession(
        context = context,
        scope = scope,
        note = ::note,
        onNetworkError = { onNetworkError },
        stopServer = { id, graceful -> stop(id, graceful) },
    )

    private val backups = BackupSession(
        context = context,
        scope = scope,
        dataDir = ::dataDir,
        note = ::note,
        onFinished = { onBackupFinished },
        // The pump outlives the engine so `[Backup]` lines still reach the UI;
        // only the backup knows when there is genuinely nothing left to write.
        finishConsole = { id -> drainLogs(id); stopLogPump() },
    )

    // -----------------------------------------------------------------------
    // Storage
    // -----------------------------------------------------------------------

    /**
     * `filesDir` is app-private and survives updates. Not `cacheDir` — the
     * system may delete that under storage pressure, and it would take the
     * player's world with it.
     *
     * Every path this backend builds from an id goes through here, which is
     * what makes it the place to check one — same as [JavaServerBackend], so
     * the two engines cannot come to disagree about which ids are real. See
     * [requireValidServerId].
     */
    private fun dataDir(serverId: String): File =
        File(context.filesDir, "servers/${requireValidServerId(serverId)}").apply { mkdirs() }

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
        val binary = binary(context)
            ?: throw ServerBackendException.Engine("This build cannot host this kind of server.")
        // Admission was decided by the core before this was called, in the
        // bridge's start handler — see `homerun-core::lifecycle`.

        val port = (config.extra["port"] as? Int) ?: DEFAULT_PORT
        val dir = dataDir(serverId)
        currentServerId = serverId
        currentPort = port
        logCursor = 0

        // A relaunch supersedes a backup still reading the world it is about
        // to overwrite. The core answers whether this start is that.
        if (ServerHost.lifecycle.supersedesOnStopBackup(serverId)) {
            backups.cancelSuperseded(serverId)
        }

        startLogPump(serverId)

        // Started now and awaited once the engine is up. The gateway
        // provisions the peer asynchronously and polls for up to a minute, so
        // beginning here overlaps that with the world restore and the world
        // generating rather than adding it to the end of the launch.
        tunnel.begin(config.resolveTunnel)

        // Before anything reads the world — a newer snapshot from another
        // device has to win over this device's stale copy, and after the
        // engine has opened the directory it is too late. Deliberately not
        // caught: starting on a world we were told is out of date is the
        // failure this exists to prevent.
        try {
            backups.restore(serverId, dir, config.backupContext)
        } catch (err: Throwable) {
            tunnel.cancel()
            stopLogPump()
            currentServerId = null
            currentPort = null
            transition(serverId, ServerState.STOPPED)
            throw err as? ServerBackendException ?: ServerBackendException.Engine(
                err.message ?: "This server's world could not be restored."
            )
        }

        writeSettings(serverId, dir, config)

        // Held for the exit path, which needs the repository and device id
        // long after the caller's config has gone out of scope.
        backups.hold(serverId, config.backupContext)

        val invocation = JavaProcess.Invocation(
            program = binary.absolutePath,
            // None. The server reads `pumpkin.toml`, `data/` and our own
            // settings file out of the working directory, which the supervisor
            // sets to [dir] — so there is no path to pass and nothing to parse.
            args = emptyList(),
            // Nothing to add: this is an ordinary Rust binary against bionic,
            // with none of the JVM's `LD_LIBRARY_PATH`/`JAVA_HOME` needs.
            env = emptyMap(),
            workDir = dir,
        )

        engineThread = NativeServer.startBlocking(
            serverId,
            dir.absolutePath,
            port,
            invocation.toJson().toString(),
        ) { result ->
            // The engine has exited — cleanly or not. Report before tearing
            // the pump down so the final lines still reach the UI.
            val parsed = parse(result)
            val ok = parsed["ok"]?.jsonPrimitive?.booleanOrNull ?: false
            val error = parsed["error"]?.jsonPrimitive?.contentOrNull
            scope.launch {
                // The last of the console, including whatever it said on the
                // way down. The pump keeps running past this: an on-stop
                // backup writes `[Backup]` lines for minutes after the engine
                // is gone, and those are console lines like any other.
                drainLogs(serverId)
                // Whether this was a stop or a fall-over is the core's call,
                // from whether one was asked for — the engine's own `ok` says
                // only that it unwound cleanly.
                val verdict = ServerHost.lifecycle.exited(serverId, if (ok) 0 else 1)
                if (!ok && error != null) Log.e(TAG, "engine exited: $error")

                // However the engine went — stopped, crashed, killed — a
                // tunnel outliving it would hold the gateway's peer slot
                // against the next start.
                tunnel.shutdown()

                val due = backups.claim(serverId, verdict.state)
                // `force`: the core ruled on this exit a few lines ago, and
                // `superseded` is how it says a newer launch owns this server.
                transition(
                    serverId,
                    if (verdict.state == "crashed") ServerState.CRASHED else ServerState.STOPPED,
                    backupInProgress = due != null,
                    force = !verdict.superseded,
                )
                currentServerId = null
                currentPort = null

                // The pump stops here only when nothing else will write to it.
                if (due != null) backups.runAfterStop(serverId, due) else stopLogPump()
            }
        }
        // The engine is up; from here its exit needs judging.
        ServerHost.lifecycle.spawned(serverId)

        // `start` is contracted to return once the server accepts connections,
        // and the bridge has no timeout, so waiting here is correct.
        //
        // There is deliberately **no deadline**. First boot generates a world,
        // which legitimately runs for minutes on a mid-range phone, and the
        // old two-minute cap turned that into "The server did not finish
        // starting in time" with a healthy server behind it. A wedged engine
        // is caught by the run ending, which is what ends this wait.
        while (currentServerId == serverId) {
            when (status(serverId)) {
                ServerState.RUNNING -> {
                    ServerHost.lifecycle.consoleReady(serverId)
                    // Only now, and before `running` is announced. A server
                    // accepting connections on loopback is not the same as
                    // players being able to reach it, and reporting `running`
                    // before the tunnel is up is how a server looks healthy to
                    // everyone except the people trying to join. Throws if the
                    // tunnel cannot be brought up, having stopped the server.
                    tunnel.open(serverId, dir, port)
                    transition(serverId, ServerState.RUNNING)
                    return
                }
                ServerState.CRASHED -> throw ServerBackendException.Engine(
                    "The server stopped unexpectedly while starting."
                )
                else -> {
                    // A stop can arrive during world generation, which is
                    // exactly when a launch is slowest and a player is most
                    // likely to give up on it. Same check the JVM backend
                    // makes, and this backend was missing it.
                    if (ServerHost.lifecycle.shouldAbandon(serverId)) {
                        // The gateway poll would otherwise outlive the launch
                        // and hold a peer slot the next start needs.
                        tunnel.cancel()
                        withContext(Dispatchers.IO) { NativeServer.nativeStop() }
                        return
                    }
                    delay(POLL_MS)
                }
            }
        }
        // The run ended before it ever reported running.
        throw ServerBackendException.Engine("The server stopped before it finished starting.")
    }

    /**
     * Hand the engine what the player configured.
     *
     * A file rather than arguments or environment: the values include a MOTD
     * and player names, and a command line is world-readable on Android
     * through `/proc`.
     *
     * The file carries the *raw* inputs, not a rendered `pumpkin.toml`. What
     * a setting means — the clamps, the online-mode pairing Pumpkin asserts,
     * which keys it even has — is `homerun-pumpkin-ffi`'s and the engine reads
     * it with the same code the linked build uses. Rendering TOML here would
     * be a second spelling of every key and every enum, and getting one wrong
     * is silent: the value is dropped on load and the server starts on
     * defaults with nothing to say so.
     *
     * Never fails a launch. The fallback is Pumpkin's own configuration, which
     * includes `online_mode = true` — a server nobody can join — so the engine
     * shouts about it on the console instead.
     */
    private suspend fun writeSettings(serverId: String, dir: File, config: ServerConfig) {
        val env = config.settingsEnv
        val file = File(dir, SETTINGS_FILE)
        if (env == null) {
            // A stale file from a previous run would otherwise be applied to
            // this one, silently reinstating settings the API no longer has.
            runCatching { file.delete() }
            return
        }
        runCatching {
            val resolved = ServerSettingsWriter.resolveIdentities(env, config.gameType) { line ->
                onLog?.invoke(serverId, line)
            }
            val payload = buildJsonObject {
                put("env", env)
                put("gameType", config.gameType)
                put("resolved", buildJsonArray {
                    resolved.forEach { add(buildJsonObject { put("name", it.name); put("id", it.id) }) }
                })
            }
            withContext(Dispatchers.IO) { file.writeText(payload.toString()) }
        }.onFailure {
            Log.w(TAG, "$serverId: could not write settings: ${it.message}")
            onLog?.invoke(
                serverId,
                "[Homerun] Server settings could not be applied, so the server's defaults apply.",
            )
        }
    }

    /**
     * [graceful] reaches the supervisor's stop ladder, which asks the server
     * to save and shut down before it escalates — the same ladder the JVM
     * backend climbs, because the supervisor cannot tell the two apart.
     */
    override suspend fun stop(serverId: String, graceful: Boolean) {
        if (currentServerId != serverId) throw ServerBackendException.NotRunning(serverId)
        transition(serverId, ServerState.STOPPING)
        // Blocking, and it waits for a world save — never on the main thread.
        val result = withContext(Dispatchers.IO) {
            // Before the engine, so the gateway's peer slot is free by the
            // time the next start asks for one — a world save can take a
            // while, and the slot is not ours to hold through it.
            tunnel.shutdown()
            NativeServer.nativeStop()
        }
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
     * The server's own memory, at last.
     *
     * This used to be the whole app's resident set, because the engine ran
     * inside it and there was nothing else to measure — a figure that included
     * the WebView and everything the UI had loaded. The supervisor now reads
     * `/proc/<pid>` for the child, so the number is the server's.
     *
     * The ceiling is still `largeMemoryClass`, which is the app's heap limit
     * rather than the child's, so the gauge is a proportion of what this app
     * would be killed for rather than a hard limit on the server. Same
     * question the JVM backend's gauge answers.
     */
    override fun memoryUsage(serverId: String): MemoryUsage? {
        if (currentServerId != serverId) return null
        val usedMb = engineSamples().lastOrNull()?.memUsedMb
        val manager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        return MemoryUsage(usedKb = usedMb?.times(1024), maxMb = manager.largeMemoryClass)
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
        return engineSamples().lastOrNull()?.cpuPercent
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
        if (currentServerId != serverId) emptyList() else engineSamples()

    /**
     * The graph the supervisor built while this run was up.
     *
     * Nothing here samples anything. The supervisor owns the process, so it is
     * the only thing that knows what to measure — and `homerun-core` decides
     * what the readings mean and how much to keep. Identical to
     * [JavaServerBackend]'s, which is the point: both backends' numbers now
     * describe the same window in the same way, measured the same way.
     */
    private fun engineSamples(): List<PerfSample> = runCatching {
        val obj = json.parseToJsonElement(NativeServer.nativeMetrics()).jsonObject
        (obj["samples"] as? JsonArray).orEmpty().map { entry ->
            val o = entry.jsonObject
            fun num(key: String) = o[key]?.jsonPrimitive?.contentOrNull?.toDoubleOrNull()
            PerfSample(
                t = o["t"]?.jsonPrimitive?.longOrNull ?: 0L,
                memUsedMb = num("memUsedMb")?.toInt(),
                cpuPercent = num("cpuPercent"),
                playerCount = num("playerCount")?.toInt(),
            )
        }
    }.getOrDefault(emptyList())

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

    /**
     * A line of Homerun's own narrative, into the server's console.
     *
     * Written to the supervisor's buffer rather than emitted directly, which
     * is what keeps it in sequence with the engine's own output — and what
     * stops it arriving twice, once from here and once from the pump reading
     * the same buffer a moment later.
     */
    private fun note(serverId: String, line: String) {
        Log.i(TAG, line)
        runCatching { NativeServer.nativeNote(line) }
            .onFailure { Log.w(TAG, "note did not reach the console: ${it.message}") }
    }

    private fun transition(
        serverId: String,
        state: ServerState,
        backupInProgress: Boolean = false,
        /**
         * Announce without asking the core's permission, for an exit it has
         * just adjudicated itself.
         *
         * `exited` prunes a server's entry once the device has nothing left in
         * flight, and `mayAnnounce` refuses `stopped` for a server it holds no
         * entry for — so asking again turns "was this exit announced" into a
         * race between the exit callback and the stop call returning. The
         * in-app Stop wins that race; the notification's Stop loses it, and the
         * server sticks at `stopping` for ever with the foreground service
         * pinned behind it.
         *
         * Nothing is lost by not asking: `exited` already answers the question
         * `mayAnnounce` exists for, with `superseded`.
         */
        force: Boolean = false,
    ) {
        // The same guard the JVM backend has, and for the same reason: a
        // launch still catching up must not announce `running` for a server
        // already on its way down.
        //
        // The core answers *may this be said*; the check below answers *have
        // we already said it*, which is about the event stream rather than the
        // server, and is this file's own business.
        if (!force && !ServerHost.lifecycle.mayAnnounce(serverId, state.wire)) return
        if (lastAnnounced == state) return
        lastAnnounced = state
        onStateChanged?.invoke(serverId, state, backupInProgress)
    }

    private fun parse(raw: String): JsonObject =
        runCatching { json.parseToJsonElement(raw).jsonObject }.getOrElse { JsonObject(emptyMap()) }

    companion object {
        private const val TAG = "HomerunBackend"
        private const val DEFAULT_PORT = 25565
        private const val POLL_MS = 1000L

        /**
         * What the host leaves in the server directory for the engine to read.
         * The other half is `rust/homerun-pumpkin-bin/src/main.rs`.
         */
        private const val SETTINGS_FILE = "homerun-settings.json"

        /**
         * The server, staged as a library so Android will ship and exec it.
         *
         * Two platform rules, one rename: only `lib*.so` under `jniLibs` is
         * packaged, and API 29+ execs only from `nativeLibraryDir`. Same trick
         * as `libjavabin.so` — see `scripts/targets.js`.
         */
        private const val BINARY = "libpumpkin.so"

        /** The binary, or null if this build shipped none for the device's ABI. */
        fun binary(context: Context): File? =
            File(context.applicationInfo.nativeLibraryDir, BINARY).takeIf { it.canExecute() }

        /**
         * Whether this build can host a Pumpkin server at all.
         *
         * A declaration, not an inference: a device that can spawn processes
         * perfectly well still cannot run a server it did not ship.
         */
        fun isAvailable(context: Context): Boolean =
            NativeServer.available && binary(context) != null
    }
}

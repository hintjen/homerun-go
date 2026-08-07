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
import kotlinx.coroutines.withTimeoutOrNull
import java.io.File
import java.time.Instant

/**
 * A real Java Minecraft server, as a child process.
 *
 * This is the differentiated Android product: unlike iOS, Android can spawn
 * processes, so it runs the actual server jar rather than a reimplementation.
 * The desktop supervisor (`src/electron/supervisor.js` in the `homerun` repo)
 * is the spec, and the contract is deliberately identical so a world behaves
 * the same on a phone as on a PC:
 *
 *     java -Xmx<N>M -Xms<N>M -jar <jar> nogui      cwd = the server directory
 *
 * **Graceful stop is `stop` on stdin, not a signal.** Killing the JVM risks
 * the world save. The desktop waits and then force-kills; so does this.
 *
 * Player tracking reads the console rather than RCON. Vanilla prints join and
 * leave lines, and parsing them costs nothing — no port, no password, no
 * second protocol to keep alive. RCON becomes worth adding when moderation
 * (kick/ban/op) lands.
 */
class JavaServerBackend(
    private val context: Context,
    private val scope: CoroutineScope,
) : ServerBackend {

    override val kind = "javaNative"

    private var process: Process? = null
    private var pumpJob: Job? = null
    private var currentServerId: String? = null
    private var startedAt: Instant? = null
    private var currentPort: Int? = null
    private var lastState: ServerState = ServerState.STOPPED

    /** Console ring buffer. The bridge hands out slices by cursor. */
    private val lines = ArrayDeque<String>()
    private var firstLineIndex = 0

    private val roster = LinkedHashSet<String>()
    private var maxPlayers: Int? = null

    override var onStateChanged: ((String, ServerState) -> Unit)? = null
    override var onLog: ((String, String) -> Unit)? = null
    override var onPlayersChanged: ((String) -> Unit)? = null

    // -----------------------------------------------------------------------
    // Storage
    // -----------------------------------------------------------------------

    private fun dataDir(serverId: String): File =
        File(context.filesDir, "servers/$serverId").apply { mkdirs() }

    /** Server jars are *data*, so unlike the JRE they may be downloaded. */
    private fun serverJar(serverId: String): File = File(dataDir(serverId), "server.jar")

    override fun create(serverId: String) {
        dataDir(serverId)
        // Accepting the EULA is the user's act; the UI collects it during
        // creation. Writing it here would be doing it on their behalf.
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
        runningServerIds.firstOrNull()?.let { running ->
            if (running == serverId) throw ServerBackendException.AlreadyRunning(serverId)
            throw ServerBackendException.AnotherServerRunning(running)
        }

        val launcher = JavaRuntime.launcher(context)
            ?: throw ServerBackendException.Engine(
                "This build has no Java launcher, so it cannot host a Java server."
            )

        val jar = serverJar(serverId)
        if (!jar.isFile) {
            throw ServerBackendException.Engine("This server has not finished downloading yet.")
        }

        // The jar names the class to run. Doing this here keeps the native
        // launcher free of zip parsing.
        val mainClass = mainClassOf(jar)
            ?: throw ServerBackendException.Engine(
                "That server jar has no Main-Class, so it cannot be started."
            )

        val javaMajor = (config.extra["javaMajor"] as? Int) ?: DEFAULT_JAVA
        transition(serverId, ServerState.STARTING)
        reset()

        // Blocking, tens of megabytes, and the first start on a device pays
        // for it. The bridge has no call timeout precisely so this is allowed
        // to take as long as it takes.
        val javaHome = withContext(Dispatchers.IO) {
            runCatching { JavaRuntime.ensure(context, javaMajor) }
        }.getOrElse { err ->
            transition(serverId, ServerState.CRASHED)
            throw ServerBackendException.Engine(
                err.message ?: "The Java runtime could not be installed."
            )
        }
        val libjvm = JavaRuntime.libjvm(context, javaMajor)
            ?: throw ServerBackendException.Engine("The Java runtime is incomplete.")

        val dir = dataDir(serverId)
        val heap = heapMb(config.memoryMb)
        val port = (config.extra["port"] as? Int) ?: DEFAULT_PORT

        val started = withContext(Dispatchers.IO) {
            runCatching {
                ProcessBuilder(
                    listOf(
                        launcher.absolutePath,
                        libjvm.absolutePath,
                        mainClass.replace('.', '/'),
                        // No `-jar` here: the VM is created through JNI, so the
                        // jar goes on the classpath and the main class is named.
                        "-Djava.class.path=${jar.absolutePath}",
                        "-Djava.home=${javaHome.absolutePath}",
                        // The JRE's own natives live here; without it the VM
                        // starts but java.nio cannot load libnio.so.
                        "-Djava.library.path=${javaHome.absolutePath}/lib",
                        "-Duser.dir=${dir.absolutePath}",
                        "-Xmx${heap}M",
                        "-Xms${heap}M",
                        "--",
                        "nogui",
                    )
                )
                    .directory(dir)
                    .redirectErrorStream(true)
                    .also { builder ->
                        builder.environment().apply {
                            put("JAVA_HOME", javaHome.absolutePath)
                            // The runtime's .so files carry DT_NEEDED entries
                            // for each other (libnio -> libnet), and Android's
                            // linker will not find them without this. It has
                            // to be in the environment: the linker reads it at
                            // process start, so setting it later is too late.
                            put(
                                "LD_LIBRARY_PATH",
                                listOfNotNull(
                                    "${javaHome.absolutePath}/lib",
                                    "${javaHome.absolutePath}/lib/server",
                                    // Termux's libandroid-shmem, libandroid-spawn
                                    // and libz.so.1. The runtime's DT_RUNPATH
                                    // points at Termux's own prefix, which does
                                    // not exist here — LD_LIBRARY_PATH is
                                    // searched first, so this is what resolves.
                                    "${javaHome.absolutePath}/${JavaRuntime.DEPS_DIR}",
                                    System.getenv("LD_LIBRARY_PATH"),
                                ).joinToString(":"),
                            )
                            put("HOME", dir.absolutePath)
                            config.extra.forEach { (k, v) -> if (v is String) put(k, v) }
                        }
                    }
                    .start()
            }
        }.getOrElse { err ->
            transition(serverId, ServerState.CRASHED)
            throw ServerBackendException.Engine(
                "The server could not be launched: ${err.message ?: "unknown error"}"
            )
        }

        process = started
        currentServerId = serverId
        currentPort = port
        startLogPump(serverId, started)

        // "Done" is the JVM telling us it is accepting connections. Waiting
        // for the process to merely exist would report a server that cannot
        // be joined yet; the bridge has no timeout so waiting is correct.
        val ready = withTimeoutOrNull(START_TIMEOUT_MS) {
            while (true) {
                if (!started.isAlive) return@withTimeoutOrNull false
                if (lastState == ServerState.RUNNING) return@withTimeoutOrNull true
                delay(POLL_MS)
            }
            @Suppress("UNREACHABLE_CODE") false
        }

        if (ready != true) {
            stopProcess(started)
            transition(serverId, ServerState.CRASHED)
            throw ServerBackendException.Engine(
                if (started.isAlive) "The server did not finish starting in time."
                else "The server stopped unexpectedly while starting."
            )
        }
        startedAt = Instant.now()
    }

    override suspend fun stop(serverId: String) {
        val running = process
        if (currentServerId != serverId || running == null) {
            throw ServerBackendException.NotRunning(serverId)
        }
        transition(serverId, ServerState.STOPPING)
        withContext(Dispatchers.IO) { stopProcess(running) }
    }

    /**
     * `stop` on stdin, then wait. Escalate only if it will not go — a killed
     * JVM can lose the world it was mid-save on.
     */
    private fun stopProcess(running: Process) {
        runCatching {
            running.outputStream.write("stop\n".toByteArray())
            running.outputStream.flush()
        }.onFailure {
            // Broken pipe means it is already on its way out. Benign.
            Log.d(TAG, "stdin closed before `stop` landed")
        }
        if (!running.waitFor(GRACEFUL_STOP_SECONDS, java.util.concurrent.TimeUnit.SECONDS)) {
            Log.w(TAG, "server ignored `stop` for ${GRACEFUL_STOP_SECONDS}s — terminating")
            running.destroy()
            if (!running.waitFor(FORCE_STOP_SECONDS, java.util.concurrent.TimeUnit.SECONDS)) {
                running.destroyForcibly()
            }
        }
    }

    override val runningServerIds: List<String>
        get() = currentServerId
            ?.takeIf { process?.isAlive == true && lastState == ServerState.RUNNING }
            ?.let(::listOf)
            ?: emptyList()

    // -----------------------------------------------------------------------
    // Console
    // -----------------------------------------------------------------------

    private fun startLogPump(serverId: String, running: Process) {
        pumpJob?.cancel()
        pumpJob = scope.launch(Dispatchers.IO) {
            running.inputStream.bufferedReader().useLines { seq ->
                for (raw in seq) {
                    val line = raw.trim()
                    if (line.isEmpty()) continue
                    record(line)
                    onLog?.invoke(serverId, line)
                    interpret(serverId, line)
                }
            }
            // The stream ends when the process does.
            val code = runCatching { running.waitFor() }.getOrDefault(-1)
            val intentional = lastState == ServerState.STOPPING
            Log.i(TAG, "server exited (code $code, intentional=$intentional)")
            transition(serverId, if (intentional || code == 0) ServerState.STOPPED else ServerState.CRASHED)
            process = null
            currentServerId = null
            startedAt = null
            currentPort = null
        }
    }

    /**
     * Read state out of the console, the way every server wrapper does.
     * These strings are vanilla's; a modded server may word them differently,
     * which is why the roster is best-effort and never blocks anything.
     */
    private fun interpret(serverId: String, line: String) {
        if (lastState != ServerState.RUNNING && DONE.containsMatchIn(line)) {
            transition(serverId, ServerState.RUNNING)
        }
        JOINED.find(line)?.let {
            if (roster.add(it.groupValues[1])) onPlayersChanged?.invoke(serverId)
        }
        LEFT.find(line)?.let {
            if (roster.remove(it.groupValues[1])) onPlayersChanged?.invoke(serverId)
        }
        MAX_PLAYERS.find(line)?.let { maxPlayers = it.groupValues[1].toIntOrNull() }
    }

    @Synchronized
    private fun record(line: String) {
        lines.addLast(line)
        while (lines.size > MAX_BUFFERED_LINES) {
            lines.removeFirst()
            firstLineIndex++
        }
    }

    @Synchronized
    override fun logs(serverId: String, cursor: Int): LogSlice {
        if (currentServerId != serverId) return LogSlice(emptyList(), cursor)
        val from = (cursor - firstLineIndex).coerceIn(0, lines.size)
        return LogSlice(lines.drop(from), firstLineIndex + lines.size)
    }

    @Synchronized
    private fun reset() {
        lines.clear()
        firstLineIndex = 0
        roster.clear()
        maxPlayers = null
    }

    override suspend fun command(serverId: String, command: String) {
        val running = process
        if (currentServerId != serverId || running == null) {
            throw ServerBackendException.NotRunning(serverId)
        }
        withContext(Dispatchers.IO) {
            runCatching {
                running.outputStream.write("$command\n".toByteArray())
                running.outputStream.flush()
            }.getOrElse {
                throw ServerBackendException.Engine("The server is not accepting commands.")
            }
        }
    }

    // -----------------------------------------------------------------------
    // Introspection
    // -----------------------------------------------------------------------

    override fun status(serverId: String): ServerState =
        if (currentServerId == serverId) lastState else ServerState.STOPPED

    override fun players(serverId: String): PlayerRoster? {
        if (currentServerId != serverId || lastState != ServerState.RUNNING) return null
        return PlayerRoster(roster.map { PlayerRoster.Player(it, null) }, maxPlayers)
    }

    override fun uptime(serverId: String): Instant? =
        if (currentServerId == serverId) startedAt else null

    /**
     * The heap we told the JVM to take, not what it is using. Reading a child
     * process's RSS needs `/proc/<pid>/statm`, and the pid is not exposed
     * below API 26 in a form worth the branch — this is honest and stable.
     */
    override fun memoryUsage(serverId: String): MemoryUsage? {
        if (currentServerId != serverId) return null
        return MemoryUsage(usedKb = null, maxMb = lastHeapMb)
    }

    override fun cpuUsage(serverId: String): Double? = null

    override fun port(serverId: String): Int? =
        if (currentServerId == serverId) currentPort else null

    // -----------------------------------------------------------------------
    // Heap
    // -----------------------------------------------------------------------

    private var lastHeapMb: Int = 0

    /**
     * Android kills **the whole app** under memory pressure, not just the
     * server, so this is deliberately conservative: never more than a third of
     * total RAM, and never more than the caller asked for.
     */
    private fun heapMb(requestedMb: Int): Int {
        val manager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        val info = ActivityManager.MemoryInfo().also { manager.getMemoryInfo(it) }
        val totalMb = (info.totalMem / (1024 * 1024)).toInt()
        val ceiling = (totalMb / 3).coerceAtLeast(MIN_HEAP_MB)
        return requestedMb.coerceIn(MIN_HEAP_MB, ceiling).also { lastHeapMb = it }
    }

    /** `Main-Class` from the jar manifest, which is what `java -jar` reads. */
    private fun mainClassOf(jar: File): String? = runCatching {
        java.util.jar.JarFile(jar).use { it.manifest?.mainAttributes?.getValue("Main-Class") }
    }.getOrNull()

    private fun transition(serverId: String, state: ServerState) {
        if (lastState == state) return
        lastState = state
        onStateChanged?.invoke(serverId, state)
    }

    private companion object {
        const val TAG = "HomerunJava"
        const val DEFAULT_PORT = 25565
        /** Minecraft 1.20.5+ requires 21, and Termux ships it for both ABIs. */
        const val DEFAULT_JAVA = 21
        const val POLL_MS = 250L
        const val START_TIMEOUT_MS = 300_000L
        const val GRACEFUL_STOP_SECONDS = 30L
        const val FORCE_STOP_SECONDS = 8L
        const val MAX_BUFFERED_LINES = 2000
        const val MIN_HEAP_MB = 512

        val DONE = Regex("""Done \([^)]*\)! For help""")
        val JOINED = Regex("""]: (\w+) joined the game""")
        val LEFT = Regex("""]: (\w+) left the game""")
        val MAX_PLAYERS = Regex("""max(?:-players|Players)[=: ]+(\d+)""", RegexOption.IGNORE_CASE)
    }
}

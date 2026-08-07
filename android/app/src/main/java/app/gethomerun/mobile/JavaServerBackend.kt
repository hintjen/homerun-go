package app.gethomerun.mobile

import android.app.ActivityManager
import android.content.Context
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
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

    private val wireProxy = WireProxy(context, scope)

    /** The tunnel lookup, resolved alongside the JVM booting rather than before it. */
    private var tunnelJob: Deferred<WireProxy.Link?>? = null

    /**
     * The console has printed `Done (…)!`. Distinct from [ServerState.RUNNING],
     * which is only reported once the tunnel is up too.
     */
    @Volatile
    private var consoleReady = false

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

    /** Scratch space for the JVM, inside the server's own directory. */
    private fun tmpDir(dir: File): File = File(dir, "tmp").apply { mkdirs() }

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

    /** Everything the JVM invocation needs, resolved before anything is spawned. */
    private data class Launch(
        val javaHome: File,
        val libjvm: File,
        val jar: File,
        val mainClass: String,
    )

    override suspend fun start(serverId: String, config: ServerConfig) {
        runningServerIds.firstOrNull()?.let { running ->
            if (running == serverId) throw ServerBackendException.AlreadyRunning(serverId)
            throw ServerBackendException.AnotherServerRunning(running)
        }

        val launcher = JavaRuntime.launcher(context)
            ?: throw ServerBackendException.Engine(
                "This build has no Java launcher, so it cannot host a Java server."
            )

        val dir = dataDir(serverId)

        // Open the console before the slow work, not after: unpacking the
        // runtime and downloading a jar are minutes of a launch, and their
        // progress lines are the only thing the UI can show meanwhile.
        transition(serverId, ServerState.STARTING)
        reset()

        // Started now and awaited after the JVM is up. The gateway provisions
        // the peer asynchronously and the poll runs up to a minute, so doing
        // it here overlaps that with the download and the world generating,
        // exactly as the desktop provisions in parallel with Java booting.
        tunnelJob = config.resolveTunnel?.let { resolve ->
            scope.async { runCatching { resolve() }.getOrNull() }
        }

        // Nothing below has started a process, so a failure here is a launch
        // that did not happen — reported as stopped, with the reason on the
        // call. `crashed` is reserved for a JVM that ran and died.
        val prepared = runCatching {
            // Blocking the first time — a hundred megabytes out of the APK —
            // and the first start on a device pays for it. The bridge has no
            // call timeout precisely so this is allowed to take as long as it
            // takes.
            val javaHome = withContext(Dispatchers.IO) { JavaRuntime.ensure(context) }
            val libjvm = JavaRuntime.libjvm(context)
                ?: throw ServerBackendException.Engine("The Java runtime is incomplete.")

            val jar = ServerJar.ensure(
                dir = dir,
                version = config.version,
                loader = config.loader,
                bundledJava = JavaRuntime.javaMajor(context),
                onLog = { note(serverId, it) },
            )

            // Accept Mojang's EULA on the user's behalf, on every start —
            // byte-for-byte what the desktop app does in
            // `nativeServerManager.startServer`. The server will not boot
            // without it, and there is no acceptance step anywhere in the
            // product; `docs/android-server-backend.md` records that.
            File(dir, "eula.txt").writeText("eula=true\n")

            // The jar names the class to run. Doing this here keeps the native
            // launcher free of zip parsing.
            val mainClass = mainClassOf(jar)
                ?: throw ServerBackendException.Engine(
                    "That server jar has no Main-Class, so it cannot be started."
                )
            Launch(javaHome, libjvm, jar, mainClass)
        }.getOrElse { err ->
            transition(serverId, ServerState.STOPPED)
            throw err as? ServerBackendException ?: ServerBackendException.Engine(
                err.message ?: "The server could not be prepared."
            )
        }

        val (javaHome, libjvm, jar, mainClass) = prepared
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
                        // These builds are Termux's and carry Termux's prefix
                        // compiled in as the temp directory — a path that does
                        // not exist outside Termux, so anything writing a temp
                        // file fails on a path no one can explain.
                        //
                        // Note this does NOT silence the JNA/oshi stack trace
                        // at boot: JNA ships a glibc `libjnidispatch.so`, and
                        // bionic cannot dlopen it wherever it is unpacked.
                        // Minecraft wraps that probe in `ignoreErrors` and
                        // boots regardless — the cost is no hardware detail in
                        // crash reports.
                        "-Djava.io.tmpdir=${tmpDir(dir).absolutePath}",
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
                if (consoleReady) return@withTimeoutOrNull true
                delay(POLL_MS)
            }
            @Suppress("UNREACHABLE_CODE") false
        }

        if (ready != true) {
            tunnelJob?.cancel()
            stopProcess(started)
            transition(serverId, ServerState.CRASHED)
            throw ServerBackendException.Engine(
                if (started.isAlive) "The server did not finish starting in time."
                else "The server stopped unexpectedly while starting."
            )
        }

        openTunnel(serverId, dir, port)

        // Only now. The server accepting connections on loopback is not the
        // same as players being able to reach it, and reporting `running`
        // before the tunnel is up is how a server looks healthy to everyone
        // except the people trying to join. The desktop learned this too.
        startedAt = Instant.now()
        transition(serverId, ServerState.RUNNING)
    }

    /**
     * Bring up the gateway tunnel, if this server has one.
     *
     * A failure here is loud but not fatal: the world is up and playable on
     * the local network, so killing it would destroy something that works in
     * order to punish something that does not.
     */
    private suspend fun openTunnel(serverId: String, dir: File, port: Int) {
        val link = tunnelJob?.let { job ->
            note(serverId, "[Homerun] Connecting to the Homerun gateway...")
            runCatching { job.await() }.getOrNull()
        }
        tunnelJob = null

        if (link == null) {
            note(
                serverId,
                "[Homerun] No gateway tunnel for this server — it is reachable on this " +
                    "network, but not from the internet.",
            )
            return
        }

        runCatching {
            wireProxy.start(
                serverId = serverId,
                dir = dir,
                link = link,
                minecraftPort = port,
                onLog = { line -> note(serverId, line) },
                // The gateway regenerating its keys is the usual cause, and
                // the credentials we hold are then permanently dead. The
                // desktop stops the server; so does this, rather than leave
                // something running that nobody can join.
                onHandshakeFailed = { scope.launch { runCatching { stop(serverId) } } },
            )
            note(serverId, "[Homerun] Connected to the Homerun gateway.")
        }.onFailure { err ->
            note(serverId, "[Homerun] ${err.message ?: "The tunnel could not be started."}")
        }
    }

    override suspend fun stop(serverId: String) {
        val running = process
        if (currentServerId != serverId || running == null) {
            throw ServerBackendException.NotRunning(serverId)
        }
        transition(serverId, ServerState.STOPPING)
        withContext(Dispatchers.IO) {
            // Before the JVM, so the gateway's peer slot is free by the time
            // the next start asks for one.
            wireProxy.stop()
            stopProcess(running)
        }
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
            // However the JVM went — stopped, crashed, killed — the tunnel
            // outliving it would hold the gateway's peer slot against the
            // next start. The desktop kills it on java exit for the same
            // reason.
            tunnelJob?.cancel()
            tunnelJob = null
            wireProxy.stop()
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
        // Records that the JVM is up; `running` is announced by start(),
        // after the tunnel, so the two are not the same thing.
        if (DONE.containsMatchIn(line)) consoleReady = true
        JOINED.find(line)?.let {
            if (roster.add(it.groupValues[1])) onPlayersChanged?.invoke(serverId)
        }
        LEFT.find(line)?.let {
            if (roster.remove(it.groupValues[1])) onPlayersChanged?.invoke(serverId)
        }
        MAX_PLAYERS.find(line)?.let { maxPlayers = it.groupValues[1].toIntOrNull() }
    }

    /**
     * A line from Homerun rather than from the server — jar downloads, runtime
     * unpacking. Recorded as well as emitted so a console that mounts after
     * the fact still shows how the launch went; the log pump does not exist
     * yet when these are written.
     */
    private fun note(serverId: String, line: String) {
        Log.i(TAG, line)
        record(line)
        onLog?.invoke(serverId, line)
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
        consoleReady = false
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

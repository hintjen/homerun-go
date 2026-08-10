package app.gethomerun.mobile

import android.app.ActivityManager
import android.content.Context
import android.util.Log
import kotlinx.coroutines.CancellationException
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
    private val backups = BackupManager(context)

    /**
     * The backup context for a server that is running, kept until it exits.
     *
     * The exit handler needs the repository and device id, and by then the
     * caller's `ServerConfig` is long out of scope.
     */
    private val backupOnStop = java.util.concurrent.ConcurrentHashMap<String, BackupContext>()

    /**
     * On-stop backups still running, so a relaunch can cancel one.
     *
     * A backup outlives the server it backs up — restic reads `world/` long
     * after the JVM is gone — and the user is entitled to press Start during
     * it. See [cancelOnStopBackup] for why cancelling is safe.
     */
    private val backupJobs = java.util.concurrent.ConcurrentHashMap<String, Job>()

    /** The tunnel lookup, resolved alongside the JVM booting rather than before it. */
    private var tunnelJob: Deferred<WireProxy.Link?>? = null

    /**
     * The console has printed `Done (…)!`. Distinct from [ServerState.RUNNING],
     * which is only reported once the tunnel is up too.
     */
    @Volatile
    private var consoleReady = false

    /**
     * Who owns a server right now, and what its last exit meant.
     *
     * Held by [ServerHost] so the bridge and this backend consult one answer,
     * and computed by `homerun-core::lifecycle` so iOS cannot answer
     * differently. Everything this backend used to track by hand — a stop
     * asked for but not yet carried out, which launch a dying process belongs
     * to, whether an exit was a crash — lives there now, with the tests.
     */
    private val lifecycle: Core.Lifecycle get() = ServerHost.lifecycle

    /**
     * The server whose JVM this backend is holding.
     *
     * Set once the process is spawned, which is a download and a runtime
     * unpack after the start call arrived — so it is not, and never was, the
     * answer to "does this device own this server". That question is
     * [lifecycle]'s, and answering it from this field is what let the reconcile
     * loop start a second launch on every poll.
     */
    private var currentServerId: String? = null
    private var startedAt: Instant? = null
    private var currentPort: Int? = null
    private var lastState: ServerState = ServerState.STOPPED

    /** Console ring buffer. The bridge hands out slices by cursor. */
    private val lines = ArrayDeque<String>()
    private var firstLineIndex = 0

    private val roster = LinkedHashSet<String>()
    private var maxPlayers: Int? = null

    override var onStateChanged: ((String, ServerState, Boolean) -> Unit)? = null
    override var onLog: ((String, String) -> Unit)? = null
    override var onPlayersChanged: ((String) -> Unit)? = null
    override var onNetworkError: ((String, String) -> Unit)? = null

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

    /**
     * Admission — is this a duplicate, is another server in the way — is
     * decided by the core before this is ever called, in the bridge's start
     * handler. Two places asking the same question is how they come to
     * disagree.
     */
    override suspend fun start(serverId: String, config: ServerConfig) {
        launch(serverId, config)
    }

    private suspend fun launch(serverId: String, config: ServerConfig) {
        val launcher = JavaRuntime.launcher(context)
            ?: throw ServerBackendException.Engine(
                "This build has no Java launcher, so it cannot host a Java server."
            )

        val dir = dataDir(serverId)

        // The order below is the core's, not this file's. `order.at(...)`
        // refuses a step that arrives before one the plan puts ahead of it,
        // and honours a pending stop at the checkpoints the core marks — so a
        // reordering fails here rather than surfacing months later as a
        // re-downloaded world or a green card for an unreachable server.
        val order = LaunchOrder(
            serverId,
            Core.launchPlan(
                backups = config.backupContext != null,
                settings = config.settingsEnv != null,
                tunnel = config.resolveTunnel != null,
            ),
        )

        order.at("cancelOnStopBackup")
        if (lifecycle.supersedesOnStopBackup(serverId)) cancelOnStopBackup(serverId)

        // Open the console before the slow work, not after: unpacking the
        // runtime and downloading a jar are minutes of a launch, and their
        // progress lines are the only thing the UI can show meanwhile.
        order.at("announceStarting")
        transition(serverId, ServerState.STARTING)
        reset()

        // Started now and awaited after the JVM is up. The gateway provisions
        // the peer asynchronously and the poll runs up to a minute, so doing
        // it here overlaps that with the download and the world generating,
        // exactly as the desktop provisions in parallel with Java booting.
        if (config.resolveTunnel != null) order.at("beginResolveTunnel")
        tunnelJob = config.resolveTunnel?.let { resolve ->
            scope.async { runCatching { resolve() }.getOrNull() }
        }

        // Nothing below has started a process, so a failure here is a launch
        // that did not happen — reported as stopped, with the reason on the
        // call. `crashed` is reserved for a JVM that ran and died.
        order.at("ensureJar")
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

        // Held for the stop path: the exit handler needs the repository and
        // device id, and by then the caller's config is long gone.
        config.backupContext?.let { backupOnStop[serverId] = it }

        // Cheapest place to notice: nothing has been spawned, so there is
        // nothing to tear down.
        if (abandonIfStopped(serverId)) return

        // A start admitted *during* a stop is a restart, and the core says
        // whether an outgoing engine still has to be waited for. Asked here,
        // immediately before spawning, because the old JVM usually exits while
        // this launch was preparing.
        order.at("awaitPreviousExit")
        if (lifecycle.awaitPreviousExit(serverId)) awaitPreviousExit(serverId)

        val (javaHome, libjvm, jar, mainClass) = prepared
        val heap = heapMb(config.memoryMb)
        val port = (config.extra["port"] as? Int) ?: DEFAULT_PORT

        // Written on every launch, after the jar is in place and before the
        // JVM reads any of it. This is what makes a setting changed in the
        // wizard or on the web dashboard take effect — and what makes a
        // *removal* take effect, since the files are the server's source of
        // truth before it accepts anyone. Never throws: the server's own
        // defaults are a better outcome than refusing to start.
        // Before the world is read by anything: if another device holds a
        // newer snapshot, its world wins over this device's stale copy. A
        // failure here stops the launch rather than starting a server on a
        // world we were told is out of date — quietly diverging two devices is
        // the failure this exists to prevent.
        if (config.backupContext != null && order.at("restoreWorld")) return
        config.backupContext?.let { backup ->
            backups.restoreBeforeLaunch(
                serverId = serverId,
                dir = dir,
                settings = backup.settings,
                deviceId = backup.deviceId,
                onLog = { note(serverId, it) },
            )
        }

        if (config.settingsEnv != null) order.at("writeSettings")
        config.settingsEnv?.let { env ->
            ServerSettingsWriter.apply(
                serverId = serverId,
                dir = dir,
                env = env,
                gameType = config.gameType,
                port = port,
                onLog = { note(serverId, it) },
            )
        }

        if (order.at("spawn")) return
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
        // There is now something to stop, and something whose exit will need
        // judging. The core pins this launch's generation here, so a process
        // that outlives a stop-then-restart is recognised as the old one's.
        lifecycle.spawned(serverId)
        startLogPump(serverId, started)

        // "Done" is the JVM telling us it is accepting connections. Waiting
        // for the process to merely exist would report a server that cannot
        // be joined yet; the bridge has no timeout so waiting is correct.
        order.at("awaitConsole")
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

        // A stop that landed while the JVM was booting: the console exists
        // now, so it can be asked politely rather than killed. Reached when
        // the stop arrived before `process` was assigned — after that, `stop`
        // waits for the console itself.
        if (lifecycle.shouldAbandon(serverId)) {
            Log.i(TAG, "$serverId: honouring a stop that arrived during startup")
            tunnelJob?.cancel()
            tunnelJob = null
            withContext(Dispatchers.IO) {
                wireProxy.stop()
                stopProcess(started)
            }
            return
        }

        if (config.resolveTunnel != null) order.at("openTunnel")
        openTunnel(serverId, dir, port)

        // Only now. The server accepting connections on loopback is not the
        // same as players being able to reach it, and reporting `running`
        // before the tunnel is up is how a server looks healthy to everyone
        // except the people trying to join. The desktop learned this too.
        order.at("announceRunning")
        startedAt = Instant.now()
        transition(serverId, ServerState.RUNNING)
    }

    /**
     * Bring up the gateway tunnel. Failing to is fatal to the launch.
     *
     * A server nobody can reach is not a working server, so the desktop stops
     * it rather than leave something running that looks healthy and is not —
     * `pollAndProvisionWireproxy` throws when the config never arrives, and
     * `server-started`'s catch stops the server. This matches that exactly.
     * Both paths also emit `native-server-network-error`, because a clean stop
     * with no explanation is indistinguishable from the user's own Stop.
     */
    private suspend fun openTunnel(serverId: String, dir: File, port: Int) {
        note(serverId, "[Homerun] Connecting to the Homerun gateway...")

        val link = tunnelJob?.let { job -> runCatching { job.await() }.getOrNull() }
        tunnelJob = null

        if (link == null) {
            failTunnel(
                serverId, PROVISIONING,
                "Failed to establish network tunnel: the gateway did not provide one.",
            )
        }

        runCatching {
            wireProxy.start(
                serverId = serverId,
                dir = dir,
                link = link,
                minecraftPort = port,
                onLog = { line -> note(serverId, line) },
                // The tunnel came up and then stopped being answered — the
                // gateway regenerating its keys is the usual cause, and the
                // credentials we hold are permanently dead. Same verdict, but
                // reported as `handshake` so the UI can say so.
                onHandshakeFailed = {
                    scope.launch {
                        runCatching { stopForNetworkError(serverId, HANDSHAKE) }
                    }
                },
            )
        }.onFailure { err ->
            failTunnel(
                serverId, PROVISIONING,
                "Failed to establish network tunnel: ${err.message ?: "it could not be started"}.",
            )
        }

        note(serverId, "[Homerun] Connected to the Homerun gateway.")
    }

    /**
     * Walks the core's launch plan alongside the launch.
     *
     * Enforces *relative* order rather than announcing every step: a step may
     * be reached without the host narrating the ones the core folds into it,
     * but never before something the plan puts ahead of it. That is exactly
     * the property `homerun-core::launch`'s tests pin.
     *
     * Also honours a pending stop at the checkpoints the core marks, which is
     * what the scattered `abandonIfStopped` calls were doing by hand and less
     * reliably.
     */
    private inner class LaunchOrder(
        private val serverId: String,
        private val steps: List<Core.Step>,
    ) {
        private var next = 0

        /** True when the caller should give up: a stop is waiting. */
        fun at(name: String): Boolean {
            val index = steps.indexOfFirst { it.name == name }
            require(index >= 0) { "launch step \"$name\" is not in the plan" }
            require(index >= next) {
                "launch step out of order: $name comes after ${steps[next - 1].name}"
            }
            next = index + 1
            return if (steps[index].checkpoint) abandonIfStopped(serverId) else false
        }
    }

    /**
     * Carry out the core's decision to supersede an on-stop backup: cancel the
     * coroutine, which kills restic's child process on its way out.
     *
     * The reasoning — why this is safe, and why no backup state is reported —
     * is `homerun-core::lifecycle::supersedes_on_stop_backup`.
     */
    private fun cancelOnStopBackup(serverId: String) {
        val job = backupJobs.remove(serverId) ?: return
        if (!job.isActive) return
        Log.i(TAG, "$serverId: cancelling the on-stop backup — this device is relaunching")
        note(serverId, "[Backup] Starting again — the backup in progress was cancelled.")
        job.cancel()
    }

    /**
     * Wait out the JVM a previous launch left behind, having been told by the
     * core that there is one. Bounded, because a wedged process must not block
     * a start for ever; refusing is better than spawning a second server into
     * one directory.
     */
    private suspend fun awaitPreviousExit(serverId: String) {
        val previous = process ?: return
        if (!previous.isAlive) return
        Log.i(TAG, "$serverId: waiting for the previous server to finish stopping")
        val gone = withTimeoutOrNull(PREVIOUS_EXIT_WAIT_MS) {
            while (previous.isAlive) delay(POLL_MS)
            true
        } == true
        if (!gone) {
            Log.w(TAG, "$serverId: the previous server has not exited — refusing to start a second")
            throw ServerBackendException.Engine(
                "The previous server is still shutting down. Try again in a moment."
            )
        }
    }

    /**
     * Give up a launch that was stopped while it was still preparing.
     *
     * Returns true when the caller should return without starting anything.
     * Reported as `stopped` rather than `crashed`: the user asked for this.
     */
    private fun abandonIfStopped(serverId: String): Boolean {
        if (!lifecycle.shouldAbandon(serverId)) return false
        lifecycle.abandoned(serverId)
        Log.i(TAG, "$serverId: launch abandoned — a stop arrived while it was preparing")
        tunnelJob?.cancel()
        tunnelJob = null
        transition(serverId, ServerState.STOPPED)
        return true
    }

    /** Report, stop, and fail the launch. */
    private suspend fun failTunnel(serverId: String, kind: String, message: String): Nothing {
        note(serverId, "[Homerun] $message Stopping server.")
        stopForNetworkError(serverId, kind)
        throw ServerBackendException.Engine(message)
    }

    /**
     * Stop a server because its tunnel failed.
     *
     * The event goes out *before* the stop so the UI has the reason in hand by
     * the time the card flips — it stops through the normal clean path, so
     * otherwise this is indistinguishable from the user pressing Stop.
     */
    private suspend fun stopForNetworkError(serverId: String, kind: String) {
        Log.w(TAG, "$serverId: tunnel failed ($kind) — stopping")
        onNetworkError?.invoke(serverId, kind)

        // Through the core, exactly as a stop from the bridge would be. This
        // is a stop somebody asked for — Homerun did, on the player's behalf —
        // and recording the intent is what keeps the exit from being reported
        // as a crash, which would also skip the on-stop backup.
        val verdict = lifecycle.stopRequested(serverId)
        try {
            if (verdict.verdict != "notRunning") {
                runCatching { stop(serverId, graceful = verdict.verdict == "graceful") }
            }
        } finally {
            lifecycle.callFinished(serverId)
        }
    }

    override suspend fun stop(serverId: String, graceful: Boolean) {
        val running = process
        if (running == null) {
            // A launch can be minutes long before there is a process to talk
            // to — a jar downloading, a world restoring. The core recorded the
            // intent when the call arrived, so the launch will see it at its
            // next checkpoint and give up; there is nothing to do here.
            Log.i(TAG, "$serverId: stop arrived before the JVM existed — the launch will abandon")
            return
        }
        if (currentServerId != serverId) {
            throw ServerBackendException.NotRunning(serverId)
        }

        transition(serverId, ServerState.STOPPING)

        // *How* to stop is the core's call — graceful only when there is a
        // console that can hear it — and it is carried out now, never deferred
        // until the server has finished starting. This method only performs
        // the verdict it was given when the call arrived.
        withContext(Dispatchers.IO) {
            // Before the JVM, so the gateway's peer slot is free by the time
            // the next start asks for one.
            wireProxy.stop()
            if (graceful) {
                stopProcess(running)
            } else {
                Log.i(TAG, "$serverId: stopping before the console was ready — terminating")
                runCatching { running.destroy() }
                if (!running.waitFor(FORCE_STOP_SECONDS, java.util.concurrent.TimeUnit.SECONDS)) {
                    runCatching { running.destroyForcibly() }
                }
            }
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
            // Caught for the same reason as the tunnel's pump: this scope's
            // SupervisorJob keeps siblings alive but does nothing about an
            // unhandled exception, which reaches the default handler and kills
            // the app. Killing the JVM closes this stream under a blocked
            // readLine, so the throw is a normal part of stopping.
            try {
                running.inputStream.bufferedReader().useLines { seq ->
                    for (raw in seq) {
                        val line = raw.trim()
                        if (line.isEmpty()) continue
                        record(line)
                        onLog?.invoke(serverId, line)
                        interpret(serverId, line)
                    }
                }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (err: Throwable) {
                Log.d(TAG, "console output ended: ${err.message}")
            }
            // The stream ends when the process does.
            val code = runCatching { running.waitFor() }.getOrDefault(-1)
            // The verdict is the core's: intent, not the exit code and not
            // a state a still-running launch can overwrite. A stop carried out
            // by terminating a starting JVM exits 143, and calling that a
            // crash skips the on-stop backup and loses the session's play.
            val verdict = lifecycle.exited(serverId, code)
            val intentional = verdict.intentional
            Log.i(TAG, "server exited (code $code, intentional=$intentional)")
            val outcome = verdict.state
            // However the JVM went — stopped, crashed, killed — the tunnel
            // outliving it would hold the gateway's peer slot against the
            // next start. The desktop kills it on java exit for the same
            // reason.
            tunnelJob?.cancel()
            tunnelJob = null
            wireProxy.stop()
            // The stop ack carries `backup_in_progress`, which is what opens
            // the backup lease — so it is claimed only when a backup is
            // actually about to run, and `backupAfterStop` reports an outcome
            // either way, because only that closes it again.
            val backup = backupOnStop.remove(serverId)
                ?.takeIf { outcome != "crashed" && backups.hasLocalWorld(dataDir(serverId)) }

            transition(
                serverId,
                if (outcome == "crashed") ServerState.CRASHED else ServerState.STOPPED,
                backupInProgress = backup != null,
            )
            process = null
            currentServerId = null
            startedAt = null
            currentPort = null

            if (backup != null) {
                val job = scope.launch {
                    runCatching {
                        backups.backupAfterStop(
                            serverId = serverId,
                            dir = dataDir(serverId),
                            settings = backup.settings,
                            deviceId = backup.deviceId,
                            onLog = { note(serverId, it) },
                        )
                    }.onFailure {
                        if (it is CancellationException) throw it
                        Log.w(TAG, "on-stop backup failed for $serverId: ${it.message}")
                    }
                }
                backupJobs[serverId] = job
                job.invokeOnCompletion { backupJobs.remove(serverId, job) }
            }
        }
    }

    /**
     * Read state out of the console, the way every server wrapper does.
     * These strings are vanilla's; a modded server may word them differently,
     * which is why the roster is best-effort and never blocks anything.
     */
    private fun interpret(serverId: String, line: String) {
        // What a console line means is decided in `homerun-core::console`,
        // which knows the things a regex here kept having to relearn — that a
        // loader may add its own prefix, and that anyone can type "Notch
        // joined the game" into chat.
        val meaning = runCatching { Core.classify(line) }.getOrNull() ?: return

        // Records that the JVM is up; `running` is announced by start(), after
        // the tunnel, so the two are not the same thing.
        if (meaning.ready) {
            consoleReady = true
            lifecycle.consoleReady(serverId)
        }
        meaning.joined?.let { if (roster.add(it)) onPlayersChanged?.invoke(serverId) }
        meaning.left?.let { if (roster.remove(it)) onPlayersChanged?.invoke(serverId) }

        // Still local: the core has no opinion about server.properties, which
        // is where this actually belongs once settings move across.
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

    private fun transition(
        serverId: String,
        state: ServerState,
        backupInProgress: Boolean = false,
    ) {
        // A launch still catching up must not announce `running` for a
        // server already on its way down: the UI would flip the card to
        // running and the API would mark the service healthy, moments before
        // it exits. The core decides that.
        if (!lifecycle.mayAnnounce(serverId, state.wire)) return
        if (lastState == state) return
        lastState = state
        onStateChanged?.invoke(serverId, state, backupInProgress)
    }

    private companion object {
        const val TAG = "HomerunJava"
        const val DEFAULT_PORT = 25565
        const val POLL_MS = 250L
        const val START_TIMEOUT_MS = 300_000L

        /**
         * How long a *restart* waits for the outgoing JVM to exit before
         * refusing, rather than spawning a second server into one directory.
         *
         * Nothing else waits on a stop: a stop is carried out immediately,
         * gracefully when the console can hear it and by termination when it
         * cannot.
         */
        const val PREVIOUS_EXIT_WAIT_MS = 120_000L
        const val GRACEFUL_STOP_SECONDS = 30L
        const val FORCE_STOP_SECONDS = 8L
        const val MAX_BUFFERED_LINES = 2000
        const val MIN_HEAP_MB = 512

        /** The two `native-server-network-error` kinds the contract defines. */
        const val PROVISIONING = "provisioning"
        const val HANDSHAKE = "handshake"

        val MAX_PLAYERS = Regex("""max(?:-players|Players)[=: ]+(\d+)""", RegexOption.IGNORE_CASE)
    }
}

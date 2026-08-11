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

    /**
     * This run's performance graph, and the coroutine feeding it. The graph
     * belongs to `homerun-core`; this backend only reads `/proc`.
     */
    private val metrics = Core.Metrics()
    private var perfJob: Job? = null

    /**
     * The JVM's pid, as the launcher announced it on its first console line.
     *
     * Read from the console rather than from [Process], whose `pid()` does not
     * resolve against the Android SDK — see the note in the launcher's
     * `main.rs`. Null until that line arrives, and again once the run ends.
     */
    @Volatile
    private var serverPid: Long? = null

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

    /**
     * The last state **announced to the UI** — not a second opinion about what
     * the server is doing. `homerun-core::lifecycle` owns that, and nothing
     * here re-derives it.
     *
     * This exists because the core deliberately *forgets*: an entry with no
     * engine and no call in flight is removed, so `lifecycle.state` answers
     * `stopped` for a run that ended in a crash. That is right for the core —
     * a finished run owns nothing — and wrong for [status], which is answering
     * "what became of it".
     */
    private var lastAnnounced: ServerState = ServerState.STOPPED

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

    /**
     * Server jars, shared by every server on this device and named after their
     * digests — see [ServerJar.ensure]. A sibling of `servers/`, not a child of
     * one, and in `filesDir` rather than `cacheDir`: Android may delete
     * `cacheDir` under a running app, and losing an entry there would cost a
     * download the next server on that version was counting on.
     */
    private fun jarCacheDir(): File = File(context.filesDir, "jars").apply { mkdirs() }

    override fun create(serverId: String) {
        dataDir(serverId)
    }

    override fun delete(serverId: String) {
        // "Is anything still holding this" is the core's question, and it is
        // the same one `native-server-active-ids` answers — running, coming up
        // or winding down. Asking it any other way here is how a directory
        // gets deleted out from under a launch that is still preparing.
        if (serverId in lifecycle.activeIds()) {
            throw ServerBackendException.AlreadyRunning(serverId)
        }
        dataDir(serverId).deleteRecursively()

        // The jar this server was using may now be shared with nobody. Its
        // bytes live in the cache entry as well as in the directory just
        // removed, so without this the space is not actually given back until
        // some other server happens to download something.
        ServerJar.dropUnusedCacheEntries(jarCacheDir(), File(context.filesDir, "servers"))
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
                Core.refusal("noJavaRuntime")
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
        // Heap size, the flags that carry it, what Minecraft's own main takes
        // and the EULA file — all the core's. This file supplies the one
        // thing only it can know, which is how much RAM the device has.
        val jvm = Core.jvmLaunch(config.memoryMb, deviceTotalMb())
        lastHeapMb = jvm.heapMb

        val prepared = runCatching {
            // Blocking the first time — a hundred megabytes out of the APK —
            // and the first start on a device pays for it. The bridge has no
            // call timeout precisely so this is allowed to take as long as it
            // takes.
            val javaHome = withContext(Dispatchers.IO) { JavaRuntime.ensure(context) }
            val libjvm = JavaRuntime.libjvm(context)
                ?: throw ServerBackendException.Engine(Core.refusal("brokenJavaRuntime"))

            val jar = ServerJar.ensure(
                dir = dir,
                cacheDir = jarCacheDir(),
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
            File(dir, jvm.eulaFile).writeText(jvm.eulaContents)

            // The jar names the class to run. Doing this here keeps the native
            // launcher free of zip parsing.
            val mainClass = mainClassOf(jar)
                ?: throw ServerBackendException.Engine(
                    Core.refusal("noMainClass")
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
                    )
                        // Everything above is Android's — a path that only
                        // exists here, or a workaround for a Termux-built
                        // runtime. Everything below is what any host running
                        // this server would pass, so the core says it.
                        + jvm.options
                        + "--"
                        + jvm.programArgs
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
        startPerfSampler(serverId, started)

        // "Done" is the JVM telling us it is accepting connections. Waiting
        // for the process to merely exist would report a server that cannot
        // be joined yet; the bridge has no timeout so waiting is correct.
        order.at("awaitConsole")
        val ready = withTimeoutOrNull(Core.jvmLimits().startTimeoutMs) {
            while (true) {
                if (!started.isAlive) return@withTimeoutOrNull false
                if (consoleReady) return@withTimeoutOrNull true
                delay(POLL_MS)
            }
            @Suppress("UNREACHABLE_CODE") false
        }

        if (ready != true) {
            tunnelJob?.cancel()
            climbStopLadder(started, console = true)
            transition(serverId, ServerState.CRASHED)
            throw ServerBackendException.Engine(
                if (started.isAlive) Core.refusal("startTimedOut")
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
                climbStopLadder(started, console = true)
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
        val gone = withTimeoutOrNull(Core.jvmLimits().previousExitWaitMs) {
            while (previous.isAlive) delay(POLL_MS)
            true
        } == true
        if (!gone) {
            Log.w(TAG, "$serverId: the previous server has not exited — refusing to start a second")
            throw ServerBackendException.Engine(
                Core.refusal("previousServerBusy")
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
            // `graceful` is the core's word for "there is a console that can
            // hear a stop". The ladder itself, including what to do when there
            // is not, is also the core's.
            if (!graceful) {
                Log.i(TAG, "$serverId: stopping before the console was ready")
            }
            climbStopLadder(running, console = graceful)
        }
    }

    /**
     * Climb the core's stop ladder until the JVM is gone.
     *
     * Nothing about *what to try, in what order, waiting how long* is decided
     * here — see `homerun_core::minecraft::jvm::stop_ladder`, which carries the
     * reason the first rung is a console command and not a terminate. This
     * carries the rungs out.
     */
    private fun climbStopLadder(running: Process, console: Boolean) {
        val (command, rungs) = Core.stopLadder(console)

        for (rung in rungs) {
            when (rung.action) {
                "console" -> runCatching {
                    running.outputStream.write("$command\n".toByteArray())
                    running.outputStream.flush()
                }.onFailure {
                    // Broken pipe means it is already on its way out. Benign,
                    // and the next rung is harmless against a dead process.
                    Log.d(TAG, "stdin closed before `$command` landed")
                }

                "terminate" -> {
                    Log.w(TAG, "server has not gone — terminating")
                    runCatching { running.destroy() }
                }

                "kill" -> {
                    Log.w(TAG, "server ignored a terminate — killing")
                    runCatching { running.destroyForcibly() }
                }

                else -> Log.w(TAG, "unknown stop rung \"${rung.action}\" — skipping")
            }

            if (rung.waitMs <= 0L) return
            if (running.waitFor(rung.waitMs, java.util.concurrent.TimeUnit.MILLISECONDS)) return
        }
    }

    /**
     * Straight from the core, which is the only place that knows.
     *
     * This used to re-derive it from `process.isAlive` and the last announced
     * state, and the two could disagree: between the JVM dying and the console
     * pump noticing, the host said "not running" while the core still said it
     * was. That list goes to the API as this device's `instances` on every
     * heartbeat, and to the UI on reconnect, so a second opinion is a device
     * telling two stories about what it is hosting.
     */
    override val runningServerIds: List<String>
        get() = lifecycle.runningIds()


    // -----------------------------------------------------------------------
    // Console
    // -----------------------------------------------------------------------

    /**
     * Feed the core a reading every so often, for as long as the JVM lives.
     *
     * The interval is the core's and is **re-read every pass**: it doubles
     * once the graph is full, so a sampler that scheduled itself once would
     * keep paying to read `/proc` at a resolution the core has stopped
     * keeping. The first reading is taken immediately, so a server that is
     * stopped after a minute still has a graph.
     *
     * Nothing here decides anything. It reads counters and hands them over —
     * see [ProcMetrics] for why they are counters and not percentages.
     */
    private fun startPerfSampler(serverId: String, running: Process) {
        perfJob?.cancel()
        metrics.reset()
        perfJob = scope.launch(Dispatchers.IO) {
            while (running.isAlive) {
                // Null until the launcher's first console line has been read,
                // which is a moment or two. A reading with nothing in it is
                // still worth offering: it anchors the clock, and the graph
                // renders "unavailable" rather than a fabricated zero.
                val pid = serverPid
                metrics.record(
                    atMs = System.currentTimeMillis(),
                    memUsedKb = pid?.let { ProcMetrics.residentKb(it) },
                    cpuSeconds = pid?.let { ProcMetrics.cpuSeconds(it) },
                    // Best-effort, like everything else on this graph: the
                    // roster is built from console lines and is empty before
                    // the server has printed any.
                    playerCount = players(serverId)?.players?.size,
                )
                delay(metrics.intervalMs())
            }
        }
    }

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
            // The sampler notices a dead process only when it next wakes, and
            // by then its interval may be half an hour. The samples stay in
            // the core's history until the next launch resets it; nothing
            // reads them once `currentServerId` is cleared below, which
            // matches the UI — Insights only fetches while a server runs.
            perfJob?.cancel()
            perfJob = null

            process = null
            serverPid = null
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
        // The launcher naming itself, before the VM has printed anything. Only
        // taken once per run: the server's own output could contain this
        // string, and the first one is the only one that came from us.
        if (serverPid == null) {
            LAUNCHER_PID.find(line)?.let { serverPid = it.groupValues[1].toLongOrNull() }
        }

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

        // The ceiling comes from the same classification as the rest — it is
        // console parsing, and it belongs beside ready/joined/left rather than
        // in a regex this file kept for itself.
        meaning.maxPlayers?.let { maxPlayers = it }
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
                throw ServerBackendException.Engine(Core.refusal("notAcceptingCommands"))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Introspection
    // -----------------------------------------------------------------------

    /**
     * What became of this server, which is [lastAnnounced] and deliberately
     * not `lifecycle.state`. See that field: the core drops a finished run, so
     * asking it would turn every crash into a `stopped`.
     */
    override fun status(serverId: String): ServerState =
        if (currentServerId == serverId) lastAnnounced else ServerState.STOPPED

    override fun players(serverId: String): PlayerRoster? {
        // Running is the core's word, not this file's — the roster is only
        // meaningful once the server is accepting people.
        if (serverId !in lifecycle.runningIds()) return null
        return PlayerRoster(roster.map { PlayerRoster.Player(it, null) }, maxPlayers)
    }

    override fun uptime(serverId: String): Instant? =
        if (currentServerId == serverId) startedAt else null

    /**
     * What the JVM is actually resident in, against the ceiling it was given.
     *
     * Read live rather than from the graph, because this answers a number the
     * Insights panel shows *now* and a caller may well ask between samples.
     */
    override fun memoryUsage(serverId: String): MemoryUsage? {
        if (currentServerId != serverId) return null
        val pid = serverPid?.takeIf { process?.isAlive == true }
        val usedKb = pid?.let { ProcMetrics.residentKb(it) }?.toInt()
        return MemoryUsage(usedKb = usedKb, maxMb = lastHeapMb)
    }

    /**
     * The most recent rate the core worked out, which is the same number the
     * graph's last point shows.
     *
     * Deliberately not measured on demand: a rate needs two readings separated
     * by enough time to mean something, and taking the second one here would
     * either block the caller or divide by an interval too short to trust.
     * Null until the sampler has two readings, which renders as "unavailable".
     */
    override fun cpuUsage(serverId: String): Double? {
        if (currentServerId != serverId) return null
        return metrics.samples().lastOrNull()?.cpuPercent
    }

    override fun perfHistory(serverId: String): List<PerfSample> =
        if (currentServerId != serverId) emptyList()
        else metrics.samples().map { PerfSample(it.t, it.memUsedMb, it.cpuPercent, it.playerCount) }

    override fun port(serverId: String): Int? =
        if (currentServerId == serverId) currentPort else null

    // -----------------------------------------------------------------------
    // Heap
    // -----------------------------------------------------------------------

    private var lastHeapMb: Int = 0

    /**
     * How much RAM this device has, which is all this file gets to say about
     * heap. What fraction of it is safe to hand a JVM is the core's — see
     * `homerun_core::minecraft::jvm::heap_mb`, which carries the reason.
     */
    private fun deviceTotalMb(): Int? = runCatching {
        val manager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        val info = ActivityManager.MemoryInfo().also { manager.getMemoryInfo(it) }
        (info.totalMem / (1024 * 1024)).toInt().takeIf { it > 0 }
    }.getOrNull()

    /** `Main-Class` from the jar manifest, which is what `java -jar` reads. */
    private fun mainClassOf(jar: File): String? = runCatching {
        java.util.jar.JarFile(jar).use { it.manifest?.mainAttributes?.getValue("Main-Class") }
    }.getOrNull()

    private fun transition(
        serverId: String,
        state: ServerState,
        backupInProgress: Boolean = false,
    ) {
        // Two different questions, and it is worth being clear which is which.
        //
        // The core answers *may this be said at all* — a launch still catching
        // up must not announce `running` for a server already on its way down,
        // or the UI flips the card to running and the API marks the service
        // healthy moments before it exits. It deliberately does not also
        // suppress an unchanged state: the core's clock and the host's are not
        // the same one, and letting it veto on "no change" is what once left a
        // running server showing as offline for ever.
        //
        // This file answers *have we already said it* — which is about the
        // event stream, not about the server, and is the host's own business.
        if (!lifecycle.mayAnnounce(serverId, state.wire)) return
        if (lastAnnounced == state) return
        lastAnnounced = state
        onStateChanged?.invoke(serverId, state, backupInProgress)
    }

    private companion object {
        const val TAG = "HomerunJava"
        const val DEFAULT_PORT = 25565

        /**
         * How often this file polls something it is waiting on. Its own
         * business — it is the granularity of a loop, not a rule about
         * servers.
         *
         * The waits themselves are not here: how long a launch may take, how
         * long a restart waits for its predecessor, and how long each rung of
         * the stop ladder gets are `homerun_core::minecraft::jvm`'s, along with
         * the heap ceiling and every refusal this file used to word for itself.
         */
        const val POLL_MS = 250L

        const val MAX_BUFFERED_LINES = 2000

        /** The two `native-server-network-error` kinds the contract defines. */
        const val PROVISIONING = "provisioning"
        const val HANDSHAKE = "handshake"


        /** `homerun-java-launcher` naming itself, ahead of the VM's own output. */
        val LAUNCHER_PID = Regex("""^\[launcher] pid=(\d+)$""")
    }
}

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
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.boolean
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
 * Player tracking reads the console rather than RCON — vanilla prints join and
 * leave lines, and parsing them costs nothing. That reading happens in the
 * supervisor, which is already looking at every line as it arrives; this file
 * asks it who is playing. RCON becomes worth adding when moderation
 * (kick/ban/op) lands.
 */
class JavaServerBackend(
    private val context: Context,
    private val scope: CoroutineScope,
) : ServerBackend {

    override val kind = "javaNative"
    override val engine = "spawned"

    /** The thread the supervisor's blocking `start` runs on. */
    private var engineThread: Thread? = null
    private var pumpJob: Job? = null

    /** Where this host has read the supervisor's console up to. */
    private var engineCursor = 0L

    /**
     * The gateway tunnel and the backup lifecycle, both shared with
     * [PumpkinBackend].
     *
     * These used to live in this file, which was fine while it was the only
     * backend that had them. It is not: neither is engine-specific — a tunnel
     * forwards a TCP port and restic reads a `world/` directory — and Pumpkin
     * shipped without either, so a Pumpkin server ran unreachable and backed
     * nothing up while looking healthy everywhere.
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
        finishConsole = { id -> drainConsole(id); stopLogPump() },
    )

    /**
     * Servers whose directory goes when they stop, recorded at the spawn.
     *
     * A minigame lobby is generated for one session and the API soft-deletes
     * the server record behind it, so the world it made is worth nothing to
     * anybody the moment the JVM exits. The exit handler is where that gets
     * acted on and by then the caller's `ServerConfig` is long out of scope,
     * which is the same reason [BackupSession] holds its contexts.
     *
     * On a phone this is not tidiness. A Paper server with a generated world
     * is a gigabyte or two, and a player who hosts three games in an evening
     * has given up several gigabytes to worlds nobody will ever open again —
     * on a device whose storage they cannot expand and where nothing in this
     * app would have told them where it went.
     */
    private val ephemeral: MutableSet<String> =
        java.util.concurrent.ConcurrentHashMap.newKeySet()

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

    /** Last roster size announced, so the pump only reports actual changes. */
    private var lastPlayerCount = -1

    override var onStateChanged: ((String, ServerState, Boolean) -> Unit)? = null
    override var onLog: ((String, String) -> Unit)? = null
    override var onPlayersChanged: ((String) -> Unit)? = null
    override var onNetworkError: ((String, String) -> Unit)? = null
    override var onBackupFinished: ((String) -> Unit)? = null

    // -----------------------------------------------------------------------
    // Storage
    // -----------------------------------------------------------------------

    /**
     * Every path this backend builds from an id goes through here, which is
     * what makes it the place to check one — a caller that reached the
     * filesystem without it would be a path the check does not cover. See
     * [requireValidServerId].
     */
    private fun dataDir(serverId: String): File =
        File(context.filesDir, "servers/${requireValidServerId(serverId)}").apply { mkdirs() }

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

    /**
     * Downloaded `.mrpack` archives, keyed by version id.
     *
     * A sibling of `servers/` for the same reason as [jarCacheDir]: shared
     * between servers on the same pack, and in `filesDir` because Android may
     * delete `cacheDir` under a running app — which here would cost a
     * several-hundred-megabyte download on the next start.
     */
    private fun modpackCacheDir(): File =
        File(context.filesDir, "modpacks").apply { mkdirs() }

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
        /**
         * Empty for Forge and NeoForge: their argfile supplies a module path
         * instead, and putting a jar on the class path alongside it is how
         * you get two copies of the same classes.
         */
        val classpath: List<File>,
        val mainClass: String,
        /**
         * What the launch actually resolved to — the pack's loader when there
         * is a pack, and the server's `TYPE` otherwise. [ModInstaller] must be
         * told the same one, or a Fabric pack on a server declared `paper`
         * would resolve its mods against the wrong facet.
         */
        val loader: String,
        /** Whatever an argfile carried, already in the form the VM accepts. */
        val extraJvmOptions: List<String> = emptyList(),
        /** `--launchTarget neoforgeserver` and friends, before `nogui`. */
        val extraProgramArgs: List<String> = emptyList(),
        /**
         * The version that was actually resolved, not the one requested —
         * `null` and `LATEST` both become a number here. [ModInstaller] needs
         * it to ask Modrinth what fits.
         */
        val mcVersion: String,
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
        if (lifecycle.supersedesOnStopBackup(serverId)) backups.cancelSuperseded(serverId)

        // Open the console before the slow work, not after: unpacking the
        // runtime and downloading a jar are minutes of a launch, and their
        // progress lines are the only thing the UI can show meanwhile.
        order.at("announceStarting")
        transition(serverId, ServerState.STARTING)
        reset()
        // Before the slow work, not after it. Everything below writes into the
        // supervisor's console — a jar being adopted, a world coming back —
        // and the pump is what turns those into events. Starting it at the
        // spawn, as it used to be, would mean the minutes before that arrived
        // in one burst at the end of the wait rather than as they happened.
        startLogPump(serverId)

        // Said out loud, because the alternative is what actually happened: a
        // settings lookup threw, this launched vanilla-latest with no loader,
        // no mods and no plugins, and every surface reported a healthy server
        // — the console included. The fallback is deliberate and stays, but a
        // server that is not the one the player configured has to say so
        // somewhere the player can see.
        if (config.settingsEnv == null) {
            note(
                serverId,
                "[Homerun] Could not read this server's settings — starting with " +
                    "defaults. Its version, loader, mods and plugins are not applied.",
            )
        }

        // Started now and awaited after the JVM is up. The gateway provisions
        // the peer asynchronously and the poll runs up to a minute, so doing
        // it here overlaps that with the download and the world generating,
        // exactly as the desktop provisions in parallel with Java booting.
        if (config.resolveTunnel != null) order.at("beginResolveTunnel")
        tunnel.begin(config.resolveTunnel)

        // Nothing below has started a process, so a failure here is a launch
        // that did not happen — reported as stopped, with the reason on the
        // call. `crashed` is reserved for a JVM that ran and died.
        order.at("ensureJar")
        // Heap size, the flags that carry it, what Minecraft's own main takes
        // and the EULA file — all the core's. This file supplies the one
        // thing only it can know, which is how much RAM the device has.
        // PowerNukkitX takes its own main, its own flags and no EULA, so the
        // game type is part of the question — see `jvm::NUKKIT_PROGRAM_ARGS`.
        val jvm = Core.jvmLaunch(config.memoryMb, deviceTotalMb(), config.gameType)
        lastHeapMb = jvm.heapMb
        // Both inputs and the answer, because a heap that comes out at the
        // floor is indistinguishable from one that was asked for — and the
        // difference is whether the device is small or the request was.
        Log.i(
            TAG,
            "$serverId: heap ${jvm.heapMb} MB " +
                "(asked ${config.memoryMb}, device ${deviceTotalMb() ?: -1})",
        )

        // A Bedrock server that happens to be a jar. Everything about the
        // *shape* of the launch is the same — download, pick a runtime, read a
        // `Main-Class`, spawn — and everything about its content differs, so
        // it forks here rather than threading conditionals through the loader
        // path it has nothing to do with.
        val nukkit = runCatching { Core.isNukkit(config.gameType) }.getOrDefault(false)

        val prepared = runCatching {
            val bundled = JavaRuntime.available(context)

            if (nukkit) {
                return@runCatching prepareNukkit(serverId, dir, config, bundled)
            }

            // A modpack goes first and outranks the server's own settings: the
            // manifest decides the loader, the Minecraft version *and* the
            // loader build, because a pack is built and tested against one
            // revision and a different one breaks its mixins at boot. The
            // desktop orders it the same way, running `setupModrinthModpack`
            // before `setupServerLoader` and using what it returned.
            val pack = ModpackInstaller.configured(config.settingsEnv)?.let { modpack ->
                ModpackInstaller.install(
                    dir = dir,
                    cacheDir = modpackCacheDir(),
                    modpack = modpack,
                    env = config.settingsEnv,
                    onLog = { note(serverId, it) },
                )
            }
            val wantedLoader = pack?.loader ?: config.loader
            val wantedVersion = pack?.mcVersion ?: config.version

            // Two shapes of loader, and the core says which this is: vanilla
            // and Paper publish a jar to download, Fabric publishes an
            // installer to run. Asking the core rather than matching on a name
            // here means there is no second loader list to drift.
            val installs = Core.loaderIsInstalled(wantedLoader)

            // Whichever shape, resolving comes first, and the ordering is
            // load-bearing now that the build ships more than one runtime:
            // which runtime to unpack follows from what is being launched. It
            // also means the ~170 MB unpack is only ever paid for a runtime
            // this server actually needs.
            // Three shapes now, and only the first two need a jar: a
            // downloaded server jar, an installed launch jar (Fabric), or an
            // argfile that names its own main class (Forge, NeoForge).
            var installed: ServerLoader.Installed? = null
            var jar: File? = null
            val mcVersion: String
            var javaMajor: Int
            var javaHome: File
            var libjvm: File

            if (installs) {
                // An installed loader has no artifact of its own — its
                // installer fetches the server jar — so the version and the
                // Java level come from Mojang's manifest, exactly as the
                // desktop takes them for every loader. The loader is passed
                // alongside it because *how strictly* that Java level binds is
                // the loader's business: Forge and NeoForge want it exactly.
                val resolved = ServerJar.resolveVanilla(wantedVersion)
                mcVersion = resolved.version
                javaMajor = Core.selectRuntime(resolved.toJson(), wantedLoader, bundled)
                javaHome = unpackRuntime(javaMajor)
                libjvm = libjvmOrRefuse(javaMajor)

                installed = ServerLoader.ensure(
                    dir = dir,
                    loader = wantedLoader,
                    mcVersion = resolved.version,
                    // The pack's pin, when there is a pack. Without one an
                    // install keeps whatever it has rather than chasing the
                    // newest loader on every start.
                    loaderVersion = pack?.loaderVersion,
                    runtime = ServerLoader.Runtime(launcher, javaHome, libjvm, tmpDir(dir)),
                    onLog = { note(serverId, it) },
                )
                jar = (installed as? ServerLoader.Installed.LaunchJar)?.jar

                // The installer has now produced a server jar, and that jar can
                // need a newer Java than Mojang's manifest claimed. The jar
                // wins, because it is the thing that fails.
                val needs = ServerLoader.bundlerJavaMajor(dir)
                if (needs != null && needs > javaMajor) {
                    Log.i(TAG, "$serverId: the bundler needs Java $needs, not $javaMajor")
                    note(serverId, "[Homerun] This version needs Java $needs.")
                    javaMajor = Core.selectRuntimeFor(
                        needs, "The server jar", wantedLoader, bundled,
                    )
                    javaHome = unpackRuntime(javaMajor)
                    libjvm = libjvmOrRefuse(javaMajor)
                }
            } else {
                val downloaded = ServerJar.ensure(
                    dir = dir,
                    cacheDir = jarCacheDir(),
                    version = wantedVersion,
                    loader = wantedLoader,
                    bundled = bundled,
                    onLog = { note(serverId, it) },
                )
                jar = downloaded.jar
                mcVersion = downloaded.mcVersion
                javaMajor = downloaded.javaMajor
                javaHome = unpackRuntime(javaMajor)
                libjvm = libjvmOrRefuse(javaMajor)

                // A downloaded-jar server never runs an installer, so this is
                // the only place it gets a marker — and its mod records need
                // one to live in. The desktop reaches the same state by
                // running `setupServerLoader` for every loader, vanilla too.
                LoaderMarker.putLoader(dir, wantedLoader, mcVersion, pack?.loaderVersion)
            }

            // Accept Mojang's EULA on the user's behalf, on every start —
            // byte-for-byte what the desktop app does in
            // `nativeServerManager.startServer`. The server will not boot
            // without it, and there is no acceptance step anywhere in the
            // product; `docs/android-server-backend.md` records that.
            //
            // Empty means this server has no EULA to accept, which is how the
            // core says so for PowerNukkitX. Writing it unconditionally would
            // create a file named `""`.
            if (jvm.eulaFile.isNotEmpty()) File(dir, jvm.eulaFile).writeText(jvm.eulaContents)

            // Where the main class comes from is the last thing that differs
            // between the three shapes. A jar carries it in its manifest;
            // Forge and NeoForge put it in the middle of an argfile, between
            // the module path and the launch target.
            val launch = when (val what = installed) {
                is ServerLoader.Installed.Argfiles -> Launch(
                    javaHome = javaHome,
                    libjvm = libjvm,
                    // Empty on purpose: the argfile supplies a module path,
                    // and adding a jar to the class path beside it is how you
                    // get two copies of the same classes.
                    classpath = emptyList(),
                    mainClass = what.expanded.mainClass!!,
                    loader = wantedLoader,
                    extraJvmOptions = what.expanded.jvmOptions,
                    extraProgramArgs = what.expanded.programArgs,
                    mcVersion = mcVersion,
                )
                else -> {
                    val runnable = jar ?: (what as ServerLoader.Installed.LaunchJar).jar
                    // The jar names the class to run. Doing this here keeps the
                    // native launcher free of zip parsing.
                    val mainClass = JavaProcess.mainClassOf(runnable)
                        ?: throw ServerBackendException.Engine(Core.refusal("noMainClass"))
                    Launch(
                        javaHome = javaHome,
                        libjvm = libjvm,
                        classpath = listOf(runnable),
                        mainClass = mainClass,
                        loader = wantedLoader,
                        mcVersion = mcVersion,
                    )
                }
            }
            Log.i(TAG, "$serverId: launching ${launch.mainClass} on Java $javaMajor")
            launch
        }.getOrElse { err ->
            // Nothing was spawned, so no exit will arrive to tidy up after
            // this one.
            stopLogPump()
            transition(serverId, ServerState.STOPPED)
            throw err as? ServerBackendException ?: ServerBackendException.Engine(
                err.message ?: "The server could not be prepared."
            )
        }

        // Held for the stop path: the exit handler needs the repository and
        // device id, and by then the caller's config is long gone.
        backups.hold(serverId, config.backupContext)

        // Cheapest place to notice: nothing has been spawned, so there is
        // nothing to tear down.
        if (abandonIfStopped(serverId)) return

        // A start admitted *during* a stop is a restart, and the core says
        // whether an outgoing engine still has to be waited for. Asked here,
        // immediately before spawning, because the old JVM usually exits while
        // this launch was preparing.
        order.at("awaitPreviousExit")
        if (lifecycle.awaitPreviousExit(serverId)) awaitPreviousExit(serverId)

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
        backups.restore(serverId, dir, config.backupContext)

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

        // After settings and before the spawn, which is where the desktop puts
        // it (`startServer`: loader, properties, mods, spawn). It is deliberately
        // outside the `prepared` block above: a mod that cannot be fetched must
        // not stop a server starting, and `sync` never throws for a mod-shaped
        // reason. Every loader gets this — a Paper server's plugins go through
        // exactly the same resolver as a Fabric server's mods.
        // Not for a Bedrock server: PowerNukkitX loads no Fabric mod and no
        // Bukkit plugin, so the resolver would go looking for facets that do
        // not exist — and `mods::sweep` deletes jars it does not recognise,
        // which on this engine is all of them.
        if (!nukkit) ModInstaller.sync(
            dir = dir,
            loader = prepared.loader,
            mcVersion = prepared.mcVersion,
            gameType = config.gameType,
            env = config.settingsEnv,
            onLog = { note(serverId, it) },
        )

        // After the mods and for the same reason [PluginInstaller] is: this
        // jar is not on Modrinth, so `mods::sweep` has no record of it. That
        // makes it safe from the sweep rather than at risk from it — the sweep
        // only ever deletes what it installed itself — but the ordering is the
        // convention and there is no reason to be the exception.
        //
        // A no-op for every server that is not crossplay, and never fatal: a
        // crossplay server without Floodgate is still a working Java server,
        // and the players who lose out are the Bedrock ones. That is a smaller
        // loss than not starting at all.
        CrossplayInstaller.sync(
            dir = dir,
            loader = prepared.loader,
            gameType = config.gameType,
            onLog = { note(serverId, it) },
        )

        // Immediately after the mods, and that order is load-bearing rather
        // than stylistic: `mods::sweep` deletes jars in the directory it does
        // not recognise, and these are jars it has never heard of. The desktop
        // sequences it the same way, for the same reason.
        //
        // Unlike the line above, this one can stop a launch. A mod is
        // decoration on a world that exists without it; these jars *are* the
        // game, and a BedWars lobby with no BedWars in it is not a server
        // anybody asked for. See [PluginInstaller].
        if (!nukkit) PluginInstaller.sync(
            dir = dir,
            loader = prepared.loader,
            env = config.settingsEnv,
            onLog = { note(serverId, it) },
        )

        if (order.at("spawn")) return

        // Everything the supervisor needs to run this server, and nothing it
        // could work out for itself. What makes a JVM start on Android is
        // [JavaProcess]'s, because a loader installer is the same launch with
        // a different classpath; what a *Minecraft server* is given is the
        // core's. This line is where those two meet and nothing else decides
        // either half.
        //
        // A loader's argfile contributes to both halves — its module path is a
        // JVM option and its `--launchTarget` is a program argument — and its
        // program arguments go **before** the core's, because `nogui` is the
        // last thing a Minecraft server expects to be told.
        val invocation = JavaProcess.invocation(
            launcher = launcher,
            javaHome = prepared.javaHome,
            libjvm = prepared.libjvm,
            classpath = prepared.classpath,
            mainClass = prepared.mainClass,
            jvmOptions = jvm.options + prepared.extraJvmOptions,
            programArgs = prepared.extraProgramArgs + jvm.programArgs,
            workDir = dir,
            tmpDir = tmpDir(dir),
            // Two sources, and the second one is not obvious. A server's
            // settings are written into files, never into this map — that is
            // the rule [ServerConfig.extra] exists to state. But our own
            // plugins read theirs with `System.getenv`, so the handful of keys
            // in our namespace have to cross over, and the core decides which
            // those are: [Core.pluginEnv] forwards `MINIGAME*`/`BEDWARS*` and
            // refuses everything else. Without it the host's chosen match size
            // never reached the game and every match started at the plugin's
            // built-in default of two.
            extraEnv = config.extra.mapNotNull { (k, v) ->
                (v as? String)?.let { k to it }
            }.toMap() + Core.pluginEnv(config.settingsEnv),
        ).toJson()

        currentServerId = serverId
        currentPort = port

        // Recorded here rather than at the top of the launch, so it describes
        // a run that actually happened. A launch that failed before this line
        // leaves its directory alone: the next start reuses it, and deleting
        // the world a failed launch had already restored would turn a retry
        // into a loss.
        if (Core.isMinigame(config.settingsEnv)) ephemeral += serverId else ephemeral -= serverId

        // From here the supervisor in `homerun-pumpkin-ffi` owns the process:
        // it spawns it, reads its console, climbs the stop ladder and reports
        // what the exit meant. This host no longer manages any of that — the
        // same state machine runs the linked engine on iOS.
        engineThread = NativeServer.startBlocking(
            serverId,
            dir.absolutePath,
            port,
            invocation.toString(),
        ) { result ->
            scope.launch { serverExited(serverId, result) }
        }

        // There is now something to stop, and something whose exit will need
        // judging. The core pins this launch's generation here, so a process
        // that outlives a stop-then-restart is recognised as the old one's.
        lifecycle.spawned(serverId)
        // "Done" is the JVM telling us it is accepting connections. Waiting
        // for the process to merely exist would report a server that cannot
        // be joined yet; the bridge has no timeout so waiting is correct.
        //
        // The supervisor is the one watching for it: it reads every console
        // line already, and `running` is what it calls having seen `Done (…)`.
        // This host used to classify the same lines a second time to learn the
        // same fact.
        order.at("awaitConsole")
        val ready = withTimeoutOrNull(Core.jvmLimits().startTimeoutMs) {
            while (true) {
                if (engineThread?.isAlive != true) return@withTimeoutOrNull false
                if (engineState() == ServerState.RUNNING) return@withTimeoutOrNull true
                delay(POLL_MS)
            }
            @Suppress("UNREACHABLE_CODE") false
        }

        if (ready != true) {
            tunnel.cancel()
            val stillUp = engineThread?.isAlive == true
            if (stillUp) withContext(Dispatchers.IO) { NativeServer.nativeStop() }
            transition(serverId, ServerState.CRASHED)
            throw ServerBackendException.Engine(
                if (stillUp) Core.refusal("startTimedOut")
                else "The server stopped unexpectedly while starting."
            )
        }

        // There is a console now, which is what makes a *graceful* stop
        // possible — the core needs to know before it can say so.
        lifecycle.consoleReady(serverId)

        // A stop that landed while the JVM was booting: the console exists
        // now, so it can be asked politely rather than killed. Reached when
        // the stop arrived before `process` was assigned — after that, `stop`
        // waits for the console itself.
        if (lifecycle.shouldAbandon(serverId)) {
            Log.i(TAG, "$serverId: honouring a stop that arrived during startup")
            withContext(Dispatchers.IO) {
                tunnel.shutdown()
                NativeServer.nativeStop()
            }
            return
        }

        if (config.resolveTunnel != null) order.at("openTunnel")
        tunnel.open(serverId, dir, port, Core.exposureFor(config.gameType))

        // Only now. The server accepting connections on loopback is not the
        // same as players being able to reach it, and reporting `running`
        // before the tunnel is up is how a server looks healthy to everyone
        // except the people trying to join. The desktop learned this too.
        order.at("announceRunning")
        startedAt = Instant.now()
        transition(serverId, ServerState.RUNNING)
    }

    /**
     * The PowerNukkitX half of a launch.
     *
     * Everything the Java path does before spawning, minus everything that is
     * about Java servers: no modpack, no loader, no installer, no argfile, no
     * EULA. What is left is a jar with a `Main-Class`, which is why this ends
     * up in the same [Launch] the other three shapes produce and why nothing
     * downstream of it knows the difference.
     *
     * The version pin is the server's `VERSION`, which is the API's field and
     * therefore the pin of record. `LATEST` or nothing means the newest stable
     * release — the jar is data, so that is how a new PowerNukkitX reaches
     * players without a store update, and naming a release here is how a bad
     * one is stopped without one either.
     */
    private suspend fun prepareNukkit(
        serverId: String,
        dir: File,
        config: ServerConfig,
        bundled: List<Int>,
    ): Launch {
        val blessed = config.version
            ?.takeIf { it.isNotBlank() && !it.equals("LATEST", ignoreCase = true) }

        val downloaded = ServerJar.ensureNukkit(
            dir = dir,
            cacheDir = jarCacheDir(),
            blessed = blessed,
            bundled = bundled,
            onLog = { note(serverId, it) },
        )

        val mainClass = JavaProcess.mainClassOf(downloaded.jar)
            ?: throw ServerBackendException.Engine(Core.refusal("noMainClass"))

        Log.i(TAG, "$serverId: PowerNukkitX ${downloaded.mcVersion} on Java ${downloaded.javaMajor}")

        return Launch(
            javaHome = unpackRuntime(downloaded.javaMajor),
            libjvm = libjvmOrRefuse(downloaded.javaMajor),
            classpath = listOf(downloaded.jar),
            mainClass = mainClass,
            // Not a loader anyone can install; the field is what `ModInstaller`
            // would resolve against, and nothing calls it for this game type.
            loader = "vanilla",
            mcVersion = downloaded.mcVersion,
        )
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
     * Wait out the JVM a previous launch left behind, having been told by the
     * core that there is one. Bounded, because a wedged process must not block
     * a start for ever; refusing is better than spawning a second server into
     * one directory.
     */
    private suspend fun awaitPreviousExit(serverId: String) {
        val previous = engineThread ?: return
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
        tunnel.cancel()
        // Reached before anything was spawned, so nothing else will stop it.
        stopLogPump()
        transition(serverId, ServerState.STOPPED)
        return true
    }

    override suspend fun stop(serverId: String, graceful: Boolean) {
        if (engineThread?.isAlive != true) {
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
            tunnel.shutdown()
            // The ladder is climbed inside the supervisor, which owns the
            // process and its stdin. `graceful` is the core's word for
            // "there is a console that can hear a stop", and the supervisor
            // reaches the same answer from the same place — so there is
            // nothing left for this host to escalate.
            if (!graceful) {
                Log.i(TAG, "$serverId: stopping before the console was ready")
            }
            val reply = NativeServer.nativeStop()
            if (!ok(reply)) {
                Log.w(TAG, "$serverId: the supervisor could not stop it: $reply")
            }
        }
    }

    private fun ok(reply: String): Boolean = runCatching {
        Json.parseToJsonElement(reply).jsonObject["ok"]?.jsonPrimitive?.boolean
    }.getOrNull() == true

    override val runningServerIds: List<String>
        get() = lifecycle.runningIds()


    // -----------------------------------------------------------------------
    // Console
    // -----------------------------------------------------------------------

    /**
     * The graph the supervisor built while this run was up.
     *
     * Nothing here samples anything. The supervisor owns the process, so it
     * is the only thing that knows what to measure — and `homerun-core`
     * decides what the readings mean and how much to keep. This host asks.
     */
    private fun engineSamples(): List<PerfSample> = runCatching {
        val obj = Json.parseToJsonElement(NativeServer.nativeMetrics()).jsonObject
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

    /**
     * Page the supervisor's console into this host's buffer.
     *
     * A cursor rather than a stream: the process belongs to
     * `homerun-pumpkin-ffi` now, and its console is read the same way the
     * Pumpkin backend reads its own. Kept locally as well because this buffer
     * also holds the host's own notes — the jar download, the runtime unpack —
     * which happen minutes before there is a server to have a console.
     */
    private fun startLogPump(serverId: String) {
        pumpJob?.cancel()
        // The cursor is deliberately *not* reset. Sequence numbers are
        // monotonic across the buffer being cleared, so carrying it over is
        // what stops a new launch replaying the last run's console — and it
        // still picks up everything this launch has written.
        pumpJob = scope.launch(Dispatchers.IO) {
            while (true) {
                drainConsole(serverId)
                pollPlayers(serverId)
                delay(POLL_MS)
            }
        }
    }

    /** Nothing more will be written for this run, so stop paging it. */
    private fun stopLogPump() {
        pumpJob?.cancel()
        pumpJob = null
    }

    private fun drainConsole(serverId: String) {
        val slice = runCatching { parseLogs(NativeServer.nativeLogsSince(engineCursor)) }
            .getOrNull() ?: return
        engineCursor = slice.second
        for (raw in slice.first) {
            val line = raw.trim()
            if (line.isEmpty()) continue
            onLog?.invoke(serverId, line)
        }
    }

    /**
     * Notice the roster changing, so the UI is told rather than having to poll.
     *
     * A count rather than the names: the supervisor keeps the roster itself,
     * and the event carries no payload — it is a nudge to come and ask.
     */
    private fun pollPlayers(serverId: String) {
        val count = players(serverId)?.players?.size ?: return
        if (count != lastPlayerCount) {
            lastPlayerCount = count
            onPlayersChanged?.invoke(serverId)
        }
    }

    private fun parseLogs(raw: String): Pair<List<String>, Long> {
        val obj = Json.parseToJsonElement(raw).jsonObject
        val lines = obj["lines"]?.jsonArray?.mapNotNull { it.jsonPrimitive.contentOrNull }
            ?: emptyList()
        return lines to (obj["cursor"]?.jsonPrimitive?.longOrNull ?: engineCursor)
    }

    /**
     * The supervisor reported the run is over.
     *
     * Was the tail of the console pump, back when this host owned the process.
     * The reasoning is unchanged; only who noticed has moved.
     */
    private suspend fun serverExited(serverId: String, result: String) {
        // The last of the console, including whatever it said on the way down.
        // The pump keeps running past this: an on-stop backup writes `[Backup]`
        // lines for minutes after the JVM is gone, and they are console lines
        // like any other. It is stopped below, once there is genuinely nothing
        // left to write.
        drainConsole(serverId)

        val reply = runCatching { Json.parseToJsonElement(result).jsonObject }.getOrNull()
        val ok = reply?.get("ok")?.jsonPrimitive?.booleanOrNull == true
        // The supervisor's own account of why. It also writes a refusal into
        // the console, which is what the crash report is built from; this is
        // for whoever is reading logcat, where nothing else says it.
        if (!ok) {
            val why = reply?.get("error")?.jsonPrimitive?.contentOrNull ?: "no reason given"
            Log.w(TAG, "$serverId did not run cleanly: $why")
        }
        // There is no process exit code to read any more — the supervisor
        // reports whether the run unwound cleanly, and 0 stands in for that.
        val code = if (ok) 0 else 1
        run {
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
            tunnel.shutdown()
            // The stop ack carries `backup_in_progress`, which is what opens
            // the backup lease — so it is claimed only when a backup is
            // actually about to run, and `backupAfterStop` reports an outcome
            // either way, because only that closes it again.
            val backup = backups.claim(serverId, outcome)

            // `force`: the core ruled on this exit a few lines ago, and
            // `superseded` is how it says a newer launch owns this server.
            transition(
                serverId,
                if (outcome == "crashed") ServerState.CRASHED else ServerState.STOPPED,
                backupInProgress = backup != null,
                force = !verdict.superseded,
            )
            engineThread = null
            currentServerId = null
            startedAt = null
            currentPort = null

            if (backup != null) backups.runAfterStop(serverId, backup) else stopLogPump()

            // Last, and only once nothing else is going to read the directory.
            // A minigame never has a backup to wait for — it is excluded from
            // them at the source, in the bridge — so the `backup != null`
            // branch above is unreachable for one, and this cannot pull the
            // world out from under restic. Sequenced after the branch anyway,
            // because "unreachable today" is not a thing to build a delete on.
            if (ephemeral.remove(serverId)) discardEphemeral(serverId)
        }
    }

    /**
     * Give back the storage a finished lobby was using.
     *
     * Best effort by design. A directory that will not delete is worth a log
     * line and nothing more — the alternative is failing a stop the player
     * already watched succeed, over a world neither of us wants.
     */
    private fun discardEphemeral(serverId: String) {
        val dir = dataDir(serverId)
        val freed = runCatching { dir.walkBottomUp().sumOf { if (it.isFile) it.length() else 0L } }
            .getOrDefault(0L)

        if (!dir.deleteRecursively()) {
            Log.w(TAG, "$serverId: could not delete the finished minigame directory")
            return
        }
        Log.i(TAG, "$serverId: discarded a finished minigame, freeing ${freed / (1024 * 1024)} MB")

        // The same reason [delete] does it: the server jar's bytes are in the
        // cache as well as in the directory just removed, and a lobby is
        // exactly the short-lived server that would otherwise leave a Paper
        // jar pinned in the cache for nothing.
        runCatching {
            ServerJar.dropUnusedCacheEntries(jarCacheDir(), File(context.filesDir, "servers"))
        }.onFailure { Log.w(TAG, "$serverId: could not sweep the jar cache: ${it.message}") }
    }

    /**
     * A line from the host rather than from the server — jar downloads, runtime
     * unpacking, the world coming back from a backup.
     *
     * Into the supervisor's console, which is the same one the server writes
     * to, so a console opened after a slow launch still shows where the time
     * went. This host kept its own buffer for exactly this until the core
     * grew somewhere to put them; the emit is still here because the UI wants
     * the line now, not on its next poll.
     *
     * A note only ever appends. Clearing the previous run's console is
     * [reset]'s job, through `homerun_server_console_begin` — an implicit
     * "the first note clears" would wipe the console of the run a player had
     * just watched stop, because the on-stop backup writes notes after a run
     * ends.
     */
    private fun note(serverId: String, line: String) {
        Log.i(TAG, line)
        // Written, not emitted. The pump is the only thing that turns a
        // console line into an event now — emitting here as well is how the
        // tunnel's two lines came out twice, once from this call and once
        // from the pump reading the very same buffer a moment later.
        //
        // Loud on failure: this is the only copy of the launch narrative, so a
        // note that never lands leaves a console that silently begins at the
        // spawn, which reads as a launch that took no time rather than as a
        // broken one.
        runCatching { NativeServer.nativeNote(line) }
            .onFailure { Log.w(TAG, "note did not reach the console: ${it.message}", it) }
    }

    /**
     * The console, paged by cursor — the supervisor's, not a copy of it.
     *
     * Same buffer the pump drains and the same one the host's own notes go
     * into, so the launch narrative and the server's output are one sequence
     * in the order they actually happened.
     */
    override fun logs(serverId: String, cursor: Int): LogSlice {
        if (currentServerId != serverId) return LogSlice(emptyList(), cursor)
        val obj = runCatching { Json.parseToJsonElement(NativeServer.nativeLogsSince(cursor.toLong())).jsonObject }
            .getOrNull() ?: return LogSlice(emptyList(), cursor)
        val lines = obj["lines"]?.jsonArray?.mapNotNull { it.jsonPrimitive.contentOrNull } ?: emptyList()
        val next = obj["cursor"]?.jsonPrimitive?.longOrNull ?: cursor.toLong()
        return LogSlice(lines, next.toInt())
    }

    /**
     * A launch is beginning.
     *
     * The console this launch will write into is emptied here, at the top,
     * rather than by `start` — everything between the two is the part a player
     * most wants explained when a start is slow.
     */
    private fun reset() {
        lastPlayerCount = -1
        runCatching { NativeServer.nativeConsoleBegin() }
            .onFailure { Log.w(TAG, "the console was not cleared for this launch: ${it.message}") }
    }

    override suspend fun command(serverId: String, command: String) {
        if (currentServerId != serverId || engineThread?.isAlive != true) {
            throw ServerBackendException.NotRunning(serverId)
        }
        // Onto the server's stdin, by the supervisor that holds it.
        val reply = withContext(Dispatchers.IO) { NativeServer.nativeCommand(command) }
        if (!ok(reply)) {
            throw ServerBackendException.Engine(Core.refusal("notAcceptingCommands"))
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

    /**
     * The supervisor's own view of the run, which is **not** [status].
     *
     * It reports `running` the moment the console says `Done (…)`; this host
     * holds `running` back until the tunnel is up as well, because a server
     * nobody can reach is not a running server. Both are right about different
     * questions, and this one answers "is there a console yet".
     */
    private fun engineState(): ServerState {
        val wire = runCatching {
            Json.parseToJsonElement(NativeServer.nativeState())
                .jsonObject["state"]?.jsonPrimitive?.contentOrNull
        }.getOrNull()
        return ServerState.entries.firstOrNull { it.wire == wire } ?: ServerState.STOPPED
    }

    /**
     * Who is playing, according to the supervisor that read the console.
     *
     * Built there rather than here: it sees every line as it arrives, and
     * classifying them twice is how two answers to one question appear. The
     * names carry no UUID — console lines have none to give, and the Pumpkin
     * backend fills that field from a source this one does not have.
     */
    override fun players(serverId: String): PlayerRoster? {
        // Running is the core's word, not this file's — the roster is only
        // meaningful once the server is accepting people.
        if (serverId !in lifecycle.runningIds()) return null
        val raw = NativeServer.nativePlayers()
        if (raw.trim() == "null") return null
        val obj = runCatching { Json.parseToJsonElement(raw).jsonObject }.getOrNull() ?: return null
        val list = obj["players"]?.jsonArray?.map {
            val player = it.jsonObject
            PlayerRoster.Player(
                name = player["name"]?.jsonPrimitive?.contentOrNull.orEmpty(),
                uuid = player["uuid"]?.jsonPrimitive?.contentOrNull,
            )
        } ?: emptyList()
        return PlayerRoster(list, obj["max"]?.jsonPrimitive?.intOrNull)
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
        // The newest point on the supervisor's graph, which is where the
        // reading came from in the first place.
        val usedMb = engineSamples().lastOrNull()?.memUsedMb
        return MemoryUsage(usedKb = usedMb?.times(1024), maxMb = lastHeapMb)
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
        return engineSamples().lastOrNull()?.cpuPercent
    }

    override fun perfHistory(serverId: String): List<PerfSample> =
        if (currentServerId != serverId) emptyList() else engineSamples()

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
    /**
     * Unpack one staged runtime, and collect any this build no longer ships.
     *
     * Blocking the first time — a hundred and seventy megabytes out of the
     * APK — and the first start on a device pays for it. The bridge has no
     * call timeout precisely so this is allowed to take as long as it takes.
     *
     * Called more than once per launch on the loader path: the installer runs
     * on the runtime Mojang's manifest asked for, and the jar it produces can
     * then ask for a different one. Both calls are cheap after the first.
     */
    private suspend fun unpackRuntime(major: Int): File = withContext(Dispatchers.IO) {
        JavaRuntime.ensure(context, major).also {
            // An app updated from a build that staged a different set leaves
            // half a gigabyte of unpacked runtime that nothing will ever
            // launch again. Nothing else would collect it: the unpack is keyed
            // by major, so a runtime that stopped being staged simply stops
            // being asked for.
            JavaRuntime.dropUnusedRuntimes(context)
        }
    }

    private fun libjvmOrRefuse(major: Int): File =
        JavaRuntime.libjvm(context, major)
            ?: throw ServerBackendException.Engine(Core.refusal("brokenJavaRuntime"))

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
        if (!force && !lifecycle.mayAnnounce(serverId, state.wire)) return
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


    }
}

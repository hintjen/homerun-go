package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob

/**
 * Process-level owner of the running server.
 *
 * The engine must outlive the UI. A WebView can be destroyed and rebuilt, and
 * the activity can be finished, while a server keeps running — so the backend
 * cannot hang off `lifecycleScope`, which is exactly where it started out.
 * Cancelling that scope would kill the log pump and orphan the engine thread
 * with nothing draining its output.
 *
 * This also multiplexes the backend's callbacks. `PumpkinBackend` has one slot
 * per event, but two things need them: the bridge (to reach the UI) and the
 * foreground service (to keep its notification current). Listeners register
 * here instead of fighting over the slots.
 *
 * And it owns one decision, [hosting]: whether this device is still busy. That
 * is what [HostingService] is started and stopped by, and it is not the same
 * question as "is a server running" — see below.
 */
object ServerHost {

    /**
     * Deliberately not tied to any lifecycle. Cancelled never — the process
     * dying is what ends it, which is the same thing that ends the server.
     *
     * Shared with [DeviceWebsocket], which needs the same guarantee for the
     * same reason: its link outlives every page and must not be taken down by
     * one being rebuilt.
     *
     * The handler is not decoration — see [keepAlive]. Nearly everything the
     * host does off the main thread runs here: both backends' log pumps, the
     * backend's exit callback, [Reporting]'s loop and its fire-and-forget API
     * calls, and the device link. Any one of them throwing would otherwise end
     * the process that *is* the server.
     */
    val scope = CoroutineScope(
        SupervisorJob() + Dispatchers.Default + keepAlive(TAG, "the host's background work"),
    )

    private lateinit var appContext: Context
    private val listeners = mutableSetOf<Listener>()

    lateinit var backend: ServerBackend
        private set

    /**
     * Who owns a server right now — the bookkeeping the bridge and the backend
     * both consult, answered by `homerun-core::lifecycle`.
     *
     * Process-scoped like the backend, and for the same reason: a page reload
     * must not lose track of a running server. `"one"` because this host runs
     * a single server at a time, which is `multipleRunningServers: false` in
     * the capabilities the UI reads.
     */
    val lifecycle = Core.Lifecycle("one")

    interface Listener {
        fun onLog(serverId: String, line: String) {}
        fun onStateChanged(serverId: String, state: ServerState, backupInProgress: Boolean = false) {}
        fun onPlayersChanged(serverId: String) {}
        fun onNetworkError(serverId: String, kind: String) {}

        /**
         * The on-stop backup finished — successfully or not, and either way
         * this device is finally idle.
         *
         * The bridge ignores it; nothing in the UI is waiting. It exists
         * because [HostingService] cannot stand down without it: the run is
         * over, the state is already `stopped`, and the world is still
         * uploading.
         */
        fun onBackupFinished(serverId: String) {}
    }

    @Synchronized
    fun init(context: Context) {
        if (::backend.isInitialized) return
        appContext = context.applicationContext
        // Java is the Android product: a real server jar on a real JVM, with
        // the mods, plugins and parity that implies. Pumpkin is the fallback
        // for builds that ship no JRE — and the only option on iOS, which
        // cannot spawn a process at all.
        val java = JavaRuntime.isAvailable(appContext)
        Log.i(TAG, if (java) "using the bundled JVM" else "no JVM bundled — falling back to Pumpkin")
        backend = (if (java) JavaServerBackend(appContext, scope) else PumpkinBackend(appContext, scope)).apply {
            onLog = { id, line -> forEach { it.onLog(id, line) } }
            onPlayersChanged = { id -> forEach { it.onPlayersChanged(id) } }
            onNetworkError = { id, kind -> forEach { it.onNetworkError(id, kind) } }
            onStateChanged = { id, state, backingUp ->
                // Before the fan-out, not after. The service is what stops
                // Android reclaiming this process, and the transition to
                // `starting` is followed immediately by minutes of downloading
                // and unpacking — work that must not be interruptible while
                // listeners are still being called.
                track(id, state, backingUp)
                forEach { it.onStateChanged(id, state, backingUp) }
            }
            onBackupFinished = { id ->
                track(id, null, false)
                forEach { it.onBackupFinished(id) }
            }
        }
    }

    @Synchronized
    fun addListener(listener: Listener) {
        listeners.add(listener)
    }

    @Synchronized
    fun removeListener(listener: Listener) {
        listeners.remove(listener)
    }

    /**
     * Put a line of Homerun's own into a server's console.
     *
     * For what this app worked out and the server did not say — why a crash
     * happened, that an operator change was saved. It reaches the UI on the
     * same stream as the server's own output, which is where a player is
     * already looking when something has gone wrong.
     */
    fun note(serverId: String, line: String) {
        // Some messages arrive already badged — `homerun-core` writes the
        // prefix into the lines it hands back, because the desktop puts them
        // straight into its log. Adding a second one produced
        // "[Homerun] [Homerun] Operator change saved…" in front of a player.
        val prefixed = if (line.startsWith(BADGE)) line else "$BADGE$line"
        forEach { it.onLog(serverId, prefixed) }
    }

    /** How Homerun's own lines are marked in a server's console. */
    private const val BADGE = "[Homerun] "

    /** Copy before dispatch: a listener may unregister while being called. */
    private fun forEach(action: (Listener) -> Unit) {
        val snapshot = synchronized(this) { listeners.toList() }
        for (listener in snapshot) {
            runCatching { action(listener) }
        }
    }

    /**
     * The server that is running right now, if any.
     *
     * A page that reloads, or an activity recreated after process death,
     * has no idea a server is up. It asks this on reconnect rather than
     * assuming continuity (PROTOCOL.md §4.3).
     */
    fun runningServerId(): String? = backend.runningServerIds.firstOrNull()

    // -----------------------------------------------------------------------
    // Is this device busy?
    // -----------------------------------------------------------------------

    /**
     * What the notification says, and what decides whether there is one.
     *
     * [state] is the last state announced for [serverId], so `stopped` with
     * [backingUp] true is a real and important combination — the run is over
     * and the world is still going up.
     */
    data class Hosting(
        val serverId: String?,
        val name: String?,
        val state: ServerState,
        val backingUp: Boolean,
        val players: Int?,
        /**
         * A `native-server-start` call has been admitted and has not returned.
         *
         * Tracked separately from [state] because the first minute of a launch
         * happens before the backend announces anything: the settings lookup
         * and the backup-lease check are both network round-trips made with
         * nothing spawned and nothing to report.
         */
        val starting: Boolean = false,
    ) {
        /** Anything to stop. False during a backup: the upload is not cancellable. */
        val stoppable: Boolean
            get() = serverId != null && !backingUp &&
                (starting || state == ServerState.STARTING || state == ServerState.RUNNING)

        /**
         * Whether this device is doing something it must not be killed for.
         *
         * The backup is the term that is easy to miss. It runs for minutes
         * *after* the server has stopped, and it is uploading the session that
         * is not yet in the repository — so killing it does not lose a server,
         * it loses the play. That is why this is not
         * `runningServerIds.isNotEmpty()`.
         */
        val busy: Boolean
            get() = starting || backingUp || state == ServerState.STARTING ||
                state == ServerState.RUNNING || state == ServerState.STOPPING
    }

    private var hostingId: String? = null
    private var hostingName: String? = null
    private var hostingState: ServerState = ServerState.STOPPED
    private var hostingBackup: Boolean = false
    private var hostingStarting: Boolean = false
    private var serviceWanted: Boolean = false

    /** One host-failure line per run — see [noteHostFailure]. */
    private var failureNoted: Boolean = false

    /**
     * The last snapshot, readable with no lock and no JNI. For the crash
     * handler in [HomerunApplication] and nothing else.
     *
     * A dying thread must not call [hosting]: that reaches into the supervisor's
     * own mutex through JNI, and the thread that is crashing may be the one
     * holding it. A crash handler that blocks turns a crash into a hang, which
     * is the single outcome worse than the crash — a slightly stale answer is
     * worth more than that.
     */
    @Volatile
    private var lastHosting: Hosting = Hosting(null, null, ServerState.STOPPED, false, null)

    @Synchronized
    private fun snapshot(): Hosting =
        Hosting(hostingId, hostingName, hostingState, hostingBackup, null, hostingStarting)
            .also { lastHosting = it }

    /** What this device was doing, in a few words, for that handler's log. */
    fun hostingSummary(): String = lastHosting.let {
        when {
            !it.busy -> "idle"
            it.backingUp -> "backing up ${it.serverId}"
            else -> "hosting ${it.serverId} (${it.state})"
        }
    }

    /**
     * A snapshot, safe to read from any thread.
     *
     * The roster is fetched **outside** the lock, on purpose. It is a JNI call
     * into the supervisor's own mutex, and holding this monitor across a lock
     * the server thread also takes is how a notification refresh would come to
     * deadlock a running server.
     */
    fun hosting(): Hosting {
        val snapshot = snapshot()
        val id = snapshot.serverId ?: return snapshot
        // Read live rather than cached: the pump signals that the roster
        // changed, not what it changed to.
        return snapshot.copy(
            players = runCatching { backend.players(id)?.players?.size }.getOrNull(),
        )
    }

    /**
     * A user asked to host, before any of it has begun.
     *
     * Called from the bridge's start handler rather than waiting for the
     * backend's first state change, because the two network round-trips in
     * between — the settings lookup and the backup-lease check — happen with
     * this process still merely cached. It is also the only place the server's
     * display name is known; the backend is never told it.
     *
     * Must be paired with [hostingSettled], or the process stays pinned in the
     * foreground for the rest of its life.
     */
    @Synchronized
    fun hostingRequested(serverId: String, name: String?) {
        hostingId = serverId
        hostingName = name
        hostingStarting = true
        // A new run gets its own console, and its own chance to be told that
        // something inside Homerun went wrong during it.
        failureNoted = false
        syncHosting()
    }

    /**
     * The start call has returned, one way or another.
     *
     * The failures are why this exists. A launch refused by the backup lease,
     * or by a game type this host cannot run, throws before the backend has
     * announced a single state — so there is no `stopped` coming to stand the
     * service down, and without this the notification would sit there
     * describing a server that was never started.
     */
    @Synchronized
    fun hostingSettled(serverId: String) {
        if (hostingId != serverId) return
        hostingStarting = false
        syncHosting()
    }

    /**
     * Fold one event into the hosting snapshot. [state] null means "only the
     * backup changed" — which is [Listener.onBackupFinished].
     */
    @Synchronized
    private fun track(serverId: String, state: ServerState?, backupInProgress: Boolean) {
        if (state != null) {
            hostingId = serverId
            hostingState = state
        }
        // A terminal state carries whether a backup follows it, so this is
        // assignment and not an or: `stopped, false` genuinely means idle.
        hostingBackup = backupInProgress
        syncHosting()
    }

    /**
     * Bring the foreground service into line with [hosting].
     *
     * Idempotent and cheap to over-call. Public because a caller that changes
     * the answer without going through an event — the bridge, admitting a start
     * — needs to be able to say so.
     */
    @Synchronized
    fun syncHosting() {
        val busy = snapshot().busy
        if (busy == serviceWanted) {
            // Already up: refresh in place rather than re-entering the service.
            // The service listens for its own updates.
            return
        }
        serviceWanted = busy
        if (busy) HostingService.start(appContext) else HostingService.stop(appContext)
    }

    /**
     * Re-post the notification although nothing about the server changed.
     *
     * One caller, and it exists because of a failure that is invisible from the
     * code: a foreground service started while POST_NOTIFICATIONS was denied
     * keeps its notification *attached and unposted*. The service runs, the
     * process is protected, and there is nothing on screen — and granting the
     * permission a moment later does not retroactively post it. Only entering
     * the foreground again does, which is what this does.
     *
     * Observed on API 35: `dumpsys activity services` showed
     * `isForeground=true` with the notification held, and it was absent from
     * `dumpsys notification`'s list entirely.
     */
    @Synchronized
    fun refreshHosting() {
        if (snapshot().busy) HostingService.start(appContext)
    }

    /**
     * Stop a server, as the UI's button and the notification's action both do.
     *
     * Returns null on success, or a sentence for a player. Here rather than in
     * the bridge because the notification needs the identical sequence, and the
     * part that must not be re-derived is `graceful`: it is the core's verdict
     * about whether the engine has a console that can hear `stop` and a world
     * to save, not a preference. Guessing it wrong terminates a JVM mid-save.
     */
    suspend fun stop(serverId: String): String? {
        // A stopping server is still this device's. The dashboard PATCHes
        // `stopped` only after this returns, so for the whole graceful shutdown
        // the API still reads `running` — and a host that reports itself idle in
        // that window gets the server it just stopped restarted underneath it.
        //
        // `abandonLaunch` needs no branch of its own: the intent is recorded
        // either way, and a launch with nothing spawned yet gives up at its next
        // checkpoint.
        val verdict = lifecycle.stopRequested(serverId).verdict
        return try {
            if (verdict == "notRunning") {
                "That server is not running."
            } else {
                try {
                    backend.stop(serverId, graceful = verdict == "graceful")
                    null
                } catch (err: ServerBackendException) {
                    err.message
                }
            }
        } finally {
            lifecycle.callFinished(serverId)
        }
    }

    // -----------------------------------------------------------------------
    // Not dying of a background failure
    // -----------------------------------------------------------------------

    /**
     * The exception handler every scope in this app that outlives a screen has
     * to carry.
     *
     * `SupervisorJob` answers a narrower question than its name suggests: it
     * stops a failed child cancelling its *siblings*, and does nothing at all
     * about the failure itself, which carries on to the thread's default handler
     * and takes the process with it. That is the trap [WireProxy]'s log pump
     * fell into once already, and a `try`/`catch` there only ever covers the one
     * launch site somebody remembered to write it in.
     *
     * On this app a process death is not a restart. The Minecraft server is a
     * child of this process, and the world upload runs for minutes after it
     * stops — so a `CoreException` from a malformed native reply, the kind
     * [JavaServerBackend]'s exit callback can raise, would end a session and
     * lose the backup carrying it (see [HostingService]).
     *
     * **Nothing here is a silent no-op.** Everything that arrives is logged at
     * ERROR with its stack, which is what `get-app-logs` hands to support, and
     * named by [doing] — because the scope survives and the failed coroutine
     * does not, and a heartbeat or a reporting loop that has quietly stopped is
     * otherwise indistinguishable from a healthy one. A swallowed failure
     * nobody can see is worse than the crash it replaced.
     *
     * A [VirtualMachineError] is *not* kept alive. That is the runtime saying it
     * can no longer run this code, and a process carrying on past one cannot
     * finish a backup either — it can only fail to, quietly. Re-throwing the
     * **same instance** is deliberate: kotlinx.coroutines passes a handler's own
     * throwable straight through when it is the one it just handed in, so the
     * default handler gets the original stack rather than a wrapper around it.
     *
     * A cancellation never reaches here — the machinery treats it as ordinary
     * completion — so there is nothing to filter out.
     */
    fun keepAlive(tag: String, doing: String): CoroutineExceptionHandler =
        CoroutineExceptionHandler { _, err ->
            if (err is VirtualMachineError) throw err
            Log.e(tag, "$doing failed; this process is carrying on without it", err)
            noteHostFailure(err)
        }

    /**
     * Tell the player, once per run, that Homerun itself stumbled.
     *
     * The console is the only surface this host has for a failure that is not a
     * server's own: `Core.crashReport` reads a server's output and would file a
     * host bug as a Minecraft crash. The line is worth more than it looks —
     * [Reporting] keeps every console line in the tail it sends with a crash
     * report, so a host failure that preceded a crash travels with it to
     * support without anything new being built to carry it.
     *
     * Once per run, because whatever failed usually fails on a timer: a report
     * throwing every 30 s would bury the console a player reads to find out
     * what went wrong.
     */
    private fun noteHostFailure(err: Throwable) {
        val serverId = synchronized(this) {
            snapshot().takeIf { it.busy && !failureNoted }?.serverId?.also { failureNoted = true }
        } ?: return
        note(
            serverId,
            "Homerun hit an unexpected problem in the background " +
                "(${err.javaClass.simpleName}) and has carried on. If this server " +
                "misbehaves from here, stopping it and starting it again is the fix.",
        )
    }

    private const val TAG = "HomerunHost"
}

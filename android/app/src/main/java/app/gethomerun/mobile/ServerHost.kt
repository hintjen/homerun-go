package app.gethomerun.mobile

import android.content.Context
import android.util.Log
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
     */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

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

    @Synchronized
    private fun snapshot(): Hosting =
        Hosting(hostingId, hostingName, hostingState, hostingBackup, null, hostingStarting)

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

    private const val TAG = "HomerunHost"
}

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

    interface Listener {
        fun onLog(serverId: String, line: String) {}
        fun onStateChanged(serverId: String, state: ServerState, backupInProgress: Boolean = false) {}
        fun onPlayersChanged(serverId: String) {}
        fun onNetworkError(serverId: String, kind: String) {}
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
            // M4 hooks a foreground service in here, so that a running
            // server survives the app being backgrounded.
            onStateChanged = { id, state, backingUp ->
                forEach { it.onStateChanged(id, state, backingUp) }
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

    private const val TAG = "HomerunHost"
}

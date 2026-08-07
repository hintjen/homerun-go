package app.gethomerun.mobile

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/**
 * This device, as the backend knows it.
 *
 * # What "installation" means here
 *
 * The desktop has an installation wizard with nine steps — pick a path, check
 * disk space, unpack a JRE, unpack wireproxy, register the device, verify
 * Java, launch a supervisor. On Android almost all of that is already true
 * before the app first runs: the JRE and wireproxy ship inside the APK, and
 * the install root is app-private storage that always exists.
 *
 * What is left is the one step that was never about the local machine —
 * **registering with the backend** — so there is no wizard here, no progress
 * UI, and `installation` stays false in the capability profile. Importing the
 * concept would mean importing a user-visible flow the platform does not need.
 *
 * # Why this is load-bearing
 *
 * A native server is hosted *on a device*, and the API binds it with
 * `config.current_device`. Without a registered device:
 *
 *  - server creation is rejected — the id does not exist
 *  - instance reports are discarded, because they are filtered on
 *    `current_device`, so the service is never marked healthy and the UI sits
 *    on "Starting up" forever
 *  - no gateway link is provisioned, so nothing can be joined
 *
 * `POST /api/init/native/` does the whole job in one call: it creates the
 * device, adds it to the user's default group and that group's gateway
 * service, joins it to servers the user already has, and issues a device
 * token. Everything about *who* owns it is derived server-side from the user
 * JWT — the client sends only a name.
 */
object DeviceRegistry {

    /**
     * The API's id for this device, and the token it authenticates reports
     * with. Both are assigned by the backend.
     */
    data class Registration(
        val deviceId: String,
        val deviceToken: String,
        val groupId: String?,
    )

    private lateinit var prefs: SharedPreferences

    /**
     * Not tied to any lifecycle. The heartbeat has to outlive every activity —
     * a device that stops reporting is marked unhealthy 60 s later, whatever
     * the UI happens to be doing.
     */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /** Serialises registration so a burst of callers makes one API call. */
    private val gate = Mutex()

    private var heartbeat: Job? = null

    /** Server ids currently hosted here, for the instance report. */
    @Volatile
    private var runningIds: () -> List<String> = { emptyList() }

    fun init(context: Context, runningIds: () -> List<String>) {
        prefs = context.applicationContext
            .getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        this.runningIds = runningIds
        // A device token outlives the process, so a relaunch resumes reporting
        // without waiting for the user to log in again.
        if (current() != null) startHeartbeat()
    }

    /** What is stored, or null if this device has never registered. */
    fun current(): Registration? {
        val id = prefs.getString(KEY_ID, null) ?: return null
        val token = prefs.getString(KEY_TOKEN, null) ?: return null
        return Registration(id, token, prefs.getString(KEY_GROUP, null))
    }

    /**
     * Register if not already registered, and return the result.
     *
     * Safe to call on every login and again whenever the device id is asked
     * for — the desktop treats a missing registration as self-healing rather
     * than fatal, and so does this.
     */
    suspend fun ensure(apiUrl: String, userToken: String): Registration? {
        current()?.let { return it }
        if (userToken.isBlank()) {
            Log.i(TAG, "not registering: no user token yet")
            return null
        }
        return gate.withLock {
            // Another caller may have won the race while we waited.
            current() ?: register(apiUrl, userToken)
        }
    }

    private suspend fun register(apiUrl: String, userToken: String): Registration? {
        // Only re-sent when the backend gave it to us. The pre-registration
        // builds of this app minted a random UUID locally and stored it under
        // a different key; sending one of those would 404, because the API
        // looks it up as a primary key it owns.
        val existing = prefs.getString(KEY_ID, null)

        Log.i(TAG, "registering as \"${deviceName()}\"${if (existing != null) " (re-using $existing)" else ""}")
        val result = HomerunApi.registerDevice(apiUrl, userToken, deviceName(), existing)
        if (result == null) {
            Log.w(TAG, "registration failed — will retry on the next attempt")
            return null
        }

        prefs.edit()
            .putString(KEY_ID, result.deviceId)
            .putString(KEY_TOKEN, result.deviceToken)
            .putString(KEY_GROUP, result.groupId)
            .apply()
        Log.i(TAG, "registered as ${result.deviceId}")

        startHeartbeat()
        return result
    }

    /**
     * The name the backend shows for this device.
     *
     * Sanitised to the desktop's rules (`sanitizeDeviceName`), because the API
     * de-duplicates on the name when no existing id is supplied and a
     * differently-mangled name would create a second device row for the same
     * phone.
     */
    private fun deviceName(): String {
        val raw = listOf(Build.MANUFACTURER, Build.MODEL)
            .filter { !it.isNullOrBlank() }
            .distinctBy { it.lowercase() }
            .joinToString("-")
            .ifBlank { "homerun-device" }

        return raw
            .replace(Regex("[^a-zA-Z0-9_.-]"), "-")
            .replace(Regex("-+"), "-")
            .replace(Regex("^[^a-zA-Z0-9]+"), "")
            .ifBlank { "homerun-device" }
    }

    // -----------------------------------------------------------------------
    // Reporting
    // -----------------------------------------------------------------------

    /**
     * Tell the backend this device is alive, and what it is hosting.
     *
     * Health is derived from the age of the most recent report against a 60 s
     * threshold, so this runs every 30 s — one missed beat of slack — and
     * **from app start, not only while a server runs**. An empty instance list
     * is still a heartbeat; it is what keeps the device itself online.
     *
     * The mobile caveat with no desktop equivalent: this stops when the
     * process does, so a backgrounded app reads unhealthy about a minute
     * later even though its server is fine. That is the same gap the
     * foreground service closes, and the two belong together.
     */
    private fun startHeartbeat() {
        if (heartbeat?.isActive == true) return
        heartbeat = scope.launch {
            while (isActive) {
                val registration = current()
                if (registration != null) {
                    HomerunApi.reportInstances(
                        apiUrl = apiUrl(),
                        deviceId = registration.deviceId,
                        deviceToken = registration.deviceToken,
                        instances = runCatching { runningIds() }.getOrDefault(emptyList()),
                    )
                }
                delay(HEARTBEAT_INTERVAL_MS)
            }
        }
    }

    /**
     * Acknowledge a server's state with the **device** token.
     *
     * Distinct from the `native-server-state-changed` bridge event, which only
     * tells the page in front of us. This is what the API waits on before it
     * considers a server actually up, and what the web dashboard reads.
     *
     * **Two reports, not one.** The state POST records what happened; the
     * instance report is what the API derives *health* from, and health is
     * what the UI shows. Leaving the instance report to the next 30 s tick
     * meant a server that was genuinely up — JVM booted, tunnel handshaken —
     * kept reading as not-running for up to another half minute, nine seconds
     * of it in the run that found this. The desktop pushes both together in
     * `onServerFullyRunning`, in this order, for exactly this reason.
     */
    fun reportServerState(serverId: String, state: ServerState) {
        val wire = when (state) {
            ServerState.RUNNING -> "running"
            ServerState.STOPPED, ServerState.CRASHED -> "stopped"
            // `starting`/`stopping` are ours; the API models neither.
            else -> return
        }
        val registration = current() ?: return
        scope.launch {
            // Read the running set now rather than capturing it: the backend
            // has already applied this transition by the time it calls us, so
            // this is the post-change truth in both directions.
            HomerunApi.reportInstances(
                apiUrl = apiUrl(),
                deviceId = registration.deviceId,
                deviceToken = registration.deviceToken,
                instances = runCatching { runningIds() }.getOrDefault(emptyList()),
            )
            HomerunApi.reportServerState(apiUrl(), serverId, wire, registration.deviceToken)
        }
    }

    /**
     * The API base. Read from the same prefs the bridge writes, because the UI
     * pushes its own `apiUrl` down at boot and that is the one that wins.
     */
    private fun apiUrl(): String =
        prefs.getString(KEY_API_URL, null) ?: BuildConfig.API_URL

    /** Forget this device. Called on logout, so the next user registers their own. */
    fun clear() {
        heartbeat?.cancel()
        heartbeat = null
        prefs.edit().remove(KEY_ID).remove(KEY_TOKEN).remove(KEY_GROUP).apply()
    }

    private const val TAG = "HomerunDevice"

    /**
     * The bridge's own preferences file, so `api-url` is the same value the
     * rest of the host uses.
     */
    private const val PREFS = "homerun-host"

    // Deliberately NOT the legacy `device-id` key, which held a locally
    // generated UUID the backend never issued.
    private const val KEY_ID = "native-device-id"
    private const val KEY_TOKEN = "native-device-token"
    private const val KEY_GROUP = "native-device-group"
    private const val KEY_API_URL = "api-url"

    private const val HEARTBEAT_INTERVAL_MS = 30_000L
}

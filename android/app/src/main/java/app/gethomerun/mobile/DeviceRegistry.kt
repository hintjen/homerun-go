package app.gethomerun.mobile

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import android.util.Log
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive

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
     *
     * What the handler ([ServerHost.keepAlive]) keeps alive is the process the
     * server is a child of. Everything launched here is fire-and-forget
     * reporting, so a throw costs one beat — the device reads unhealthy for
     * 30 s and the next call corrects it — where letting it reach the default
     * handler costs the running server and the backup behind it.
     *
     * The heartbeat loop itself does *not* survive its own throw: the loop is
     * the failed coroutine. The ERROR line is the only thing that distinguishes
     * a device that stopped reporting from one that never had anything to say.
     */
    private val scope = CoroutineScope(
        SupervisorJob() + Dispatchers.IO + ServerHost.keepAlive(TAG, "device reporting"),
    )

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
    /**
     * This device's id, or null if it has not registered.
     *
     * The identity restic records as a snapshot's hostname, and the one the
     * API resolves `pushed_by` from — so it must be the id the API issued,
     * never a locally-generated one, or every device would think every
     * snapshot was someone else's.
     */
    fun currentDeviceId(): String? = current()?.deviceId

    fun current(): Registration? {
        val id = prefs.getString(KEY_ID, null) ?: return null
        // A token that will not decrypt reads as absent, which lands here as
        // "not registered" — and [register] re-sends the id above, so the
        // recovery is a new token for the same device row rather than a
        // second device appearing in the dashboard.
        val token = SecretStore.read(prefs, KEY_TOKEN) ?: return null
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
        current()?.let { existing ->
            // A device row belongs to exactly one account. Returning any
            // existing registration regardless of who is signed in is what
            // left this phone registered to a guest it had already left:
            // every later request naming the device was then refused as
            // somebody else's hardware — the push token upsert with "Not one
            // of your devices", and the migration that exists to rescue that
            // guest's servers with the same.
            val account = currentAccount()
            val registeredTo = prefs.getString(KEY_ACCOUNT, null)

            if (account == null) return existing

            if (registeredTo == null) {
                // Registered before this marker existed. Adopt the current
                // account rather than re-registering: on upgrade that would
                // mint a second device row for every install at once. If the
                // guess is wrong it corrects itself at the next real change.
                prefs.edit().putString(KEY_ACCOUNT, account).apply()
                return existing
            }

            if (registeredTo == account) {
                // Same account, same device — but re-assert it **once per
                // launch** rather than trusting the stored row.
                //
                // `/api/init/native/` is idempotent: it matches this device by
                // name and returns the id it already issued. What it also does
                // is add that device to every server the user is a member of,
                // and that loop is the only thing that repairs an ordering
                // failure nothing else can reach.
                //
                // The failure it repairs: a guest upgrading to an account
                // re-registers here, and at that instant the guest's servers
                // have not been migrated yet — so the user is not a member of
                // them and the loop adds nothing. The migration then runs and
                // grants membership to whichever device it was told about. Get
                // that wrong once and the phone is a member of none of its own
                // servers, with every launch refused as somebody else's
                // hardware and no way back that does not delete the worlds.
                //
                // Once per launch, so this costs one request at startup and
                // nothing afterwards.
                if (reasserted.compareAndSet(false, true) && userToken.isNotBlank()) {
                    scope.launch {
                        runCatching { gate.withLock { register(apiUrl, userToken) } }
                            .onFailure { Log.w(TAG, "could not re-assert this device: ${it.message}") }
                    }
                }
                return existing
            }

            Log.i(TAG, "signed in as a different account; re-registering this device")
            clear()
        }
        if (userToken.isBlank()) {
            Log.i(TAG, "not registering: no user token yet")
            return null
        }
        return gate.withLock {
            // Another caller may have won the race while we waited.
            current() ?: register(apiUrl, userToken)
        }
    }

    /**
     * Whether this launch has already re-asserted an existing registration.
     *
     * Process-lifetime only and deliberately not persisted: the point is one
     * request per app start, and a stored flag would turn "once per launch"
     * into "once, ever" and take the self-heal with it.
     */
    private val reasserted = AtomicBoolean(false)

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
            .putString(KEY_GROUP, result.groupId)
            .putString(KEY_ACCOUNT, currentAccount())
            .apply()
        SecretStore.write(prefs, KEY_TOKEN, result.deviceToken)
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
    fun reportServerState(serverId: String, state: ServerState, backupInProgress: Boolean = false) {
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
            HomerunApi.reportServerState(
                apiUrl(), serverId, wire, registration.deviceToken, backupInProgress,
            )
        }
    }

    /**
     * Report a backup or restore outcome, releasing the lease if it was a
     * backup. Needs the device token, which only this object holds.
     */
    fun reportBackupState(serverId: String, body: kotlinx.serialization.json.JsonObject) {
        val registration = current() ?: return
        scope.launch {
            HomerunApi.reportBackupState(apiUrl(), serverId, body, registration.deviceToken)
        }
    }

    /**
     * The API base. Read from the same prefs the bridge writes, because the UI
     * pushes its own `apiUrl` down at boot and that is the one that wins.
     */
    fun apiUrl(): String =
        prefs.getString(KEY_API_URL, null) ?: BuildConfig.API_URL

    /** Forget this device. Called on logout, so the next user registers their own. */
    fun clear() {
        heartbeat?.cancel()
        heartbeat = null
        prefs.edit()
            .remove(KEY_ID).remove(KEY_TOKEN).remove(KEY_GROUP).remove(KEY_ACCOUNT)
            .apply()
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

    /** Which account this device is registered to. See [ensure]. */
    private const val KEY_ACCOUNT = "native-device-account"

    /**
     * The signed-in account, as the matrix id the UI handed over with the
     * credentials.
     *
     * The matrix id rather than the address: claiming a guest account rotates
     * the address while staying the same account and keeping the same device,
     * so keying on email would re-register for no reason every time somebody
     * signed up.
     */
    private fun currentAccount(): String? = runCatching {
        val stored = SecretStore.read(prefs, KEY_CREDENTIALS) ?: return@runCatching null
        (Json.parseToJsonElement(stored) as? JsonObject)
            ?.get("matrix_id")?.jsonPrimitive?.contentOrNull
    }.getOrNull()

    /** Written by BridgeRouter; the same key, in the same preferences file. */
    private const val KEY_CREDENTIALS = "credentials"

    private const val HEARTBEAT_INTERVAL_MS = 30_000L
}

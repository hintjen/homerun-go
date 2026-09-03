package app.gethomerun.mobile

import android.app.Application
import android.util.Log
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import java.util.UUID

/**
 * Everything this app failed at, on its way to the API.
 *
 * # What was here before
 *
 * Nothing. A Kotlin crash left a logcat tombstone nobody would ever read; a
 * panic in the native core wrote a file nothing opened; a throw inside the
 * WebView was a blank screen. The shared bundle carries Sentry, but Sentry
 * there is a *renderer* integration — it can see the page's errors and cannot
 * see a Kotlin stack, a Swift stack or a Rust panic at all, and it does not
 * know which over-the-air bundle was running.
 *
 * # What this object does and does not decide
 *
 * It decides nothing. Whether two failures are the same bug, whether this one
 * is worth sending again, what has to be redacted before it leaves the device
 * and what the body looks like are all answered in
 * `homerun-core::reporting::app_error`, so that this host and the iOS one
 * cannot drift on any of them. What lives here is what only a platform knows:
 * the clock, the app's own version, the bundle it is running, and the
 * credential to sign with.
 *
 * # Two paths, because a dying thread cannot finish a request
 *
 * [report] is the ordinary one: the core decides, and a request goes out on
 * [ServerHost.scope] if there is one to send.
 *
 * [stash] is for the crash handler. `Thread.setDefaultUncaughtExceptionHandler`
 * runs on the thread that is about to be killed and hands straight off to
 * `KillApplicationHandler`; a coroutine launched there never resumes and an
 * HTTP request never completes. So the crash writes a file, synchronously,
 * and [drain] sends it on the next launch.
 *
 * # It must never report itself
 *
 * Every failure on this path is logged and dropped. A reporter that reports
 * its own failures is a reporter that turns one bad response into an infinite
 * loop, and it does it fastest exactly when the API is already struggling.
 */
object AppErrors {

    /**
     * One per process. It is what makes "this person hit forty errors in one
     * sitting" a question the API can answer — without it, forty rows from
     * one bad afternoon look like forty unrelated reports.
     */
    private val session: String = UUID.randomUUID().toString()

    private var started = false

    /**
     * Point the core's crash directory at this app's storage and take the
     * session id.
     *
     * Called before anything else in [HomerunApplication], because the window
     * it protects starts at the first line of `onCreate`.
     */
    fun init(app: Application) {
        if (started) return
        started = true
        runCatching { Core.appErrorAttach(app.filesDir.absolutePath) }
            .onFailure { Log.w(TAG, "could not attach the crash directory: ${it.message}") }
    }

    /**
     * Send whatever the last launch left behind.
     *
     * Called at the end of `onCreate`, once [DeviceRegistry] can supply a
     * credential — the reports themselves were written long before that and
     * do not care, but the request that carries them does.
     */
    fun drain() {
        ServerHost.scope.launch {
            val requests = runCatching { Core.appErrorDrain(context()) }
                .onFailure { Log.w(TAG, "could not drain crash reports: ${it.message}") }
                .getOrNull()
                .orEmpty()

            if (requests.isNotEmpty()) {
                Log.i(TAG, "sending ${requests.size} report(s) from the last run")
            }
            requests.forEach { send(it) }
        }
    }

    /**
     * Report one failure.
     *
     * The core very often decides not to send, and that is the design working
     * rather than something to log about.
     */
    fun report(occurrence: JsonObject) {
        ServerHost.scope.launch {
            val request = runCatching { Core.appErrorReport(context(), occurrence) }
                .onFailure { Log.w(TAG, "could not build an error report: ${it.message}") }
                .getOrNull()
                ?: return@launch
            send(request)
        }
    }

    /**
     * Report a failure the page described.
     *
     * The payload is the page's, but `source` is not: a bundle is replaced
     * over the air and is the least trusted thing in the process, so it does
     * not get to file a report as a native crash or a host crash. Anything
     * that is not `api` is recorded as `ui`, which is what it is.
     *
     * `atMs` is filled in when the page omits it. Zero would be the epoch, and
     * a report dated 1970 sorts to the bottom of every view that matters.
     */
    fun reportFromPage(occurrence: JsonObject) {
        val claimed = occurrence["source"]?.jsonPrimitive?.contentOrNull
        val at = occurrence["atMs"]?.jsonPrimitive?.longOrNull
        report(buildJsonObject {
            occurrence.forEach { (key, value) ->
                if (key != "source" && key != "atMs") put(key, value)
            }
            put("source", if (claimed == SOURCE_API) SOURCE_API else SOURCE_UI)
            put("atMs", if (at != null && at > 0) at else System.currentTimeMillis())
        })
    }

    /** Report a Kotlin throwable. */
    fun report(
        throwable: Throwable,
        severity: String = SEVERITY_ERROR,
        location: String? = null,
    ) = report(occurrenceOf(throwable, severity, location))

    /**
     * Write one failure to disk without sending it.
     *
     * Synchronous on purpose — see the object header. The caller is already
     * dying, so this does the least it can: one JNI call that writes one file.
     */
    fun stash(throwable: Throwable, location: String? = null) {
        runCatching {
            Core.appErrorStash(context(), occurrenceOf(throwable, SEVERITY_FATAL, location))
        }.onFailure {
            // Nowhere left to escalate to. The process is going.
            Log.w(TAG, "could not stash the crash: ${it.message}")
        }
    }

    /**
     * What this install is, as far as anything can tell right now.
     *
     * Deliberately forgiving. Every reader below can fail — the registry may
     * not be initialised yet, storage may be locked before first unlock — and
     * the report that arrives during exactly that window is the one worth
     * most. A partial context beats no report.
     */
    /**
     * Not private: [DeviceWebsocket] hands this to the native core at startup,
     * so a certificate failure inside the socket can be reported as an app
     * error instead of vanishing into logcat. The core holds it for the life
     * of the socket, which is why it must be cheap to produce and forgiving of
     * everything being absent.
     */
    fun context(): JsonObject = buildJsonObject {
        put("deviceId", runCatching { DeviceRegistry.current()?.deviceId }.getOrNull().orEmpty())
        put("session", session)
        put("platform", "android")
        put("appVersion", BuildConfig.VERSION_NAME)
        put("bundle", runCatching { BundleStore.active() }.getOrNull())
        put("hostRevision", BridgeRouter.HOST_REVISION)
        // The launch this error happened during, if one was in progress. It is
        // what lets the API put a Rust panic or a Kotlin exception beside the
        // crash report of the server it took down — without it the two are
        // rows in different tables with nothing but a timestamp in common.
        runCatching { ServerHost.hostedServerId() }.getOrNull()?.let { put("serverId", it) }
        // The core reads this to decide production from staging. It is never
        // sent verbatim, and deriving the deployment from it in one place is
        // what stops three platforms disagreeing about which one they are on.
        put("apiUrl", runCatching { DeviceRegistry.apiUrl() }.getOrNull() ?: BuildConfig.API_URL)
    }

    private fun occurrenceOf(
        throwable: Throwable,
        severity: String,
        location: String?,
    ): JsonObject = buildJsonObject {
        put("source", SOURCE_HOST)
        put("severity", severity)
        // The class name, not the message: it is the stable half, and the
        // core's fingerprint keys on it.
        put("kind", throwable.javaClass.name)
        put("message", throwable.message ?: throwable.javaClass.simpleName)
        put("stack", throwable.stackTraceToString())
        location?.let { put("location", it) }
        put("atMs", System.currentTimeMillis())
    }

    /**
     * Sign it if this device has a credential, send it unsigned if it does
     * not.
     *
     * Unsigned is not a fallback that lost something — it is the case this
     * whole path exists for. A crash before registration, or on the login
     * screen, has no token by definition, and those are the failures nobody
     * can reproduce from a bug report.
     */
    private suspend fun send(request: Core.Request) {
        val token = runCatching { DeviceRegistry.current()?.deviceToken }.getOrNull()
        val api = runCatching { DeviceRegistry.apiUrl() }.getOrNull() ?: BuildConfig.API_URL
        HomerunApi.performAppError(api, request, token)
    }

    const val SOURCE_HOST = "host"
    const val SOURCE_UI = "ui"
    const val SOURCE_API = "api"

    /**
     * Below the host language: a Rust panic, or a process death the system
     * reported to us afterwards because nothing of ours was alive to report
     * it. See [ExitReasons].
     */
    const val SOURCE_NATIVE = "native"
    const val SEVERITY_FATAL = "fatal"
    const val SEVERITY_ERROR = "error"

    private const val TAG = "HomerunAppErrors"
}

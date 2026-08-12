package app.gethomerun.mobile

import android.app.ActivityManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.JavascriptInterface
import android.webkit.WebView
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.add
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.util.Locale
import java.util.concurrent.atomic.AtomicReference

/** A channel implementation. `params` is the raw payload; the return value is the result. */
private typealias ChannelHandler = suspend (JsonElement?) -> JsonElement?

private const val PROTOCOL_VERSION = 1

/**
 * The `bridge/v1` wire envelope, both directions (PROTOCOL.md §2). Absent
 * fields are omitted rather than sent as null, because the UI distinguishes
 * "no error key" from "error: null".
 */
@Serializable
private data class Envelope(
    val v: Int = PROTOCOL_VERSION,
    val id: String? = null,
    val method: String? = null,
    val params: JsonElement? = null,
    val result: JsonElement? = null,
    val error: BridgeError? = null,
    val event: String? = null,
    val args: List<JsonElement>? = null,
)

@Serializable
private data class BridgeError(val message: String, val code: String? = null)

/**
 * The Android half of `bridge/v1` — see `shared/conformance/PROTOCOL.md`.
 *
 * Threading is the thing to get right here. `postMessage` is reached through
 * `addJavascriptInterface`, which means **it runs on a binder thread**, not the
 * main thread and not a thread we own. So:
 *
 *  - Parsing happens on the binder thread (cheap, no shared state).
 *  - All router bookkeeping — the ready flag, the event queue — is confined to
 *    the main thread, which is also the only thread allowed to touch the
 *    WebView.
 *  - Actual channel work runs on [scope], so a slow handler can never block
 *    the binder thread. Blocking it ANRs the app in ways that look like
 *    WebView bugs.
 *
 * There is deliberately **no call timeout**: `native-server-start` and modpack
 * imports legitimately run for minutes. Pending work is cleared when the page
 * goes away ([onPageGone]), not on a clock.
 */
class BridgeRouter(
    private val context: Context,
    private val scope: CoroutineScope,
) {
    private val main = Handler(Looper.getMainLooper())

    /**
     * Set by the activity, and re-set when the render process dies and the
     * WebView is rebuilt. The router outlives any single WebView, which is
     * the point — pending state belongs to the page, not to the transport.
     */
    private var webView: WebView? = null

    fun attach(view: WebView) {
        webView = view
    }

    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
        explicitNulls = false
    }

    private val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /**
     * The local server engine, owned by the process rather than by this
     * router — a page reload must not take a running server with it.
     */
    private val backend: ServerBackend get() = ServerHost.backend

    /**
     * Who owns a server right now: running, coming up, or winding down.
     *
     * Every judgement here is `homerun-core::lifecycle`'s — this router only
     * reports what it sees. The window it closes opens when the *call*
     * arrives, not when the backend starts work: the settings lookup and the
     * backup-lease check in the start handler are both network round-trips,
     * and a server not yet counted active is one the UI's reconcile loop will
     * try to start for itself, reprovisioning the gateway underneath a launch
     * that has already resolved its tunnel config.
     */
    private val lifecycle: Core.Lifecycle get() = ServerHost.lifecycle

    /**
     * Console output and state changes reach the UI only through these.
     * Registered for the life of the router; `ServerHost` fans them out so the
     * bridge and (from M4) the foreground service can both listen.
     */
    private val hostListener = object : ServerHost.Listener {
        override fun onLog(serverId: String, line: String) {
            emit("native-server-log", listOf(buildJsonObject {
                put("serverId", serverId)
                put("line", line)
            }))
        }

        override fun onPlayersChanged(serverId: String) {
            emit("native-server-players-changed", listOf(buildJsonObject {
                put("serverId", serverId)
            }))
        }

        /**
         * A tunnel failure stops the server through the normal clean path, so
         * without this the UI shows it flipping to stopped and cannot tell it
         * from the user's own Stop. The shared UI already listens and toasts,
         * worded per kind — this is the half that was missing.
         */
        override fun onNetworkError(serverId: String, kind: String) {
            emit("native-server-network-error", listOf(buildJsonObject {
                put("serverId", serverId)
                put("kind", kind)
            }))
        }

        override fun onStateChanged(
            serverId: String,
            state: ServerState,
            backupInProgress: Boolean,
        ) {
            // Two different audiences. This one is the API's — it is what
            // marks the service healthy, and the web dashboard reads it. The
            // bridge event below only reaches the page in front of us.
            DeviceRegistry.reportServerState(serverId, state, backupInProgress)

            // The event contract carries only these three. `starting` and
            // `stopping` are ours; the UI infers those from the pending call.
            val wire = when (state) {
                ServerState.RUNNING -> "running"
                ServerState.STOPPED -> "stopped"
                ServerState.CRASHED -> "crashed"
                else -> null
            } ?: return
            emit("native-server-state-changed", listOf(buildJsonObject {
                put("serverId", serverId)
                put("state", wire)
            }))
            if (state == ServerState.RUNNING) {
                backend.port(serverId)?.let { p ->
                    emit("native-server-port", listOf(buildJsonObject {
                        put("serverId", serverId)
                        put("port", p)
                    }))
                }
            }
        }
    }

    init {
        ServerHost.addListener(hostListener)

        // A relaunch already holds the token issued at login, so the device's
        // link belongs to the process rather than to the login — the same
        // reasoning that starts the heartbeat in `HomerunApplication`. Waiting
        // for `set-credentials` would mean a device is only reachable in the
        // session it logged in on. `ensure` is idempotent, so a router rebuilt
        // for a new activity does not start a second one.
        DeviceWebsocket.ensure(apiUrl(), userToken())
    }

    /** The `native-server-*` channels that take an object payload. */
    private fun JsonObject.serverId(): String =
        this["serverId"]?.jsonPrimitive?.contentOrNull
            ?: throw IllegalArgumentException("serverId is required")

    /** The six metrics getters and the log reader take a bare string instead. */
    private fun JsonElement?.asServerId(): String =
        this?.jsonPrimitive?.contentOrNull
            ?: throw IllegalArgumentException("serverId is required")

    // --- main-thread-only state ------------------------------------------

    /** False until the page announces `__bridge:ready`; events queue meanwhile. */
    private var ready = false

    /** Serialised event envelopes awaiting the handshake, in emission order. */
    private val queued = ArrayDeque<String>()

    /**
     * Parent of every in-flight handler for the *current* page. Cancelling it
     * is how a page teardown abandons work whose reply has nowhere to go.
     */
    private var pageJobs = Job()

    // ---------------------------------------------------------------------
    // UI -> host
    // ---------------------------------------------------------------------

    /** Called from JavaScript as `HomerunHost.postMessage(json)`. Binder thread. */
    @JavascriptInterface
    fun postMessage(payload: String) {
        val envelope = try {
            json.decodeFromString<Envelope>(payload)
        } catch (err: Exception) {
            Log.e(TAG, "unparseable envelope from the page: ${err.message}")
            return
        }
        main.post { dispatch(envelope) }
    }

    private fun dispatch(envelope: Envelope) {
        val method = envelope.method ?: return

        if (method == READY_METHOD) {
            onReady()
            return
        }

        val handler = handlers[method]
        if (handler == null) {
            // Answering with an error is not optional. An invoke with no reply
            // leaves a promise pending forever and the UI hangs with no clue
            // why (PROTOCOL.md §5).
            val message = "Channel \"$method\" is not implemented by the Android host yet"
            if (envelope.id != null) reply(Envelope(id = envelope.id, error = BridgeError(message)))
            else Log.w(TAG, message)
            return
        }

        scope.launch(pageJobs) {
            try {
                val result = handler(envelope.params)
                if (envelope.id != null) {
                    reply(Envelope(id = envelope.id, result = result ?: JsonNull))
                }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (err: Throwable) {
                Log.e(TAG, "handler for \"$method\" failed", err)
                if (envelope.id != null) {
                    reply(
                        Envelope(
                            id = envelope.id,
                            error = BridgeError(err.message ?: err.javaClass.simpleName),
                        )
                    )
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // host -> UI
    // ---------------------------------------------------------------------

    /**
     * Emit an event to the page. Safe from any thread. Before the handshake
     * the event is queued rather than dropped, because the UI wires its
     * subscriptions after the first paint and would otherwise miss anything
     * emitted during startup.
     */
    fun emit(event: String, args: List<JsonElement> = emptyList()) {
        val literal = literal(Envelope(event = event, args = args))
        main.post {
            if (ready) evaluate(literal) else queued.addLast(literal)
        }
    }

    private fun onReady() {
        ready = true
        // The handshake is also the health signal for an over-the-air bundle:
        // one that throws on its first chunk never gets here, and one that does
        // has proved it can run. Nothing else in the protocol says that.
        BundleStore.confirm(context)
        while (queued.isNotEmpty()) evaluate(queued.removeFirst())
        resyncServerState()
    }

    /**
     * Tell a page what is already running.
     *
     * The server outlives the page, so a reload — or an activity rebuilt after
     * the render process died — starts with no idea a server is up. Without
     * this it renders a stopped server that is very much running.
     *
     * Also called on resume. The WebView usually survives being backgrounded
     * and receives events normally, but "usually" is doing real work in that
     * sentence now that a server keeps running while the app is away: the
     * render process is exactly what Android reclaims first, and a player
     * coming back to a stopped card for a server their friends are on is the
     * failure this costs one event to rule out.
     */
    fun resyncServerState() {
        val serverId = ServerHost.runningServerId() ?: return
        hostListener.onStateChanged(serverId, ServerState.RUNNING, false)
    }

    private fun reply(envelope: Envelope) {
        val literal = literal(envelope)
        main.post { evaluate(literal) }
    }

    private fun evaluate(literal: String) {
        val view = webView
        if (view == null) {
            Log.w(TAG, "dropped a message: no WebView attached")
            return
        }
        try {
            view.evaluateJavascript(
                "window.__homerunHost && window.__homerunHost.receive($literal);",
                null,
            )
        } catch (err: Exception) {
            // The WebView can be torn down between the post and the delivery.
            Log.w(TAG, "dropped a message: ${err.message}")
        }
    }

    /**
     * One JSON literal from the serializer — never string interpolation of
     * values, which is how a server name containing a quote becomes a syntax
     * error in the middle of `evaluateJavascript`.
     */
    private fun literal(envelope: Envelope): String = escapeForJs(json.encodeToString(envelope))

    // ---------------------------------------------------------------------
    // Deep links
    // ---------------------------------------------------------------------

    /**
     * A `homerun://` link that arrived before any page could hear about it.
     *
     * Atomic rather than main-thread-confined because `deep-link:consume` is
     * answered from a coroutine like every other handler, and routing that one
     * read back through the main thread would buy nothing.
     *
     * Deliberately **not** cleared by [onPageGone]: a link that arrived just
     * before the render process died still deserves delivery to its
     * replacement.
     */
    private val pendingDeepLink = AtomicReference<String?>(null)

    /**
     * A link that arrived with the activity itself — the app was not running.
     *
     * Held for the pull path rather than emitted, because at this point no
     * page exists, and the event queue would flush at the `ready` handshake,
     * which happens *before* the UI's deep-link subscription is wired. An
     * event delivered then is dropped on the floor. This is the reason
     * `bridge/v1` has both a pull and a push path for the same link.
     */
    fun captureColdStartDeepLink(url: String) {
        pendingDeepLink.set(url)
    }

    /**
     * A link that arrived while the app was already running. The UI is mounted
     * and subscribed, so push it.
     */
    fun deliverDeepLink(url: String) {
        emit("deep-link", listOf(JsonPrimitive(url)))
    }

    // ---------------------------------------------------------------------
    // Page lifecycle
    // ---------------------------------------------------------------------

    /**
     * The page is being replaced — reload, or the render process died.
     *
     * The new page shares no state with the old one, so we re-arm the queue
     * and abandon in-flight work: its replies carry ids only the dead page
     * understood. Queued events are dropped rather than replayed because they
     * describe a timeline the fresh page never saw; it re-reads current state
     * on mount instead.
     */
    fun onPageGone() {
        main.post {
            ready = false
            queued.clear()
            pageJobs.cancel()
            pageJobs = Job()
        }
    }

    // ---------------------------------------------------------------------
    // Dispatch table
    // ---------------------------------------------------------------------

    /**
     * Every channel this host answers. `shared/conformance/check-coverage.js`
     * reads the block below and fails the build for any required channel that
     * is missing, so this is the to-do list as well as the implementation.
     */
    private val handlers: Map<String, ChannelHandler> = mapOf(
        // BRIDGE-CHANNELS-BEGIN
        "get-initial-config" to { _ ->
            buildJsonObject {
                put("apiUrl", apiUrl())
                // Omitted rather than sent blank: the UI treats a missing tag
                // as "use the default" but an empty string as a real value.
                BuildConfig.DISTRO_RELEASE_TAG.ifBlank { null }?.let { put("distroReleaseTag", it) }
                BuildConfig.DEVICE_RELEASE_TAG.ifBlank { null }?.let { put("deviceReleaseTag", it) }
            }
        },

        "get-app-version" to { _ ->
            buildJsonObject {
                put("version", BuildConfig.VERSION_NAME)
                put("commit", BuildConfig.GIT_COMMIT.ifEmpty { null })
                put("apiUrl", apiUrl())
                put("platform", "android")
                put("arch", android.os.Build.SUPPORTED_ABIS.firstOrNull() ?: "unknown")
                // Two update paths means two ways to be wrong about what a user
                // is running. The binary version no longer identifies the UI —
                // `bundle` does, and without it every bug report is a guess.
                put("bundle", BundleStore.active())
                put("hostRevision", HOST_REVISION)
            }
        },

        "get-system-language" to { _ -> JsonPrimitive(Locale.getDefault().toLanguageTag()) },

        // No install wizard exists on Android: "installed" means first-run
        // setup has produced the data directory and the bundled JRE. Until
        // that setup lands (M2) there is nothing to do and nothing to fail.
        "is-installed" to { _ -> JsonPrimitive(true) },
        "get-install-type" to { _ -> JsonPrimitive("native") },
        "check-system-time" to { _ -> JsonPrimitive(true) },

        // A send, not an invoke. The UI's boot state machine blocks until one
        // of system-check-complete / system-check-failed arrives, so emitting
        // is the whole contract here.
        "start-installation-or-check" to { _ ->
            emit("system-check-complete")
            null
        },

        // The UI authenticates against the backend itself and hands the result
        // down. The host keeps them because the server backend and the device
        // WebSocket need to call the API without a page in front of them.
        //
        // Emitting credentials-set is the load-bearing part: the boot state
        // machine waits on that event before routing to the dashboard, so a
        // handler that only stores and stays quiet hangs login at a spinner.
        "credentials-received" to { params ->
            if (params is JsonObject) {
                prefs.edit().putString(KEY_CREDENTIALS, json.encodeToString(params)).apply()
                (params["apiUrl"] as? JsonPrimitive)?.contentOrNull
                    ?.let { prefs.edit().putString(KEY_API_URL, it).apply() }

                // The first moment this host holds a user token, which is the
                // only thing registration needs. Deliberately not awaited —
                // the UI blocks on credentials-set to leave the login screen,
                // and a device that registers a second later costs nothing.
                scope.launch { DeviceRegistry.ensure(apiUrl(), userToken()) }
                // The device's own link, which the dashboard dials to reach
                // this device's console. Provisioning polls for up to a
                // minute, so like registration it is started and not awaited.
                DeviceWebsocket.ensure(apiUrl(), userToken())

                emit("credentials-set")
            } else {
                emit("credentials-error", listOf(JsonPrimitive("Credentials were not an object")))
            }
            null
        },

        // Params are the user's email; the desktop also takes a
        // shouldRemoveDistro flag that has no meaning here.
        // Params are the user's email; the desktop also takes a
        // shouldRemoveDistro flag that has no meaning here.
        "logout" to { _ ->
            prefs.edit().remove(KEY_CREDENTIALS).apply()
            // The registration belongs to the user who made it, and its token
            // authenticates reports as them. Keeping it would have the next
            // person to log in heartbeating someone else's device.
            DeviceRegistry.clear()
            // And the link that made this device reachable by name. It was
            // provisioned against that registration, so leaving it up would
            // serve the next user's traffic over the previous one's tunnel.
            DeviceWebsocket.stop()
            null
        },

        "clipboard-write-text" to { params ->
            val text = params?.jsonPrimitive?.content.orEmpty()
            val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            clipboard.setPrimaryClip(ClipData.newPlainText("Homerun", text))
            null
        },

        "open-external-url" to { params ->
            val url = params?.jsonPrimitive?.content.orEmpty()
            JsonPrimitive(openExternal(url))
        },

        // The **backend's** id for this device, not a local one. A native
        // server is hosted on a device and the API binds it by this id, so an
        // invented value fails server creation with "does not exist" — which
        // is exactly what earlier builds did. Registers on demand, because the
        // sidebar asks for this on mount and may get there before the login
        // handler's registration has finished.
        "get-device-id" to { _ ->
            val registration = DeviceRegistry.ensure(apiUrl(), userToken())
            if (registration == null) JsonNull else JsonPrimitive(registration.deviceId)
        },

        // The port the device websocket server will bind. Reserved once the
        // device's link is up; still null until then, and null is the
        // documented "not running" answer.
        //
        // It answers a port before there is a server on it (D1 holds the
        // tunnel, D2 binds the socket). That is deliberate: nothing in the UI
        // reads this while `deviceWebsocket` is false in the capabilities, so
        // the only consumer today is a person reading logs.
        "get-device-ws-port" to { _ ->
            DeviceWebsocket.port?.let { JsonPrimitive(it) } ?: JsonNull
        },

        "set-api-url" to { params ->
            prefs.edit().putString(KEY_API_URL, params?.jsonPrimitive?.content).apply()
            null
        },

        "set-posthog-distinct-id" to { params ->
            prefs.edit().putString(KEY_POSTHOG_ID, params?.jsonPrimitive?.content).apply()
            null
        },

        "cache-client-nonce" to { params ->
            prefs.edit().putString(KEY_CLIENT_NONCE, params?.jsonPrimitive?.content).apply()
            null
        },

        // The pull half of deep-link delivery. Read-and-clear: the UI calls
        // this once on mount, and a link must not be redelivered on the next
        // reload.
        "deep-link:consume" to { _ ->
            val url = pendingDeepLink.getAndSet(null)
            Log.i(TAG, "deep-link:consume -> ${url ?: "nothing pending"}")
            url?.let { JsonPrimitive(it) } ?: JsonNull
        },

        "journey-modals-get" to { _ ->
            val stored = prefs.getString(KEY_JOURNEY_MODALS, null)
            if (stored == null) JsonObject(emptyMap())
            else runCatching { json.parseToJsonElement(stored) as JsonObject }
                .getOrElse { JsonObject(emptyMap()) }
        },

        "journey-modals-set" to { params ->
            prefs.edit().putString(KEY_JOURNEY_MODALS, json.encodeToString(params ?: JsonNull)).apply()
            JsonPrimitive(true)
        },

        // ─── storage ─────────────────────────────────────────────────────
        //
        // All three of these were unhandled until now, and an unanswered
        // invoke does not fail — it hangs the UI's promise for ever
        // (PROTOCOL.md §5). A screen that asks for storage figures simply
        // froze.

        "get-storage-info" to { _ -> storageInfo() },

        /**
         * A send, not an invoke: the answer comes back as an event.
         *
         * Under a gigabyte free a world save can fail partway through, which
         * is how worlds get corrupted. Warning before that happens is the
         * whole point, so the threshold matches iOS exactly.
         */
        "check-homerun-storage-limit" to { _ ->
            val free = usableBytes()
            emit(
                if (free != null && free < LOW_STORAGE_BYTES) "storage-limit-exceeded"
                else "storage-limit-ok"
            )
            null
        },

        /**
         * A loopback. Android has a per-app storage pane, but opening it from
         * here would take the player out of the app mid-flow; the UI decides
         * what to do with the echo, exactly as on iOS.
         */
        "open-storage-settings" to { _ ->
            emit("open-storage-settings")
            null
        },

        // ─── region picking ──────────────────────────────────────────────

        /**
         * Round-trip time to a gateway region, in milliseconds.
         *
         * 9999 is the contract's "unreachable" — a number rather than an
         * error, because the UI sorts regions by this and a throw would lose
         * the whole list to one bad host.
         */
        "measure-region-latency" to { params ->
            JsonPrimitive(measureLatency(params?.jsonPrimitive?.contentOrNull))
        },

        // ─── notifications ───────────────────────────────────────────────

        /**
         * Show a local notification.
         *
         * POST_NOTIFICATIONS is a runtime permission from API 33, and it is
         * not requested here: a notification that cannot be shown is not worth
         * a permission prompt in the middle of whatever the player was doing.
         * Silently doing nothing is the documented behaviour when it is
         * denied.
         */
        "push-notification" to { params ->
            val payload = params as? JsonObject
            val message = payload?.get("message")?.jsonPrimitive?.contentOrNull
            if (message != null) {
                notify(
                    title = payload["title"]?.jsonPrimitive?.contentOrNull
                        ?: context.getString(R.string.app_name),
                    body = message,
                )
            }
            null
        },

        // ─── files ───────────────────────────────────────────────────────

        /**
         * `exists: null` hides the files UI entirely.
         *
         * A phone has no file manager a player can usefully be dropped into,
         * and the server directory is app-private storage no other app can
         * read. Reporting `false` would show the UI in an empty state; null is
         * the contract's "do not offer this".
         */
        "server-files-exist" to { _ ->
            buildJsonObject {
                put("native", true)
                put("exists", JsonNull)
            }
        },

        /**
         * Unreachable while `server-files-exist` answers null, but answered
         * rather than dropped: a channel the UI calls anyway must return an
         * error, never a silent success and never a hang.
         */
        "open-server-files" to { _ ->
            buildJsonObject { put("error", "Opening server files isn't available on Android.") }
        },

        // ─── Bedrock ─────────────────────────────────────────────────────

        /**
         * Android hosts Java servers only, so there is no Bedrock server
         * version to report. Required by the contract because this host
         * declares the `javaNative` backend, which the desktop uses for both.
         */
        "native-get-latest-bedrock-version" to { _ ->
            buildJsonObject {
                put("success", false)
                put("error", "Bedrock servers aren't supported on Android.")
            }
        },

        "get-system-memory" to { _ -> systemMemory() },
        "get-native-system-memory" to { _ -> systemMemory() },

        // Running, coming up, OR winding down. The core keeps that list and
        // this host does not get a second opinion — see [lifecycle].
        "native-server-active-ids" to { _ ->
            buildJsonArray { lifecycle.activeIds().forEach { add(JsonPrimitive(it)) } }
        },

        // The `native-server-*` family IS the local-server interface. The name
        // is desktop-legacy; on Android it drives the in-process engine.
        "native-server-start" to { params ->
            val obj = params as? JsonObject ?: throw IllegalArgumentException("start needs an object")
            val serverId = obj.serverId()

            // Before anything else, including the two lookups below. Whether
            // this is a duplicate, and whether another server holds this
            // host's single slot, are the core's calls — the backend is never
            // asked to re-decide them.
            val admission = lifecycle.startRequested(serverId)
            try {
                when (admission.verdict) {
                    // What the reconcile loop expects to hear when it races
                    // the user's own start. Not an error a player ever sees.
                    "alreadyRunning" -> buildJsonObject {
                        put("success", true)
                        put("alreadyRunning", true)
                    }

                    "anotherServerRunning" -> buildJsonObject {
                        put("success", false)
                        put(
                            "error",
                            "Another server is already running. Stop it first — " +
                                "this device can host one at a time.",
                        )
                    }

                    // Paired with `hostingSettled` in this branch's own finally,
                    // not the outer one. The outer finally also runs for the two
                    // refusals above — and a reconcile-loop start that races the
                    // user's own gets `alreadyRunning`, so settling there would
                    // stand the foreground service down underneath a launch that
                    // is still resolving its settings.
                    else -> try {
                        val config = obj["config"] as? JsonObject

                        // Raise the process to foreground importance before the
                        // two lookups below, not after. Both are network
                        // round-trips, and until the foreground service is up
                        // this process is merely cached — a launch reclaimed
                        // between here and the spawn is a server the user was
                        // told was starting and that never did.
                        //
                        // This is also the only place the server's name is
                        // known; the backend is never told it, and the
                        // notification wants something to call it.
                        ServerHost.hostingRequested(
                            serverId,
                            config?.get("name")?.jsonPrimitive?.contentOrNull,
                        )
                        backend.create(serverId)

                        // The UI sends only a name and a memory ceiling; which Minecraft
                        // version and which loader live on the backend. Fetched here
                        // rather than in the backend so the access token never reaches
                        // the server process's environment. Null means the lookup failed
                        // — vanilla latest, the same fallback the desktop takes.
                        val token = obj["userToken"]?.jsonPrimitive?.contentOrNull.orEmpty()
                        val api = apiUrl()
                        val settings = HomerunApi.serverSettings(api, serverId, token)

                        // Before the backend starts, so a crash while starting
                        // up still has somewhere to be reported from.
                        Reporting.starting(serverId, settings)

                        // The device id restic records as the snapshot hostname, and the
                        // one the API resolves `pushed_by` from. Registration is already
                        // done by now in any normal flow; null just means no backups.
                        val deviceId = DeviceRegistry.currentDeviceId()

                        try {
                            if (settings?.gameType == "bedrock") {
                                throw ServerBackendException.Engine(
                                    "Homerun for Android cannot host Bedrock servers yet."
                                )
                            }

                            // Refuse to launch while another device is finishing its
                            // backup: starting now would build a second world from a
                            // snapshot that is still being written. `force` is the UI's
                            // data-loss-warning takeover.
                            if (settings != null && deviceId != null) {
                                val force = obj["force"]?.jsonPrimitive?.booleanOrNull == true
                                BackupManager(context).leaseBlockedReason(settings, deviceId, force)?.let {
                                    throw ServerBackendException.Engine(it)
                                }
                            }
                            backend.start(
                                serverId,
                                ServerConfig(
                                    name = config?.get("name")?.jsonPrimitive?.contentOrNull ?: serverId,
                                    memoryMb = config?.get("memoryMb")?.jsonPrimitive?.intOrNull ?: 1024,
                                    version = settings?.version,
                                    loader = settings?.loader ?: "vanilla",
                                    // Read and written to files by the backend, never
                                    // forwarded into the server's environment.
                                    settingsEnv = settings?.env,
                                    gameType = settings?.rawGameType ?: "java",
                                    // Null when the server has no repository, backups are
                                    // off for it, or this device is not registered — all
                                    // of which mean "host without backups" rather than
                                    // "refuse to host".
                                    backupContext = if (settings?.backup != null && deviceId != null) {
                                        BackupContext(settings, deviceId)
                                    } else null,
                                    // A closure, so the token stays here. The backend
                                    // gets the ability to resolve a tunnel, never the
                                    // credential that resolves it — `ServerConfig.extra`
                                    // is forwarded into the server process's environment
                                    // and this must never be able to end up there.
                                    resolveTunnel = {
                                        HomerunApi.awaitTunnel(
                                            api, serverId, token, stale = settings?.tunnelBefore,
                                        )
                                    },
                                ),
                            )
                            buildJsonObject { put("success", true) }
                        } catch (already: ServerBackendException.AlreadyRunning) {
                            buildJsonObject {
                                put("success", true)
                                put("alreadyRunning", true)
                            }
                        } catch (err: ServerBackendException) {
                            buildJsonObject {
                                put("success", false)
                                put("error", err.message)
                            }
                        }
                    } finally {
                        // Whatever happened. A launch refused by the lease, or
                        // by a game type this host cannot run, throws before the
                        // backend announces anything — so no `stopped` is coming
                        // to release the service, and without this the
                        // notification would describe a server that never
                        // started for as long as the process lived.
                        ServerHost.hostingSettled(serverId)
                    }
                }
            } finally {
                // Always, whatever the verdict was: the core counts every
                // call that arrived, so an unconditional finally balances.
                lifecycle.callFinished(serverId)
            }
        },

        // Thin on purpose. The sequence lives in [ServerHost.stop] because the
        // foreground notification's Stop action has to be the *same* stop — and
        // the part that must not be re-derived is the core's `graceful` verdict,
        // which decides whether a JVM is asked to save its world or terminated.
        "native-server-stop" to { params ->
            val error = ServerHost.stop((params as JsonObject).serverId())
            buildJsonObject {
                put("success", error == null)
                if (error != null) put("error", error)
            }
        },

        "native-server-delete" to { params ->
            try {
                backend.delete((params as JsonObject).serverId())
                buildJsonObject { put("success", true) }
            } catch (err: ServerBackendException) {
                buildJsonObject {
                    put("success", false)
                    put("error", err.message)
                }
            }
        },

        // Reply arrives on native-server-rcon-response, not here — Pumpkin has
        // no RCON, so the command is dispatched in-process and its output
        // comes back through the console.
        "native-server-rcon" to { params ->
            val obj = params as JsonObject
            val serverId = obj.serverId()
            val command = obj["command"]?.jsonPrimitive?.contentOrNull.orEmpty()
            Log.i(TAG, "console command for $serverId: ${command.take(32)}")
            try {
                backend.command(serverId, command)
                // An `op` or a `ban` typed here has to reach the server's
                // settings too, or the next launch rewrites ops.json from the
                // API and quietly takes it back. Signed as the person who
                // typed it — the API strips a settings change from someone who
                // could not have made it in the UI.
                Reporting.consoleCommand(serverId, command)
                emit(
                    "native-server-rcon-response",
                    listOf(buildJsonObject {
                        put("serverId", serverId)
                        put("response", "")
                    })
                )
                buildJsonObject { put("success", true) }
            } catch (err: ServerBackendException) {
                buildJsonObject {
                    put("success", false)
                    put("error", err.message)
                }
            }
        },

        // The six metrics getters and the log reader take a bare server id
        // string, not an object. Desktop legacy, frozen in the contract.
        "native-server-get-uptime" to { params ->
            buildJsonObject {
                val at = backend.uptime(params.asServerId())?.toEpochMilli()
                if (at == null) put("startedAt", JsonNull) else put("startedAt", at)
            }
        },

        // The operators the server itself is running with, read from its
        // `ops.json` — the desktop answers this from the same file
        // (`getServerOps`). It was a hardcoded empty list here, which meant the
        // UI showed no operators however many the server had, and made a
        // working ops sync look broken.
        "native-server-get-ops" to { params ->
            buildJsonObject {
                put("ops", buildJsonArray {
                    opsOnDisk(params.asServerId()).forEach { add(it) }
                })
            }
        },

        "native-server-get-mem-usage" to { params ->
            val usage = backend.memoryUsage(params.asServerId())
            buildJsonObject {
                if (usage?.usedKb == null) put("usedKb", JsonNull) else put("usedKb", usage.usedKb)
                if (usage?.maxMb == null) put("maxMb", JsonNull) else put("maxMb", usage.maxMb)
            }
        },

        "native-server-get-cpu-usage" to { params ->
            val cpu = backend.cpuUsage(params.asServerId())
            buildJsonObject {
                if (cpu == null) put("cpuPercent", JsonNull) else put("cpuPercent", cpu)
            }
        },

        "native-server-get-players" to { params ->
            val roster = backend.players(params.asServerId())
            if (roster == null) JsonNull else buildJsonObject {
                put("players", buildJsonArray {
                    roster.players.forEach { player ->
                        add(buildJsonObject {
                            put("name", player.name)
                            if (player.uuid == null) put("uuid", JsonNull) else put("uuid", player.uuid)
                        })
                    }
                })
                if (roster.max == null) put("max", JsonNull) else put("max", roster.max)
            }
        },

        "native-server-get-perf-history" to { params ->
            buildJsonArray {
                backend.perfHistory(params.asServerId()).forEach { sample ->
                    add(buildJsonObject {
                        put("t", sample.t)
                        if (sample.memUsedMb == null) put("memUsedMb", JsonNull) else put("memUsedMb", sample.memUsedMb)
                        if (sample.cpuPercent == null) put("cpuPercent", JsonNull) else put("cpuPercent", sample.cpuPercent)
                        if (sample.playerCount == null) put("playerCount", JsonNull) else put("playerCount", sample.playerCount)
                    })
                }
            }
        },

        "get-native-server-logs" to { params ->
            // Whole-buffer read, independent of the pump's cursor: the UI asks
            // for this when a console mounts and needs the backlog.
            buildJsonArray {
                backend.logs(params.asServerId(), 0).lines.forEach { add(JsonPrimitive(it)) }
            }
        },

        "get-native-server-port" to { params ->
            val port = backend.port((params as JsonObject).serverId())
            buildJsonObject { if (port == null) put("port", JsonNull) else put("port", port) }
        },

        // Local-network exposure is a router/firewall concern the desktop
        // solves with UPnP. Nothing to toggle here yet; report the truth.
        "get-native-local-network" to { _ -> buildJsonObject { put("enabled", false) } },
        "set-native-local-network" to { _ ->
            buildJsonObject {
                put("success", false)
                put("error", "Local network exposure is not configurable on Android yet.")
            }
        },
        // BRIDGE-CHANNELS-END
    )

    // ---------------------------------------------------------------------
    // Handler helpers
    // ---------------------------------------------------------------------

    private fun apiUrl(): String = prefs.getString(KEY_API_URL, null) ?: BuildConfig.API_URL

    /**
     * The user's access token, as handed down at login.
     *
     * Registration and the tunnel poll both need it, and it lives here rather
     * than in the backend so it can never reach a server process's
     * environment.
     */
    /**
     * The names in a server's `ops.json`.
     *
     * Read from disk rather than from the API, so it reflects what the running
     * server actually honours — including an `/op` typed into the console,
     * which the server writes there itself and the API does not learn about
     * until [Reporting] mirrors it back.
     *
     * Best-effort: a server that has never started has no file, and an empty
     * list is the honest answer for that.
     */
    private fun opsOnDisk(serverId: String): List<String> = runCatching {
        val file = File(context.filesDir, "servers/$serverId/ops.json")
        if (!file.exists()) return emptyList()
        (json.parseToJsonElement(file.readText()) as? JsonArray)
            ?.mapNotNull { (it as? JsonObject)?.get("name")?.jsonPrimitive?.contentOrNull }
            ?: emptyList()
    }.getOrElse {
        Log.w(TAG, "could not read the operators for $serverId: ${it.message}")
        emptyList()
    }

    private fun userToken(): String = runCatching {
        val stored = prefs.getString(KEY_CREDENTIALS, null) ?: return@runCatching ""
        (json.parseToJsonElement(stored) as? JsonObject)
            ?.get("access_token")?.jsonPrimitive?.contentOrNull.orEmpty()
    }.getOrDefault("")

    private fun openExternal(url: String): Boolean = try {
        val intent = Intent(Intent.ACTION_VIEW, Uri.parse(url))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        context.startActivity(intent)
        true
    } catch (err: Exception) {
        Log.w(TAG, "could not open $url: ${err.message}")
        false
    }

    /** The desktop shape: a `memory` string in MB, or an error. */
    /**
     * Free bytes on the volume holding app-private storage.
     *
     * `usableSpace` rather than `freeSpace`: the difference is the reserve
     * only privileged processes may touch, and a server writing a world is
     * not one of them. Reporting the larger number would promise room that
     * does not exist.
     */
    private fun usableBytes(): Long? =
        runCatching { context.filesDir.usableSpace.takeIf { it > 0 } }.getOrNull()

    /**
     * Device storage figures, in gigabytes, as the UI's storage panel expects.
     *
     * Decimal GB (1e9), matching iOS — a phone's advertised capacity is
     * decimal, so binary units here would report a 128 GB device as 119 and
     * look like a bug to the player.
     */
    private fun storageInfo(): JsonElement = buildJsonObject {
        put("installType", "native")

        val dir = context.filesDir
        val total = runCatching { dir.totalSpace.takeIf { it > 0 } }.getOrNull()
        val free = usableBytes()
        val gb = { bytes: Long -> bytes / 1_000_000_000.0 }

        if (total != null) put("totalStorageGB", gb(total))
        if (free != null) {
            put("totalStorageFreeGB", gb(free))
            if (total != null) put("totalStorageUsedGB", gb(total) - gb(free))
        }
    }

    /**
     * Round-trip time to a region endpoint, in milliseconds.
     *
     * A HEAD with a short timeout, and [UNREACHABLE_MS] for anything that
     * fails. The UI sorts regions by this figure, so an exception would cost
     * the whole list rather than one entry.
     */
    private suspend fun measureLatency(url: String?): Int = withContext(Dispatchers.IO) {
        val target = url?.let { runCatching { URL(it) }.getOrNull() } ?: return@withContext UNREACHABLE_MS
        runCatching {
            val started = System.nanoTime()
            val connection = (target.openConnection() as HttpURLConnection).apply {
                requestMethod = "HEAD"
                connectTimeout = LATENCY_TIMEOUT_MS
                readTimeout = LATENCY_TIMEOUT_MS
                useCaches = false
            }
            try {
                connection.responseCode
            } finally {
                connection.disconnect()
            }
            ((System.nanoTime() - started) / 1_000_000).toInt()
        }.getOrDefault(UNREACHABLE_MS)
    }

    /**
     * Post a local notification, or do nothing if we may not.
     *
     * From API 33 this needs POST_NOTIFICATIONS at runtime, and it is
     * deliberately not requested here: a permission prompt interrupting
     * whatever the player was doing costs more than the notification is worth.
     * Denied means silence, which is what the contract allows.
     */
    private fun notify(title: String, body: String) {
        runCatching {
            val manager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            if (!manager.areNotificationsEnabled()) return

            manager.createNotificationChannel(
                NotificationChannel(
                    NOTIFICATION_CHANNEL,
                    // A channel name is a category *within* the app, so naming
                    // it after the app read as "Homerun / Homerun" in system
                    // settings — and gave the user nothing to distinguish it
                    // from the hosting channel when deciding what to mute.
                    context.getString(R.string.alerts_channel_name),
                    NotificationManager.IMPORTANCE_DEFAULT,
                )
            )

            val notification = Notification.Builder(context, NOTIFICATION_CHANNEL)
                .setContentTitle(title)
                .setContentText(body)
                // The dedicated monochrome icon, for the reason spelled out in
                // res/drawable/ic_notification.xml: a small icon is drawn from
                // its alpha channel, so the launcher icon renders as a solid
                // shape with no mark in it.
                .setSmallIcon(R.drawable.ic_notification)
                .setColor(context.getColor(R.color.brand_cornflower))
                .setAutoCancel(true)
                .build()

            manager.notify(body.hashCode(), notification)
        }.onFailure { Log.w(TAG, "could not post a notification: ${it.message}") }
    }

    private fun systemMemory(): JsonElement {
        val manager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        val info = ActivityManager.MemoryInfo().also { manager.getMemoryInfo(it) }
        return buildJsonObject {
            put("success", true)
            put("memory", (info.totalMem / (1024 * 1024)).toString())
        }
    }

    companion object {
        const val TAG = "HomerunBridge"

        /**
         * Below this, warn. A world save that runs out of room partway through
         * is how worlds get corrupted, and one gigabyte is the same line iOS
         * draws.
         */
        const val LOW_STORAGE_BYTES = 1_073_741_824L

        /** The contract's "unreachable", as a latency rather than an error. */
        const val UNREACHABLE_MS = 9999

        const val LATENCY_TIMEOUT_MS = 5_000

        const val NOTIFICATION_CHANNEL = "homerun"

        /** The name JavaScript sees; PROTOCOL.md §3.3 fixes it. */
        const val JS_INTERFACE = "HomerunHost"

        /**
         * How far along `shared/conformance/host-revisions.json` this host is.
         *
         * `PROTOCOL.md` §7 versions the protocol and says changes are additive,
         * which is why every host answers `v: 1` for ever and the UI cannot
         * tell a January host from a July one. That is harmless while the
         * bundle and the host ship in one binary, and stops being harmless the
         * moment a bundle arrives over the air: a call to a channel this host
         * has never heard of leaves a promise pending for ever, and the user
         * sees a frozen screen with no error.
         *
         * So: bump this whenever the table below gains a channel, and add the
         * matching ledger entry. `scripts/check-host-revision.js` compares the
         * two and fails the build if you do one without the other — the same
         * discipline as `FFI_ABI_VERSION`, one layer up.
         */
        const val HOST_REVISION = 1

        /** Protocol-level, deliberately absent from the channel inventory. */
        private const val READY_METHOD = "__bridge:ready"

        private const val PREFS = "homerun-host"
        private const val KEY_API_URL = "api-url"
        /**
         * Dead. Earlier builds stored a locally minted UUID here and handed it
         * to the UI as the device id; the backend had never heard of it, so
         * server creation failed with "does not exist". The real registration
         * lives in [DeviceRegistry] under its own keys. Named here only so
         * nobody reintroduces it.
         */
        private const val KEY_LEGACY_DEVICE_ID = "device-id"
        private const val KEY_POSTHOG_ID = "posthog-distinct-id"
        private const val KEY_CLIENT_NONCE = "client-nonce"
        private const val KEY_JOURNEY_MODALS = "journey-modals"
        private const val KEY_CREDENTIALS = "credentials"

        /**
         * Legal in JSON, fatal in JavaScript source before ES2019. The WebView
         * is modern enough not to care, but the literal is pasted into a
         * script and the cost of being certain is one string replace.
         */
        private fun escapeForJs(json: String): String =
            json.replace("\u2028", "\\u2028").replace("\u2029", "\\u2029")
    }
}

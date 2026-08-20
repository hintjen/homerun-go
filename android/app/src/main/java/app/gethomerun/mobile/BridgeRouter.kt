package app.gethomerun.mobile

import android.app.ActivityManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.os.Build
import android.util.Log
import android.webkit.JavascriptInterface
import android.webkit.WebView
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import java.io.File
import java.net.URI
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
import kotlin.math.roundToInt

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
 * Threading is the thing to get right here, and **three** threads are in play,
 * not two:
 *
 *  - **A binder thread.** `postMessage` is reached through
 *    `addJavascriptInterface`, so it runs on a thread the WebView owns rather
 *    than one we do. Parsing happens there — cheap, no shared state — and
 *    nothing else does; the envelope is handed straight to the main thread.
 *  - **The main thread.** Router bookkeeping ([ready], [queued]) is confined
 *    to it, and so is every touch of the WebView. [emit] and [reply] hop back
 *    on their own, which is why both are safe to call from anywhere.
 *  - **[Dispatchers.IO].** Every channel handler, because [dispatch] launches
 *    with it explicitly. That is not decoration: [scope] is the activity's
 *    `lifecycleScope`, whose dispatcher is `Main.immediate`, so `launch(job)`
 *    with only a [Job] in the context inherits the **UI** thread. It did, for
 *    every handler, while this doc claimed a slow one could not block —
 *    true of the binder thread it was written about and never true of the UI
 *    thread. Reachable underneath that claim: a recursive world delete, a
 *    manifest read out of a 40–60 MB jar, `Process.waitFor(5, SECONDS)`, a
 *    250 ms JNI poll held for up to five minutes, and two statfs calls.
 *
 * Four handlers hop back to the main thread and each says why in place:
 * `clipboard-write-text`, `set-appearance`, `quit-and-install` and `haptic`.
 * The test for a fifth is narrow — a handler needs the main thread only if it
 * touches the WebView, the window, or a framework object that builds a
 * `Handler` from the *calling* thread's Looper. `SharedPreferences`,
 * `ActivityManager`, `NotificationManager`, `startActivity` and the JNI calls
 * into `homerun-core` are all safe where they are, and most of them block.
 *
 * The consequence to know about: handlers no longer run in arrival order.
 * They did, by accident — on `Main.immediate` a handler ran inline until its
 * first suspension — and nothing ever promised it; PROTOCOL.md orders replies
 * against their own invoke and nothing else. A channel that must be sequenced
 * against another has to say so itself.
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

    /**
     * How `quit-and-install` applies a staged bundle: the activity promotes it
     * and rebuilds its WebView.
     *
     * A hook rather than a direct call because the router deliberately does not
     * own the activity — it survives the WebView being destroyed and rebuilt,
     * and reaching into an activity it does not hold would invert that.
     *
     * Null before an activity attaches, which is a real state: the router is
     * constructed before the first page exists.
     *
     * **Invoked on the main thread**, because what it does on the other side
     * is rebuild a view hierarchy. Handlers otherwise run on
     * [Dispatchers.IO], so this is a guarantee the router makes rather than
     * one every activity has to remember to make for itself.
     */
    @Volatile
    var onApplyUpdate: (() -> Unit)? = null

    /**
     * The page's theme, for the system bars. `"light"` or `"dark"`.
     *
     * A hook for the same reason as [onApplyUpdate], and **invoked on the
     * main thread** for the same reason too: it ends in the window's insets
     * controller. Two hooks with different threading rules would be a trap
     * worth more than the one dispatch it saves.
     */
    @Volatile
    var onAppearance: ((String) -> Unit)? = null

    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
        explicitNulls = false
    }

    private val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** Resolves the device's vibrator once; the `haptic` channel plays through it. */
    private val haptics = Haptics(context)

    /**
     * The signed-in Minecraft account, if there is one.
     *
     * Router-scoped rather than process-scoped, unlike [backend]: nothing here
     * outlives a page, because the session itself lives in [SecretStore] and a
     * new router reads the same one back.
     */
    private val minecraftAuth = MinecraftAuth(context)

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

        // Heal an override this app itself may have written. `set-api-url`
        // used to store `JsonNull.content` — the string "null" — whenever the
        // page sent nothing, and from then on every backend call was built on
        // it: no login, no device, no server, and no way back short of
        // reinstalling. Re-checking the stored value on each launch also
        // reaches [DeviceRegistry], which reads this key directly and cannot
        // be given the rule any other way.
        prefs.getString(KEY_API_URL, null)?.let {
            if (validApiUrl(it) == null) {
                Log.w(TAG, "dropping a stored API base that is not a usable https URL")
                prefs.edit().remove(KEY_API_URL).apply()
            }
        }

        // A relaunch already holds the token issued at login, so the device's
        // link belongs to the process rather than to the login — the same
        // reasoning that starts the heartbeat in `HomerunApplication`. Waiting
        // for `set-credentials` would mean a device is only reachable in the
        // session it logged in on. `ensure` is idempotent, so a router rebuilt
        // for a new activity does not start a second one.
        DeviceWebsocket.ensure(apiUrl(), userToken())
    }

    /**
     * The `native-server-*` channels that take an object payload.
     *
     * Validated here rather than at each sink. Both backends check inside
     * `dataDir`, which covers every path they build — but the router builds one
     * itself (`opsOnDisk`), and the next handler that needs a path will be
     * written the same way. These two extractors are the only door a page's
     * `serverId` comes through, so the rule belongs on them.
     */
    private fun JsonObject.serverId(): String =
        requireValidServerId(
            this["serverId"]?.jsonPrimitive?.contentOrNull
                ?: throw IllegalArgumentException("serverId is required")
        )

    /** The six metrics getters and the log reader take a bare string instead. */
    private fun JsonElement?.asServerId(): String =
        requireValidServerId(
            this?.jsonPrimitive?.contentOrNull
                ?: throw IllegalArgumentException("serverId is required")
        )

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

    /**
     * An uncaught page error, from the document-start hook in
     * [MainActivity]'s bootstrap script.
     *
     * Protocol-level, handled here rather than in [handlers]: `__host:` names
     * bypass the channel table by design and are deliberately absent from
     * `channels.ts`, so this needs no contract entry and no host revision.
     *
     * A location rather than a stack, because `window.onerror` gives a file
     * and a line and nothing else for a script it did not load same-origin.
     * The core groups on whatever it is given.
     */
    private fun logJsError(params: JsonElement?) {
        val details = params as? JsonObject
        val message = details?.get("message")?.jsonPrimitive?.contentOrNull ?: "?"
        val source = details?.get("source")?.jsonPrimitive?.contentOrNull.orEmpty()
        val line = details?.get("line")?.jsonPrimitive?.intOrNull ?: 0

        Log.e(TAG, "uncaught JS error: $message ($source:$line)")

        AppErrors.reportFromPage(buildJsonObject {
            put("source", AppErrors.SOURCE_UI)
            put("severity", AppErrors.SEVERITY_FATAL)
            put("kind", "boot")
            put("message", message)
            if (source.isNotEmpty()) put("location", "$source:$line")
        })
    }

    private fun dispatch(envelope: Envelope) {
        val method = envelope.method ?: return

        if (method == READY_METHOD) {
            onReady()
            return
        }

        if (method == JS_ERROR_METHOD) {
            logJsError(envelope.params)
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

        // [Dispatchers.IO] explicitly, and it is load-bearing. [pageJobs] is a
        // [Job] — it says who owns the coroutine, not where it runs — so a
        // bare `launch(pageJobs)` here took its dispatcher from [scope], which
        // is `lifecycleScope`, which is `Main.immediate`. Every handler below
        // ran on the UI thread, including the ones that delete a world tree or
        // sit on a JNI poll for minutes.
        //
        // Handlers that genuinely need the main thread hop back for the few
        // lines that do; see the class doc for which and why.
        scope.launch(pageJobs + Dispatchers.IO) {
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
     *
     * An OAuth callback is *not* a deep link and must not be emitted as one:
     * `lib/deepLink.ts` returns null for intents it does not recognise, so an
     * auth callback pushed here would be parsed, rejected and dropped in
     * silence while the sign-in call waited for ever. It goes to whoever is
     * awaiting the browser session instead.
     */
    fun deliverDeepLink(url: String) {
        if (completeAuthSession(url)) return
        emit("deep-link", listOf(JsonPrimitive(url)))
    }

    // ---------------------------------------------------------------------
    // Browser-based sign-in
    // ---------------------------------------------------------------------

    /**
     * The sign-in currently waiting on a browser, if any.
     *
     * Google refuses to authenticate inside an embedded WebView, so the page
     * cannot run this itself; it hands us a URL and waits. One at a time by
     * construction — a second call while one is outstanding cancels the first,
     * because there is only one browser and only one user.
     */
    private val pendingAuthSession = AtomicReference<CompletableDeferred<String>?>(null)

    /**
     * Set by [MainActivity], which owns the only ActivityResultLauncher that
     * can show the POST_NOTIFICATIONS sheet. Suspends until the sheet is
     * answered and returns the resulting status — no timeout, per the
     * protocol's rule. Null while no activity is attached, in which case
     * `push:request-permission` degrades to reporting the current state.
     */
    @Volatile
    var requestPushPermission: (suspend () -> String)? = null

    /**
     * The OS notification-permission state, in the contract's vocabulary.
     *
     * Below API 33 there is no runtime permission: notifications are on
     * unless the user turned them off in settings, so the answer is only
     * ever granted or denied. From 33, "off with no recorded ask" is
     * `notDetermined` — the one state where a prompt is worth showing.
     * [KEY_PUSH_ASKED] is written by MainActivity for *either* prompt (its
     * own first-hosting ask covers the same permission), because the OS
     * offers no reliable "was this ever asked" and a wrong `notDetermined`
     * makes the UI offer a prompt the OS will silently swallow.
     */
    fun pushPermissionStatus(): String {
        val manager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (manager.areNotificationsEnabled()) return "granted"
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            !prefs.getBoolean(KEY_PUSH_ASKED, false)
        ) {
            return "notDetermined"
        }
        return "denied"
    }

    /** True when [url] belonged to a waiting sign-in and was handed to it. */
    private fun completeAuthSession(url: String): Boolean {
        if (!url.startsWith("$AUTH_CALLBACK_PREFIX")) return false
        val waiting = pendingAuthSession.getAndSet(null) ?: return false
        Log.i(TAG, "auth:web-session -> callback received")
        waiting.complete(url)
        return true
    }

    /**
     * Called when the activity comes back to the foreground.
     *
     * A dismissed Custom Tab reports nothing at all — there is no cancel
     * callback — so the only signal that the user backed out is that we are
     * visible again with a session still outstanding. The delay is what
     * separates "they closed it" from "the redirect is a beat behind us",
     * since returning via the callback also resumes the activity.
     */
    fun onForegrounded() {
        val waiting = pendingAuthSession.get() ?: return
        scope.launch {
            delay(AUTH_CANCEL_GRACE_MS)
            if (pendingAuthSession.compareAndSet(waiting, null)) {
                Log.i(TAG, "auth:web-session -> dismissed without a callback")
                waiting.completeExceptionally(AuthSessionCanceled())
            }
        }
    }

    private class AuthSessionCanceled : Exception("Sign-in was cancelled.")

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

    /**
     * The router itself is finished — the activity that owns it is being
     * destroyed.
     *
     * Not the same as [onPageGone], which is a page ending inside a router that
     * lives on. This is the router ending, and the reason it must exist is that
     * [hostListener] is registered against [ServerHost], which is scoped to the
     * *process*: a router is built in every `onCreate`, so without this a
     * recreated activity — a low-memory kill, "Don't keep activities", a locale
     * or theme change, a fold — leaves the previous router subscribed forever,
     * holding a WebView that is already destroyed.
     *
     * The visible cost is not the memory. `onStateChanged` calls
     * [DeviceRegistry.reportServerState], so N abandoned routers send N
     * identical state POSTs per transition, and the API sees one device
     * reporting the same server over and over.
     */
    fun dispose() {
        ServerHost.removeListener(hostListener)
        onPageGone()
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

        // --- system chrome ---------------------------------------------------

        // The page's theme, so the status bar's text contrasts with it: a dark
        // page wants light glyphs. `MainActivity.applyChrome` derives the same
        // thing from the colours the page sends and is the more precise signal
        // — whichever arrives last wins, and both are describing one fact.
        "set-appearance" to { params ->
            val appearance = (params as? JsonPrimitive)?.contentOrNull
            if (appearance == "light" || appearance == "dark") {
                // Main, per [onAppearance]'s contract — the hook ends up in
                // the window's insets controller.
                withContext(Dispatchers.Main) { onAppearance?.invoke(appearance) }
            } else {
                Log.w(TAG, "set-appearance wants \"light\" or \"dark\", got: $appearance")
            }
            null
        },

        // Deliberately nothing to do. There is no `installSplashScreen` here:
        // the launch screen is the theme's window background, and Android
        // replaces it with the first frame the activity draws. The channel is
        // still answered rather than left to the unimplemented-channel warning,
        // because "handled, and the right thing to do is nothing" and "nobody
        // wrote this yet" are different states and only one of them is fine.
        "splash-shown" to { _ -> null },

        // What the user just did, for the motor. The payload is a meaning —
        // "selection", "commit" — and [Haptics] owns the translation into
        // Android's own feedback vocabulary.
        //
        // Main, because `performHapticFeedback` touches the view, and the view
        // is read inside the hop for the same reason `evaluate` does it: the
        // field belongs to whichever WebView currently exists.
        // The page failed at something nobody expected. Handed straight to
        // the core, which decides whether it is the same bug as the last one,
        // whether it is worth sending again, and what has to be redacted
        // first. Very often it decides not to send, and that is the rate
        // limiter working rather than anything to log about.
        //
        // Nothing here ever reports a failure to report. That is how a
        // reporter turns one bad response into a loop, fastest exactly when
        // the API is already struggling.
        "report-error" to { params ->
            val occurrence = params as? JsonObject
            if (occurrence == null) {
                Log.w(TAG, "report-error wants an object, got: $params")
            } else {
                AppErrors.reportFromPage(occurrence)
            }
            null
        },

        "haptic" to { params ->
            val pattern = (params as? JsonPrimitive)?.contentOrNull
            if (pattern == null) {
                Log.w(TAG, "haptic wants a pattern string, got: $params")
            } else {
                withContext(Dispatchers.Main) { haptics.play(pattern, webView) }
            }
            null
        },

        // --- the Minecraft account ------------------------------------------
        //
        // Which Minecraft player this phone belongs to. Minigame stats are
        // keyed on a Minecraft uuid and every read of them takes one as input,
        // so without these the hub could only ever show a signed-in user zero
        // of their own numbers. [MinecraftAuth] has the flow and the reason it
        // is device-code rather than a redirect.
        //
        // The two events are not optional and are the detail most likely to be
        // missed: several `useMinecraftAccount` consumers mount at once — the
        // profile banner and the leaderboard's "You" row are both on `/games` —
        // and each keeps its own state from them. A login that only answered
        // its caller would leave the other consumers signed out until reload.

        "minecraft:auth:get-profile" to { _ ->
            // Refreshes silently when the token has aged out. Null covers "not
            // signed in" and "the session could not be recovered" alike, which
            // is what the UI does with both anyway.
            minecraftAuth.profile()?.let { Core.accountRedacted(it) } ?: JsonNull
        },

        "minecraft:auth:login" to { _ ->
            try {
                val session = minecraftAuth.signIn { code ->
                    // Opened as soon as there is a code, before the poll that
                    // waits up to a quarter of an hour for approval.
                    //
                    // The code is not sent to the UI, and deliberately: the URL
                    // carries it as `?otc=`, so Microsoft fills it in and the
                    // user only has to confirm. Telling the page about it would
                    // mean a channel `bridge/v1` does not have — and the whole
                    // reason this feature needed no protocol change is that the
                    // three invokes and two events already existed.
                    minecraftAuth.openApproval(code)
                }
                val credentials = Core.accountRedacted(session)
                emit("minecraft:auth:ready", listOf(credentials))
                buildJsonObject {
                    put("success", true)
                    put("credentials", credentials)
                }
            } catch (err: Exception) {
                if (err is CancellationException) throw err
                // Written for a player: `MinecraftAuth.AuthException` messages
                // already are, and anything else would be a stack trace.
                Log.w(TAG, "Minecraft sign-in failed: ${err.message}")
                buildJsonObject {
                    put("success", false)
                    put(
                        "error",
                        (err as? MinecraftAuth.AuthException)?.message
                            ?: "Could not sign in to Microsoft.",
                    )
                }
            }
        },

        "minecraft:auth:logout" to { _ ->
            minecraftAuth.signOut()
            emit("minecraft:auth:signed-out")
            buildJsonObject { put("success", true) }
        },

        // --- over-the-air updates -------------------------------------------
        //
        // "Update" here is a new UI bundle, not a new binary. The channels are
        // the desktop's and deliberately so — the modal needs no platform
        // branch — but what they do underneath is different. See
        // `docs/ota-bundles.md`.

        // An **invoke**, so it must always answer. Every failure inside
        // `awaitCheck` is swallowed into null for exactly that reason: a check
        // that cannot reach the network is no reason to leave a UI promise
        // unresolved for ever.
        //
        // `status` mirrors the desktop's shape — the UI stores this payload and
        // tests `status !== "not-available"`.
        "wait-for-update-check" to { _ ->
            val staged = BundleUpdater.awaitCheck(context)
            buildJsonObject {
                put("status", if (staged == null) "not-available" else "available")
                // The bundle id is the version here. The binary's version has
                // not changed, and reporting it as though it had would be a
                // lie the UI writes to localStorage.
                staged?.let {
                    put("version", it)
                    put("bundle", it)
                }
            }
        },

        // Named for what it does on desktop, not for what it does here: this
        // **must not quit**. Quitting would take the foreground service and any
        // running Minecraft server with it, and on iOS there is no relaunch API
        // at all. Rebuilding the WebView is enough — about a second — and
        // `ServerHost` exists precisely so the engine outlives the page
        // (plans/ota-updates.md).
        "quit-and-install" to { _ ->
            val apply = onApplyUpdate
            if (apply == null) {
                // A `send`, so there is no reply to carry this. Logged because
                // the symptom is otherwise a button that does nothing at all.
                Log.w(TAG, "quit-and-install with no activity attached; ignoring")
            } else {
                // Main, per [onApplyUpdate]'s contract — the hook tears down
                // and rebuilds the WebView.
                withContext(Dispatchers.Main) { apply() }
            }
            null
        },

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
            // The second writer of the API base, so it takes the same rule as
            // set-api-url — and it matters more here, because the thing being
            // stored on the same breath is the token that would travel to
            // whatever host this named.
            val offered = (params as? JsonObject)?.get("apiUrl")?.takeIf { it !is JsonNull }
            val override = validApiUrl((offered as? JsonPrimitive)?.contentOrNull)

            if (params !is JsonObject) {
                emit("credentials-error", listOf(JsonPrimitive("Credentials were not an object")))
            } else if (offered != null && override == null) {
                // Refused, and nothing stored — not even the credentials. The
                // UI has authenticated somewhere this host will not follow it
                // to, so the two halves already disagree about which backend
                // they are on; a login that fails loudly beats one that
                // succeeds and then hosts against a different server.
                emit("credentials-error", listOf(JsonPrimitive(API_URL_REFUSED)))
            } else {
                SecretStore.write(prefs, KEY_CREDENTIALS, json.encodeToString(params))
                // No apiUrl at all leaves any existing override alone: the UI
                // is saying nothing about the backend, which is not the same
                // as set-api-url's explicit clear.
                override?.let { prefs.edit().putString(KEY_API_URL, it).apply() }

                // The first moment this host holds a user token, which is the
                // only thing registration needs. Deliberately not awaited —
                // the UI blocks on credentials-set to leave the login screen,
                // and a device that registers a second later costs nothing.
                //
                // Dispatched explicitly, because [scope] is `lifecycleScope`:
                // a bare `launch` here runs on the UI thread whatever the
                // handler around it is running on.
                //
                // And handled explicitly, because this is the one launch in
                // this file that escapes [dispatch]'s `catch`. `lifecycleScope`
                // carries no handler of its own, so a throw out of `ensure` —
                // a network failure at exactly the wrong moment — would reach
                // the default handler and take the process down, along with any
                // server it is hosting. See [ServerHost.keepAlive].
                scope.launch(
                    Dispatchers.IO + ServerHost.keepAlive(TAG, "registering this device"),
                ) { DeviceRegistry.ensure(apiUrl(), userToken()) }
                // The device's own link, which the dashboard dials to reach
                // this device's console. Provisioning polls for up to a
                // minute, so like registration it is started and not awaited.
                DeviceWebsocket.ensure(apiUrl(), userToken())

                emit("credentials-set")
            }
            null
        },

        // Params are the user's email; the desktop also takes a
        // shouldRemoveDistro flag that has no meaning here.
        "logout" to { _ ->
            // The API base leaves with the credentials. It outranks
            // BuildConfig.API_URL on every read, DeviceRegistry reads the same
            // key, and nothing used to clear it — so an override set once for
            // a staging backend survived the logout and pointed the *next*
            // user's bearer token at a host they never chose.
            prefs.edit().remove(KEY_CREDENTIALS).remove(KEY_API_URL).apply()
            // The registration belongs to the user who made it, and its token
            // authenticates reports as them. Keeping it would have the next
            // person to log in heartbeating someone else's device.
            DeviceRegistry.clear()
            // And the link that made this device reachable by name. It was
            // provisioned against that registration, so leaving it up would
            // serve the next user's traffic over the previous one's tunnel.
            DeviceWebsocket.stop()
            // The Minecraft account belongs to the person who signed it in, and
            // this is the worst of the three to leave behind. `get-profile`
            // would hand the next user the previous one's identity, and
            // `useMinecraftAccount` reports every profile it learns to
            // `/minecraft-account/link/` — so simply opening the Minigames Hub
            // would attach someone else's Minecraft account, and every stat
            // accumulated under it, to whoever logged in next.
            //
            // Local only, like the rest of this handler: nothing is revoked at
            // Microsoft. Signing back in is a fresh sign-in.
            minecraftAuth.signOut()
            // Told, not just forgotten. Consumers keep their own state from
            // these events, and a hub left mounted across a logout would go on
            // showing the previous player until something remounted it.
            emit("minecraft:auth:signed-out")
            null
        },

        // The one system service here that is not thread-agnostic.
        // `ClipboardManager` builds a `Handler` from the calling thread's
        // Looper, and on a plain worker thread there is none — it dies with
        // "Can't create handler inside thread that has not called
        // Looper.prepare()", which reads like a WebView bug and is not one.
        // Some OEM builds also toast from here, which is main-thread work by
        // definition.
        "clipboard-write-text" to { params ->
            val text = params?.jsonPrimitive?.content.orEmpty()
            withContext(Dispatchers.Main) {
                val clipboard =
                    context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                clipboard.setPrimaryClip(ClipData.newPlainText("Homerun", text))
            }
            null
        },

        "open-external-url" to { params ->
            val url = params?.jsonPrimitive?.content.orEmpty()
            JsonPrimitive(openExternal(url))
        },

        // `Intent.createChooser` over ACTION_SEND. Suspends until the user
        // picks a target or dismisses the sheet — a dismissal is an outcome
        // here, not an error, and the UI stays quiet about it. See [Sharing]
        // for how the two are told apart, which Android does not make obvious.
        "share-content" to { params ->
            val payload = params as? JsonObject
            fun field(name: String) =
                payload?.get(name)?.jsonPrimitive?.contentOrNull
            val completed = Sharing.share(
                context,
                title = field("title"),
                text = field("text"),
                url = field("url"),
            )
            buildJsonObject { put("completed", completed) }
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

        // Where this host sends the user's bearer token. Validated rather than
        // stored as it arrives — see [validApiUrl] for what the rule is and
        // why it is a shape check and not a list of hosts.
        "set-api-url" to { params ->
            // Absent, or JSON null, means put the build's own backend back.
            // It used to mean storing `JsonNull.content`, which is the four
            // characters n-u-l-l, which the read path then handed to
            // `URL(...)` as a perfectly good override — bricking every
            // backend call until the app was reinstalled.
            if (params == null || params is JsonNull) {
                prefs.edit().remove(KEY_API_URL).apply()
            } else {
                val url = validApiUrl((params as? JsonPrimitive)?.contentOrNull)
                    ?: throw IllegalArgumentException(API_URL_REFUSED)
                prefs.edit().putString(KEY_API_URL, url).apply()
            }
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

        /*
         * Run an OAuth redirect in a real browser and hand back where it
         * landed. A Custom Tab, never our own WebView: Google answers
         * `disallowed_useragent` to android.webkit.WebView, which is the
         * whole reason this channel exists.
         *
         * Suspends until the callback arrives through the activity's
         * `homerun://` intent filter, or until the user is seen to have
         * dismissed the tab. The bridge has no blanket call timeout by
         * design, and this is a case where that is right — the user may take
         * minutes to sign in, and the cancel path is what ends it.
         */
        "auth:web-session" to { params ->
            val obj = params as? JsonObject
            val url = obj?.get("url")?.jsonPrimitive?.contentOrNull
            if (url.isNullOrBlank()) {
                authFailure("No sign-in address was provided.")
            } else {
                val waiter = CompletableDeferred<String>()
                // A second sign-in supersedes the first; the old one can never
                // complete now that its callback would be routed here.
                pendingAuthSession.getAndSet(waiter)
                    ?.completeExceptionally(AuthSessionCanceled())
                try {
                    openInBrowser(url)
                    val callbackUrl = waiter.await()
                    buildJsonObject {
                        put("success", true)
                        put("callbackUrl", callbackUrl)
                    }
                } catch (_: AuthSessionCanceled) {
                    buildJsonObject {
                        put("success", false)
                        put("error", "Sign-in was cancelled.")
                        put("canceled", true)
                    }
                } catch (e: Exception) {
                    pendingAuthSession.compareAndSet(waiter, null)
                    Log.w(TAG, "auth:web-session failed", e)
                    authFailure("Could not open the sign-in page.")
                }
            }
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

        /**
         * Remote push (`remotePush` capability). The host's half is the OS
         * permission and the FCM token; the UI registers the token with the
         * API over the user's JWT and deletes it on logout — no identity
         * ever passes through here.
         */
        "push:permission" to { _ ->
            buildJsonObject { put("status", pushPermissionStatus()) }
        },
        "push:request-permission" to { _ ->
            // Unlike the local channel above, this one prompts — because the
            // UI asked it to, at a moment the user understands. The sheet is
            // MainActivity's; without one attached, report where we stand.
            val status = requestPushPermission?.invoke() ?: pushPermissionStatus()
            buildJsonObject { put("status", status) }
        },
        "push:get-token" to { _ ->
            // Null is a state, not an error: an emulator without Play
            // services stays null forever, and `push:token-changed` is what
            // announces one arriving later.
            buildJsonObject {
                val token = PushMessaging.currentToken()
                if (token != null) put("token", token) else put("token", JsonNull)
            }
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
                            // Before anything expensive. A modpack is minutes of
                            // downloading and unpacking, and on Pumpkin it would then
                            // start vanilla and look like it worked.
                            //
                            // This both refuses and *routes*: the same core call that
                            // says a server cannot run here says which of this device's
                            // engines runs it when it can. Asking the selected backend
                            // what it was would be backwards now that there are two.
                            //
                            // `rawGameType`, not `gameType` — the reduced form cannot
                            // tell `native-crossplay` from plain Java, and crossplay
                            // needs a plugin.
                            val engine = if (settings != null) {
                                ServerHost.select(settings.rawGameType, settings.env)
                            } else {
                                // No settings means no game type to route on, so this
                                // is the device's ordinary answer for a Java server.
                                ServerHost.select("native", JsonObject(emptyMap()))
                            }

                            // A minigame lobby is generated for one session and the
                            // API soft-deletes the server behind it, so there is no
                            // world here worth keeping. That makes it exempt from the
                            // whole backup lifecycle below — not merely "it happens to
                            // have no repository", which is what would otherwise be
                            // true and would put an ephemeral lobby into a player's
                            // restic repo, cost them the upload, and then hand the
                            // next launch a stale lobby to restore.
                            val ephemeral = Core.isMinigame(settings?.env)

                            // Refuse to launch while another device is finishing its
                            // backup: starting now would build a second world from a
                            // snapshot that is still being written. `force` is the UI's
                            // data-loss-warning takeover. A server that will never be
                            // backed up cannot be the one being written, so it does not
                            // queue behind anybody.
                            if (settings != null && deviceId != null && !ephemeral) {
                                val force = obj["force"]?.jsonPrimitive?.booleanOrNull == true
                                BackupManager(context).leaseBlockedReason(settings, deviceId, force)?.let {
                                    throw ServerBackendException.Engine(it)
                                }
                            }
                            engine.start(
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
                                    // off for it, this device is not registered, or it is
                                    // a lobby — all of which mean "host without backups"
                                    // rather than "refuse to host". Null here is also
                                    // what keeps `RestoreWorld` out of the launch plan
                                    // entirely, so the exemption is one decision rather
                                    // than a check repeated at each step.
                                    backupContext = if (settings?.backup != null && deviceId != null && !ephemeral) {
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
     * [raw] if this host may send the user's bearer token to it, else null.
     *
     * Whatever passes here outranks `BuildConfig.API_URL` on every read, and
     * not only this class's: [DeviceRegistry.apiUrl] reads the same key out of
     * the same preferences file. So the value the page stores is where the
     * access token, the device token and every server's settings request go.
     *
     * **Not an allowlist**, deliberately. The channel exists so a build can be
     * pointed at a backend that is not production — `build.gradle.kts` already
     * ships that as `-PapiUrl=…`, on a second domain — and a runtime list that
     * did not know about the build-time default would refuse to restore the
     * very URL the binary was built with. What is checkable without guessing
     * the estate is the shape:
     *
     *  - `https` only. `http` puts the bearer token on the wire in clear, and
     *    that is the whole of what this guard is for.
     *  - a real host, and no `user:pass@` in front of it —
     *    `https://api.gethomerun.app@evil.example` resolves to `evil.example`
     *    and reads as ours to anyone skimming it.
     *  - no query and no fragment. The base is concatenated with a path
     *    (`HomerunApi.get`), so either one lands in the middle of every URL
     *    built from it rather than at the end.
     *
     * That still trusts the page not to name a hostile host; the bundle is
     * Ed25519-signed and this is not the layer that decides who may ship one.
     * It does stop the accidents, and it stops the string `"null"`.
     */
    private fun validApiUrl(raw: String?): String? {
        val trimmed = raw?.trim()?.takeIf { it.isNotEmpty() } ?: return null
        val uri = runCatching { URI(trimmed) }.getOrNull() ?: return null
        if (!"https".equals(uri.scheme, ignoreCase = true)) return null
        if (uri.host.isNullOrBlank()) return null
        if (uri.userInfo != null) return null
        if (uri.query != null || uri.fragment != null) return null
        return trimmed
    }

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
        val stored = SecretStore.read(prefs, KEY_CREDENTIALS) ?: return@runCatching ""
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

    /**
     * Open a sign-in page in a Custom Tab, falling back to whatever browser
     * the device has.
     *
     * A Custom Tab is preferred over a plain `ACTION_VIEW` because it shares
     * the browser's cookie jar — a user already signed in to Google is one tap
     * from done — while still being a real browser as far as Google's
     * embedded-WebView rule is concerned. `FLAG_ACTIVITY_NEW_TASK` is
     * deliberately absent: the tab belongs to our task, so the redirect
     * returns to this activity rather than starting a second one.
     */
    private fun openInBrowser(url: String) {
        val uri = Uri.parse(url)
        val customTab = Intent(Intent.ACTION_VIEW, uri).apply {
            // The Custom Tabs extras, set literally so the class is not a
            // dependency: androidx.browser is not on this host's classpath and
            // a browser that does not understand them just opens normally.
            putExtra("android.support.customtabs.extra.SESSION", null as android.os.Bundle?)
            putExtra("android.support.customtabs.extra.TITLE_VISIBILITY", 1)
        }
        try {
            context.startActivity(customTab)
        } catch (_: Exception) {
            if (!openExternal(url)) throw IllegalStateException("no browser available")
        }
    }

    private fun authFailure(message: String): JsonElement = buildJsonObject {
        put("success", false)
        put("error", message)
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
     * Round-trip time to a region's gateway, in milliseconds.
     *
     * The whole measurement — splitting the address, resolving it, timing the
     * handshake — is `net.regionLatency` in the native host. None of it is
     * here, and that is the point: it *was* here, and in Swift, and both
     * copies were wrong. Each treated the argument as a URL when it is a bare
     * hostname, so every region came back [UNREACHABLE_MS] and the picker
     * ranked a list of ties. `homerun-core::region` has the post-mortem.
     *
     * [UNREACHABLE_MS] stays on this side because it is a *bridge* value, not
     * a measurement: the core answers null for "could not measure", and the
     * number the UI has to receive instead is this protocol's business.
     */
    private suspend fun measureLatency(domain: String?): Int = withContext(Dispatchers.IO) {
        val target = domain?.trim()?.takeIf { it.isNotEmpty() } ?: return@withContext UNREACHABLE_MS
        val measured = runCatching { Core.regionLatency(target) }.getOrNull()
        measured?.takeIf { it.isFinite() && it >= 0 }?.roundToInt() ?: UNREACHABLE_MS
    }

    /**
     * Post a local notification, or do nothing if we may not.
     *
     * From API 33 this needs POST_NOTIFICATIONS at runtime, and it is
     * deliberately not requested here: a permission prompt interrupting
     * whatever the player was doing costs more than the notification is worth.
     * Denied means silence, which is what the contract allows.
     */
    private fun notify(title: String, body: String) = postNotification(context, title, body)

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

        /**
         * The contract's "unreachable", as a latency rather than an error.
         *
         * A number, not a throw, because the UI sorts regions by this and one
         * bad host must not cost the whole list. Note the UI's own
         * "nothing answered" test is `=== Infinity`, which this never trips —
         * `JSON.stringify(Infinity)` is `null`, so no host can send it.
         */
        const val UNREACHABLE_MS = 9999


        const val NOTIFICATION_CHANNEL = "homerun"

        /** Whether the POST_NOTIFICATIONS sheet has ever been shown. */
        const val KEY_PUSH_ASKED = "push-permission-asked"

        /**
         * Create the alerts channel. Idempotent, and called from
         * [HomerunApplication] as well as before every post: a notification
         * naming a channel that does not exist is silently dropped, and a
         * *background* push is drawn by the system tray before any code here
         * has run — the channel must already exist by then.
         */
        fun ensureNotificationChannel(context: Context) {
            val manager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
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
        }

        /**
         * Post a local notification, or do nothing if we may not.
         *
         * From API 33 this needs POST_NOTIFICATIONS at runtime, and it is
         * deliberately not requested here: a permission prompt interrupting
         * whatever the player was doing costs more than the notification is
         * worth. Denied means silence, which is what the contract allows.
         *
         * Companion, like [ensureNotificationChannel], so nothing here needs
         * a router instance — the local `push-notification` channel is its
         * only caller today (a foreground FCM message deliberately shows
         * nothing; see PushMessaging), but the channel and icon rules live in
         * one place for whoever posts next.
         */
        fun postNotification(context: Context, title: String, body: String) {
            runCatching {
                val manager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                if (!manager.areNotificationsEnabled()) return

                ensureNotificationChannel(context)

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
        const val HOST_REVISION = 12

        /**
         * Auth callbacks come home on this prefix rather than one of the
         * deep-link intents, so `lib/deepLink.ts` never sees them — it returns
         * null for anything it does not recognise, which would drop a
         * sign-in silently.
         */
        const val AUTH_CALLBACK_PREFIX = "homerun://auth/"

        /**
         * How long after returning to the foreground to wait for a callback
         * before calling a sign-in dismissed. A Custom Tab gives no cancel
         * signal, so the absence of a redirect is the only evidence there is.
         */
        const val AUTH_CANCEL_GRACE_MS = 700L

        /** Protocol-level, deliberately absent from the channel inventory. */
        private const val READY_METHOD = "__bridge:ready"

        /**
         * Protocol-level like [READY_METHOD], and absent from `channels.ts`
         * for the same reason: it is spoken by a script the host injects, not
         * by the shared UI, so it is not part of the generated contract.
         */
        private const val JS_ERROR_METHOD = "__host:jsError"

        /**
         * Shown to whoever tried to move this host to another backend, so it
         * says what would be accepted rather than what was wrong. Nobody
         * outside a dev build ever sees it — which is the point of it being
         * a sentence and not a stack trace.
         */
        const val API_URL_REFUSED =
            "That backend address was refused. Homerun only talks to an https address " +
                "with a host name, like https://api.gethomerun.app."

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

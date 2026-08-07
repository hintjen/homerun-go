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
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.util.Locale
import java.util.UUID

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
        while (queued.isNotEmpty()) evaluate(queued.removeFirst())
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
                emit("credentials-set")
            } else {
                emit("credentials-error", listOf(JsonPrimitive("Credentials were not an object")))
            }
            null
        },

        // Params are the user's email; the desktop also takes a
        // shouldRemoveDistro flag that has no meaning here.
        "logout" to { _ ->
            prefs.edit().remove(KEY_CREDENTIALS).apply()
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

        // Stable per-install id. Regenerating it would orphan the device's
        // history on the backend, so it is persisted rather than derived.
        "get-device-id" to { _ -> JsonPrimitive(deviceId()) },

        // Nothing listens yet; the device WebSocket server arrives with the
        // server backends. Null is the documented "not running" answer.
        "get-device-ws-port" to { _ -> JsonNull },

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

        // Cold-start deep links (email OTP, magic links) land here once App
        // Links are wired in M2. Nothing captures them yet.
        "deep-link:consume" to { _ -> JsonNull },

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

        "get-system-memory" to { _ -> systemMemory() },
        "get-native-system-memory" to { _ -> systemMemory() },

        // No backend is wired, so nothing can be running. Answering honestly
        // keeps the dashboard's server list coherent instead of hanging it.
        "native-server-active-ids" to { _ -> buildJsonArray { } },
        // BRIDGE-CHANNELS-END
    )

    // ---------------------------------------------------------------------
    // Handler helpers
    // ---------------------------------------------------------------------

    private fun apiUrl(): String = prefs.getString(KEY_API_URL, null) ?: BuildConfig.API_URL

    private fun deviceId(): String =
        prefs.getString(KEY_DEVICE_ID, null) ?: UUID.randomUUID().toString().also {
            prefs.edit().putString(KEY_DEVICE_ID, it).apply()
        }

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

        /** The name JavaScript sees; PROTOCOL.md §3.3 fixes it. */
        const val JS_INTERFACE = "HomerunHost"

        /** Protocol-level, deliberately absent from the channel inventory. */
        private const val READY_METHOD = "__bridge:ready"

        private const val PREFS = "homerun-host"
        private const val KEY_API_URL = "api-url"
        private const val KEY_DEVICE_ID = "device-id"
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

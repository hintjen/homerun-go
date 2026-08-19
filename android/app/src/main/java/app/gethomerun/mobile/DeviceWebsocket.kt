package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.io.File
import java.net.ServerSocket

/**
 * The socket the dashboard talks to this device on.
 *
 * The console, RCON and remote log-fetch in the web dashboard do not go through
 * the API — they connect to `wss://<device-fqdn>`, which the device serves
 * itself. `plans/device-websocket.md` has the whole design and the desktop
 * implementation it is ported from.
 *
 * # What is here so far
 *
 * The device's link, the socket behind it, the certificate it serves with, and
 * the console and RCON it carries.
 *
 * This class owns the link and the lifecycle. The socket and the TLS are the
 * supervisor's, because that is where the console buffer and the command path
 * already are — see `plans/device-websocket.md`.
 *
 * Two ports, both forwarded by the tunnel. The gateway's `:443` reaches the
 * websocket; its `:80` reaches the ACME challenge listener, and that forward is
 * omitted when there is no hostname to prove, because a forward at a listener
 * that never starts looks like the device answered.
 *
 * # Why this is process-scoped
 *
 * Same reason as [ServerHost]. The link takes up to a minute to provision and
 * outlives any page; a WebView that reloads must not take it down, and a second
 * activity must not bring up a second one. The gateway holds one peer slot per
 * device, so two tunnels racing for it is the failure to design out rather than
 * detect.
 */
object DeviceWebsocket {

    private lateinit var appContext: Context
    private var scope: CoroutineScope? = null
    private var tunnel: WireProxy? = null
    private var job: Job? = null

    /** Set once the link is up, and what `get-device-ws-port` will answer. */
    @Volatile
    var port: Int? = null
        private set

    /** The device's public hostname, once the API has named it. */
    @Volatile
    var fqdn: String? = null
        private set

    /**
     * The device row the current link is for.
     *
     * A different account signing in *replaces* that row: [DeviceRegistry]
     * re-registers rather than reuse a device belonging to somebody else. A
     * link still serving the old row is then a link nothing will ever dial,
     * because the dashboard asks the API for this account's device and gets an
     * fqdn no phone is answering. Compared on every [ensure] so the switch is
     * caught there, rather than waiting for a backgrounding to clear it.
     */
    private var linkedDeviceId: String? = null

    /** Read off the link, and passed to the socket that terminates TLS. */
    private var expectsProxyProtocol: Boolean = true

    @Synchronized
    fun init(context: Context, scope: CoroutineScope) {
        appContext = context.applicationContext
        this.scope = scope
    }

    /**
     * Bring the link up, if it is not already.
     *
     * Idempotent and safe to call from anywhere credentials might have just
     * arrived — the login handler calls it, and so would a relaunch that
     * already holds a token. Never awaited: provisioning polls for up to a
     * minute and nothing in the UI should wait on it.
     */
    @Synchronized
    fun ensure(apiUrl: String, userToken: String) {
        // Before the guard below, not after: a link for the wrong device row
        // still leaves the tunnel running, which makes every later `ensure` a
        // no-op — so the stale link would outlive the account that owns it and
        // nothing else would ever notice.
        val current = DeviceRegistry.currentDeviceId()
        val linked = linkedDeviceId
        if (linked != null && current != null && linked != current) {
            Log.i(TAG, "this phone is registered as a different device now — relinking")
            stop()
        }

        if (job?.isActive == true || tunnel?.isRunning == true) return
        val scope = this.scope ?: return
        if (userToken.isBlank()) return

        job = scope.launch {
            runCatching { bringUp(apiUrl, userToken) }
                .onFailure { Log.w(TAG, "the device websocket did not come up: ${it.message}", it) }
        }
    }

    private suspend fun bringUp(apiUrl: String, userToken: String) {
        val deviceId = DeviceRegistry.ensure(apiUrl, userToken)?.deviceId ?: run {
            Log.i(TAG, "no device id yet — nothing to link")
            return
        }

        val link = HomerunApi.awaitDeviceLink(apiUrl, deviceId, userToken) ?: return

        // The ACME challenge listener's port has to be decided *before* the
        // socket starts, because the supervisor binds it during the order and
        // the tunnel has to be forwarding at it by then. This one is chosen
        // here rather than by the OS for that reason — the order and the
        // forward have to agree, and the order happens first.
        val challengePort = freePort()

        // The socket next, then the tunnel that points at it. Asking it to bind
        // 0 rather than picking a number removes the window where something
        // else takes the port between choosing and binding.
        expectsProxyProtocol = link.expectsProxyProtocol
        val bound = startSocket(apiUrl, deviceId, link.fqdn, challengePort) ?: return

        val config = Core.deviceWsTunnelConfig(
            link = link.link,
            // The **TLS** port, not the plaintext one. The gateway sends a
            // ClientHello; forwarding it at the loopback socket the app's own
            // UI uses would fail every handshake.
            httpsTarget = bound.tls,
            // Only when there is a hostname to prove. Without one no order can
            // run, and a forward at a listener that never starts looks like the
            // device answered.
            httpTarget = if (link.fqdn != null) challengePort else null,
        )

        val proxy = WireProxy(appContext, scope ?: return)
        val dir = File(appContext.filesDir, DIRECTORY).apply { mkdirs() }
        proxy.startRendered(
            label = LABEL,
            dir = dir,
            config = config,
            // Nothing user-facing yet: there is no console for a device link
            // the way there is for a server. Logged and no more.
            onLog = { Log.i(TAG, it) },
            onHandshakeFailed = { Log.w(TAG, "the gateway stopped answering this device's link") },
        )

        synchronized(this) {
            tunnel = proxy
            // The plaintext port, because this is what `get-device-ws-port`
            // answers and the shared UI dials `ws://localhost:<port>` for the
            // device it is running on. `wss://<fqdn>` is for other people's.
            port = bound.plaintext
            fqdn = link.fqdn
            linkedDeviceId = deviceId
        }
        Log.i(
            TAG,
            "device link up: fqdn=${link.fqdn ?: "(unnamed)"} " +
                "ws=:${bound.plaintext} tls=:${bound.tls} " +
                "proxyProtocol=${link.expectsProxyProtocol}",
        )
    }

    @Synchronized
    fun stop() {
        job?.cancel()
        job = null
        // The tunnel before the socket: while wireproxy is up the gateway can
        // still hand it a connection, and a forward pointing at a port that has
        // just been released is how a dashboard gets a refusal instead of a
        // clean close.
        tunnel?.stop()
        tunnel = null
        stopSocket()
        port = null
        fqdn = null
        linkedDeviceId = null
    }

    /**
     * Bring the socket up, and answer the port it bound.
     *
     * The supervisor serves it, not this class: it already owns the console
     * buffer the dashboard wants to read and the command path RCON needs, so
     * everything stays in-process rather than crossing the FFI once per console
     * line. See `plans/device-websocket.md`.
     *
     * Port 0 asks the OS to choose. Choosing here instead would leave a window
     * between picking a number and binding it in which something else could
     * take it, and the failure would land on the tunnel rather than here.
     */
    /** The two ports the supervisor bound. */
    private data class Bound(val plaintext: Int, val tls: Int)

    private fun startSocket(
        apiUrl: String,
        deviceId: String,
        fqdn: String?,
        challengePort: Int,
    ): Bound? {
        val config = buildJsonObject {
            put("port", 0)
            put("apiUrl", apiUrl)
            put("jwksUrl", JWKS_URL)
            put("deviceId", deviceId)
            fqdn?.let { put("fqdn", it) }
            put("storageDir", File(appContext.filesDir, "$DIRECTORY/tls").absolutePath)
            put("challengePort", challengePort)
            // Whether the plane in front of us writes a PROXY header. The core
            // answered this off the link; getting it wrong fails every TLS
            // handshake with a message about neither.
            put("expectProxyProtocol", expectsProxyProtocol)
            // Staging in debug builds. Production allows five certificates per
            // hostname per week, and a developer reinstalling all afternoon
            // would spend that before lunch.
            put("acmeStaging", BuildConfig.DEBUG)
        }
        val reply = runCatching { NativeServer.nativeDeviceWsStart(config.toString()) }
            .onFailure { Log.w(TAG, "the socket did not start: ${it.message}", it) }
            .getOrNull()
            ?: return null

        val parsed = runCatching { Json.parseToJsonElement(reply).jsonObject }.getOrNull()
        if (parsed?.get("ok")?.jsonPrimitive?.booleanOrNull != true) {
            Log.w(TAG, "the socket refused to start: $reply")
            return null
        }
        val plaintext = parsed["port"]?.jsonPrimitive?.intOrNull ?: return null
        val tls = parsed["tlsPort"]?.jsonPrimitive?.intOrNull ?: return null
        return Bound(plaintext, tls)
    }

    /**
     * A port nothing else currently holds.
     *
     * Only the ACME challenge listener needs this. The websocket asks the OS
     * for its own port and reports back, but the challenge port has to be in
     * the tunnel config *before* the order runs — the forward and the listener
     * have to agree, and the order happens first. Bound and released rather
     * than hardcoded: a fixed number collides with whatever else on the device
     * wanted it, and fails with less to say about why.
     */
    private fun freePort(): Int = ServerSocket(0).use { it.localPort }

    @Synchronized
    private fun stopSocket() {
        runCatching { NativeServer.nativeDeviceWsStop() }
            .onFailure { Log.w(TAG, "the socket did not stop cleanly: ${it.message}") }
    }

    private const val TAG = "HomerunDeviceWs"
    private const val LABEL = "device"

    /** Beside the servers, not among them — this link belongs to the device. */
    private const val DIRECTORY = "device-ws"

    /**
     * Keycloak's signing keys.
     *
     * One URL, whatever the API host is — which is what the desktop does
     * (`deviceWebsocket/auth.ts`) and is the safer shape anyway: a device that
     * could be told where to find "the" signing keys is a device that can be
     * told to trust somebody else's.
     */
    private const val JWKS_URL =
        "https://auth.gethomerun.app/realms/FractalKeycloak/protocol/openid-connect/certs"
}

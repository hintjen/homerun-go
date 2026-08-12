package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
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
 * **D1: the tunnel, and nothing behind it.** This brings the device's link up
 * and holds it open with the gateway's `:443` forwarded to a port reserved for
 * the websocket server. That server does not exist yet, so connections to it
 * are refused — deliberately, and visibly. What this milestone proves is that a
 * phone can hold a *device* link at all, which is separate machinery from the
 * per-server tunnel and had never been exercised here.
 *
 * The ACME `:80` forward is omitted until there is a challenge listener to
 * answer on it. Forwarding a port at something that was never started is a
 * worse failure than not forwarding it: it looks like the device answered.
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

        // Reserved now and bound by D2. Reserving early means the forward and
        // the eventual listener cannot disagree about the number.
        val wsPort = freePort()
        val config = Core.deviceWsTunnelConfig(
            link = link.link,
            httpsTarget = wsPort,
            // No cert manager yet, so nothing would answer a challenge.
            httpTarget = null,
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
            port = wsPort
            fqdn = link.fqdn
        }
        Log.i(
            TAG,
            "device link up: fqdn=${link.fqdn ?: "(unnamed)"} ws=:$wsPort " +
                "proxyProtocol=${link.expectsProxyProtocol}",
        )
    }

    @Synchronized
    fun stop() {
        job?.cancel()
        job = null
        tunnel?.stop()
        tunnel = null
        port = null
        fqdn = null
    }

    /**
     * A port nothing else holds.
     *
     * Bound and released rather than guessed. The window between releasing and
     * D2 binding it is a race in theory; in practice the alternative — a fixed
     * port — collides with whatever else on the device happened to want it, and
     * fails at the same moment with less to say about why.
     */
    private fun freePort(): Int = ServerSocket(0).use { it.localPort }

    private const val TAG = "HomerunDeviceWs"
    private const val LABEL = "device"

    /** Beside the servers, not among them — this link belongs to the device. */
    private const val DIRECTORY = "device-ws"
}

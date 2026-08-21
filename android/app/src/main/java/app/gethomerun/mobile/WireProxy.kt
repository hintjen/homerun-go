package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.io.File

/**
 * The tunnel that makes a phone-hosted server reachable.
 *
 * # Why this is not optional on mobile
 *
 * A phone on cellular sits behind carrier-grade NAT. There is no router to
 * forward a port on and no UPnP to negotiate with, so unlike desktop there is
 * no fallback: without the tunnel, a server runs perfectly and nobody in the
 * world can join it.
 *
 * wireproxy dials the Homerun gateway as a WireGuard peer and relays whatever
 * the gateway sends to the local server. Players connect to the gateway; the
 * gateway DNATs to a fixed port on the WireGuard interface; wireproxy accepts
 * there and forwards to loopback.
 *
 * # Why this needs no VPN permission
 *
 * wireproxy terminates WireGuard in its own userspace netstack — the
 * `Address` below is a virtual address inside that process, never registered
 * with Android. So there is no TUN device, no `VpnService`, no permission
 * prompt, and none of the Play policy surface a real VPN carries. That is the
 * single most important property of this design.
 *
 * # Shape
 *
 * `supervisor.js` in the `homerun` repo is the spec, and the config is
 * generated to match `wireproxyConfig.ts` byte for byte — the gateway is the
 * same on both sides, so a divergence here is a bug by definition.
 */
class WireProxy(
    private val context: Context,
    private val scope: CoroutineScope,
) {

    /**
     * The gateway's half of the tunnel, from `native_config` on the API.
     *
     * [address] and [allowedIps] are gateway-v2 only: the consolidated
     * gateway runs one shared multi-peer interface and allocates a unique /32
     * per peer, so they cannot be hardcoded the way the legacy per-server
     * tunnel could. Absent means legacy, and the fixed pair applies.
     */
    data class Link(
        val clientPrivateKey: String,
        val gatewayPublicKey: String,
        /** `host:port` — the gateway's WireGuard UDP endpoint. */
        val endpoint: String,
        val address: String? = null,
        val allowedIps: String? = null,
    ) {
        /** The core's field names, which are the API's. */
        fun toJson(): JsonObject = buildJsonObject {
            put("client_privkey", clientPrivateKey)
            put("gateway_pubkey", gatewayPublicKey)
            put("link_address", endpoint)
            address?.let { put("address", it) }
            allowedIps?.let { put("allowed_ips", it) }
        }

        companion object {
            /** Rebuild from the core's shape, as `link.fromServerBody` returns it. */
            fun fromJson(json: JsonObject): Link? {
                fun field(name: String) =
                    json[name]?.jsonPrimitive?.contentOrNull?.takeIf { it.isNotBlank() }
                return Link(
                    clientPrivateKey = field("client_privkey") ?: return null,
                    gatewayPublicKey = field("gateway_pubkey") ?: return null,
                    endpoint = field("link_address") ?: return null,
                    address = field("address"),
                    allowedIps = field("allowed_ips"),
                )
            }
        }
    }

    private var process: Process? = null
    private var pumpJob: Job? = null

    val isRunning: Boolean get() = process?.isAlive == true

    /** The binary, or null if this build ships none for the device's ABI. */
    fun binary(): File? =
        File(context.applicationInfo.nativeLibraryDir, BINARY).takeIf { it.canExecute() }

    /**
     * Render the config, in `homerun-core`.
     *
     * This used to be a list of strings built here, mirroring the desktop's
     * generator by hand. It is one place now because the gateway is one thing:
     * every `ListenPort` in it is fixed by what the gateway DNATs to, and a
     * host that "corrected" one to match its local port would produce a config
     * that loads, connects, and is unreachable. `homerun-core::wireproxy` has
     * that written down and tested byte-for-byte.
     */
    fun render(
        link: Link,
        minecraftPort: Int,
        exposure: String = "java",
        voiceChatPort: Int? = null,
    ): String =
        Core.renderWireproxy(
            link = link.toJson(),
            port = minecraftPort,
            exposure = exposure,
            // Deliberately not passed. The core defaults it to the gateway's
            // 19132, which is also Geyser's default and what
            // `minecraft::crossplay::config` writes — and Android hosts one
            // server at a time (`multipleRunningServers: false`), so there is
            // no second Geyser to collide with and no port to negotiate. A
            // probe here would have to agree with that config file, and two
            // places choosing a port is how they come to disagree.
            geyserPort = null,
            voiceChatPort = voiceChatPort,
        )

    /**
     * Write the config and spawn the tunnel.
     *
     * Returns once the process is up, which is not the same as the tunnel
     * being usable — a handshake takes a few seconds and can fail. That is
     * what [onHandshakeFailed] is for; it fires when the gateway has stopped
     * answering, which in practice means its keys were regenerated and these
     * credentials are dead.
     *
     * @throws ServerBackendException.Engine if the tunnel cannot be started.
     */
    fun start(
        serverId: String,
        dir: File,
        link: Link,
        minecraftPort: Int,
        exposure: String = "java",
        voiceChatPort: Int? = null,
        onLog: (String) -> Unit,
        onHandshakeFailed: () -> Unit,
    ) = startRendered(
        serverId,
        dir,
        render(link, minecraftPort, exposure, voiceChatPort),
        onLog,
        onHandshakeFailed,
    )

    /**
     * Start a tunnel from a config that is already rendered.
     *
     * The device websocket needs this: its forwards are the gateway's `:443`
     * and `:80` rather than a game's ports, so it renders through
     * [Core.deviceWsTunnelConfig] instead of [render]. Everything below —
     * spawning, the private-key file mode, the handshake watch, the stop — is
     * the same, and duplicating it for a second caller is how the two would
     * come to disagree about which of them holds the peer slot.
     *
     * [label] appears in the log lines and is a server id for the server
     * tunnel, `"device"` for this one.
     */
    fun startRendered(
        label: String,
        dir: File,
        config: String,
        onLog: (String) -> Unit,
        onHandshakeFailed: () -> Unit,
    ) {
        stop()

        val binary = binary() ?: throw ServerBackendException.Engine(
            "This build ships no tunnel, so the server would not be reachable from outside " +
                "this device."
        )

        // The config holds a private key. Keep it inside app-private storage
        // and readable only by us — the same reason the desktop writes it into
        // the server directory rather than a temp path.
        val conf = File(dir, "wireproxy.conf")
        conf.writeText(config)
        runCatching {
            conf.setReadable(false, false)
            conf.setReadable(true, true)
        }

        val started = runCatching {
            ProcessBuilder(listOf(binary.absolutePath, "-c", conf.absolutePath))
                .directory(dir)
                .redirectErrorStream(true)
                .start()
        }.getOrElse { err ->
            throw ServerBackendException.Engine(
                "The tunnel could not be started: ${err.message ?: "unknown error"}"
            )
        }

        process = started
        pump(label, started, onLog, onHandshakeFailed)
        Log.i(TAG, "wireproxy up for $label")
    }

    /**
     * Watch the tunnel's output for the one thing worth reacting to.
     *
     * wireproxy retries a failed handshake forever, so a dead credential set
     * looks identical to a slow network until you count. Ten consecutive
     * failures is the desktop's threshold and this keeps it — a successful
     * response resets the count.
     */
    private fun pump(
        label: String,
        running: Process,
        onLog: (String) -> Unit,
        onHandshakeFailed: () -> Unit,
    ) {
        pumpJob?.cancel()
        pumpJob = scope.launch(Dispatchers.IO) {
            // Opaque state owned here and handed back each line. The
            // threshold, and the fact a success resets it, live in
            // `homerun-core::state` so they cannot drift from the desktop's.
            var watch: JsonObject? = null
            // Everything here must be caught. This coroutine runs on a scope
            // whose SupervisorJob stops siblings being cancelled — it does NOT
            // stop an unhandled exception reaching the default handler, which
            // kills the whole app. Stopping the tunnel closes this stream
            // under a blocked readLine, and the InterruptedIOException that
            // produces took the process down with it once already.
            try {
                running.inputStream.bufferedReader().useLines { lines ->
                    for (raw in lines) {
                        val line = raw.trim()
                        if (line.isEmpty()) continue
                        Log.d(TAG, line)

                        val verdict = Core.observeHandshake(watch, line)
                        watch = verdict.watch
                        if (verdict.giveUp) {
                            Log.w(TAG, "$label: the gateway stopped answering")
                            onLog(
                                "[Homerun] The connection to the Homerun gateway could " +
                                    "not be established, so players cannot reach this server."
                            )
                            runCatching { onHandshakeFailed() }
                        } else if (verdict.recovered) {
                            onLog("[Homerun] Connection to the Homerun gateway restored.")
                        }
                    }
                }
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (err: Throwable) {
                // The stream ending abruptly *is* how a stopped tunnel looks
                // from here. Worth a line, never worth a crash.
                Log.d(TAG, "tunnel output ended: ${err.message}")
            }
            Log.i(TAG, "wireproxy exited for $label")
        }
    }

    /**
     * Stop the tunnel. Unlike the JVM there is nothing to save, so this does
     * not wait politely — a lingering wireproxy would hold the gateway's peer
     * slot against the next start.
     */
    fun stop() {
        pumpJob?.cancel()
        pumpJob = null
        process?.let { running ->
            runCatching { running.destroy() }
            if (!running.waitFor(STOP_SECONDS, java.util.concurrent.TimeUnit.SECONDS)) {
                runCatching { running.destroyForcibly() }
            }
        }
        process = null
    }

    private companion object {
        const val TAG = "HomerunTunnel"

        /**
         * Ships in `jniLibs` and is exec'd from `nativeLibraryDir` — the only
         * directory API 29+ permits exec from, and the packager only puts
         * `lib*.so` there. It is a Go executable, not a library.
         */
        const val BINARY = "libwireproxy.so"

        const val STOP_SECONDS = 5L
    }
}

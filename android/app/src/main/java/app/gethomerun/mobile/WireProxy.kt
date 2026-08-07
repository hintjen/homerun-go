package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
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
    )

    private var process: Process? = null
    private var pumpJob: Job? = null

    val isRunning: Boolean get() = process?.isAlive == true

    /** The binary, or null if this build ships none for the device's ABI. */
    fun binary(): File? =
        File(context.applicationInfo.nativeLibraryDir, BINARY).takeIf { it.canExecute() }

    /**
     * Render the config the desktop would render.
     *
     * The gateway-facing `ListenPort`s are **fixed**. The gateway always DNATs
     * player traffic to 25565 (and voice to 24454) on the WireGuard interface,
     * whatever local port we actually bound — only `Target` follows that.
     * Changing these numbers silently makes the server unreachable.
     */
    fun render(link: Link, minecraftPort: Int, voiceChatPort: Int? = null): String {
        val lines = mutableListOf(
            "[Interface]",
            "PrivateKey = ${link.clientPrivateKey}",
            "Address = ${link.address ?: "10.0.0.2/24"}",
            "MTU = 1280",
            "",
            "[Peer]",
            "PublicKey = ${link.gatewayPublicKey}",
            "Endpoint = ${link.endpoint}",
            "AllowedIPs = ${link.allowedIps ?: "10.0.0.1/32"}",
            "PersistentKeepalive = 30",
            "",
            "[TCPServerTunnel]",
            "ListenPort = 25565",
            "Target = 127.0.0.1:$minecraftPort",
        )
        // Bedrock and crossplay need [UDPServerTunnel] too — that section is
        // why we build the fork rather than upstream wireproxy. Neither is
        // reachable on Android yet, so voice chat is the only UDP case here.
        if (voiceChatPort != null) {
            lines += listOf(
                "",
                "[UDPServerTunnel]",
                "ListenPort = 24454",
                "Target = 127.0.0.1:$voiceChatPort",
            )
        }
        return lines.joinToString("\n") + "\n"
    }

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
        conf.writeText(render(link, minecraftPort))
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
        pump(serverId, started, onLog, onHandshakeFailed)
        Log.i(TAG, "wireproxy up for $serverId -> ${link.endpoint}")
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
        serverId: String,
        running: Process,
        onLog: (String) -> Unit,
        onHandshakeFailed: () -> Unit,
    ) {
        pumpJob?.cancel()
        pumpJob = scope.launch(Dispatchers.IO) {
            var failures = 0
            var signalled = false
            running.inputStream.bufferedReader().useLines { lines ->
                for (raw in lines) {
                    val line = raw.trim()
                    if (line.isEmpty()) continue
                    Log.d(TAG, line)

                    when {
                        line.contains(HANDSHAKE_TIMEOUT) -> {
                            failures++
                            if (failures >= HANDSHAKE_FAIL_THRESHOLD && !signalled) {
                                signalled = true
                                Log.w(TAG, "$serverId: handshake failed $failures times")
                                onLog(
                                    "[Homerun] The connection to the Homerun gateway could not " +
                                        "be established, so players cannot reach this server."
                                )
                                onHandshakeFailed()
                            }
                        }
                        line.contains(HANDSHAKE_OK) -> {
                            if (failures > 0) Log.i(TAG, "$serverId: handshake recovered")
                            if (failures >= HANDSHAKE_FAIL_THRESHOLD) {
                                onLog("[Homerun] Connection to the Homerun gateway restored.")
                            }
                            failures = 0
                        }
                    }
                }
            }
            Log.i(TAG, "wireproxy exited for $serverId")
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

        const val HANDSHAKE_TIMEOUT = "Handshake did not complete after 5 seconds"
        const val HANDSHAKE_OK = "Received handshake response"
        const val HANDSHAKE_FAIL_THRESHOLD = 10
        const val STOP_SECONDS = 5L
    }
}

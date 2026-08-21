package app.gethomerun.mobile

import android.content.Context
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.async
import kotlinx.coroutines.launch
import java.io.File

/**
 * A server's gateway tunnel, for the whole life of one launch.
 *
 * Both Android backends need this and it is not a thin wrapper: the rules
 * below were learned separately on three platforms, and a second copy of them
 * is a second place to get them subtly wrong. Pumpkin ran without a tunnel for
 * exactly as long as it took someone to try connecting to one.
 *
 * The shape a caller uses:
 *
 *   begin(config.resolveTunnel)   // early — the gateway takes up to a minute
 *   ... spawn, wait for ready ...
 *   open(serverId, dir, port)     // before announcing `running`
 *   ... the server exits ...
 *   shutdown()
 *
 * [begin] and [open] are deliberately separate. The gateway provisions the
 * peer asynchronously and polls for up to a minute, so a launch that resolved
 * the link only when it needed it would add that minute to every start —
 * begun early, it overlaps with the download, the world restore and the engine
 * booting, exactly as the desktop does it.
 */
class TunnelSession(
    context: Context,
    private val scope: CoroutineScope,
    /**
     * Write a line to the server's console.
     *
     * Both backends write into the supervisor's buffer rather than emitting an
     * event, so the tunnel's narrative interleaves with the server's output in
     * the order it actually happened.
     */
    private val note: (String, String) -> Unit,
    /**
     * The backend's `onNetworkError`, read at the moment it is needed.
     *
     * A getter rather than the callback itself: `ServerHost` assigns these
     * after constructing a backend, so capturing the value here would capture
     * null and the UI would never learn why its server stopped.
     */
    private val onNetworkError: () -> ((String, String) -> Unit)?,
    /** The backend's own stop, so a failed tunnel takes the server with it. */
    private val stopServer: suspend (String, Boolean) -> Unit,
) {
    private val wireProxy = WireProxy(context, scope)
    private var job: Deferred<WireProxy.Link?>? = null

    private val lifecycle: Core.Lifecycle get() = ServerHost.lifecycle

    /** Whether this launch has a tunnel at all. Null resolve means it does not. */
    fun begin(resolve: (suspend () -> WireProxy.Link?)?) {
        job = resolve?.let { r -> scope.async { runCatching { r() }.getOrNull() } }
    }

    /**
     * Drop a pending resolve without touching a running tunnel.
     *
     * For a launch that gave up before the server was reachable — the gateway
     * poll would otherwise outlive it and hold a peer slot the next start
     * needs.
     */
    fun cancel() {
        job?.cancel()
        job = null
    }

    /** Everything down: the pending resolve and the tunnel itself. */
    fun shutdown() {
        cancel()
        wireProxy.stop()
    }

    /**
     * Bring up the gateway tunnel. Failing to is fatal to the launch.
     *
     * A server nobody can reach is not a working server, so it is stopped
     * rather than left running and looking healthy — the desktop's
     * `pollAndProvisionWireproxy` throws when the config never arrives and
     * `server-started`'s catch stops the server; this matches that exactly.
     * Both paths also emit `native-server-network-error`, because a clean stop
     * with no explanation is indistinguishable from the user's own Stop.
     *
     * A no-op when there is no tunnel to open, so a caller need not ask.
     */
    suspend fun open(serverId: String, dir: File, port: Int, exposure: String = "java") {
        val pending = job ?: return
        note(serverId, "[Homerun] Connecting to the Homerun gateway...")

        val link = runCatching { pending.await() }.getOrNull()
        job = null

        if (link == null) {
            fail(
                serverId, PROVISIONING,
                "Failed to establish network tunnel: the gateway did not provide one.",
            )
        }

        runCatching {
            wireProxy.start(
                serverId = serverId,
                dir = dir,
                link = link,
                minecraftPort = port,
                // `java` forwards one TCP port and nothing else, which is right
                // for every server except a crossplay one — that needs the
                // Bedrock UDP forward as well, and a tunnel without it is a
                // server that starts, logs nothing wrong, and that no Bedrock
                // player can reach.
                exposure = exposure,
                onLog = { line -> note(serverId, line) },
                // The tunnel came up and then stopped being answered — the
                // gateway regenerating its keys is the usual cause, and the
                // credentials we hold are permanently dead. Same verdict, but
                // reported as `handshake` so the UI can say so.
                onHandshakeFailed = {
                    scope.launch {
                        runCatching {
                            stopForNetworkError(serverId, HANDSHAKE)
                        }
                    }
                },
            )
        }.onFailure { err ->
            fail(
                serverId, PROVISIONING,
                "Failed to establish network tunnel: ${err.message ?: "it could not be started"}.",
            )
        }

        note(serverId, "[Homerun] Connected to the Homerun gateway.")
    }

    private suspend fun fail(serverId: String, kind: String, message: String): Nothing {
        note(serverId, "[Homerun] $message Stopping server.")
        stopForNetworkError(serverId, kind)
        throw ServerBackendException.Engine(message)
    }

    /**
     * Stop a server because its tunnel failed.
     *
     * The event goes out *before* the stop so the UI has the reason in hand by
     * the time the card flips — it stops through the normal clean path, so
     * otherwise this is indistinguishable from the user pressing Stop.
     */
    suspend fun stopForNetworkError(serverId: String, kind: String) {
        Log.w(TAG, "$serverId: tunnel failed ($kind) — stopping")
        onNetworkError()?.invoke(serverId, kind)

        // Through the core, exactly as a stop from the bridge would be. This
        // is a stop somebody asked for — Homerun did, on the player's behalf —
        // and recording the intent is what keeps the exit from being reported
        // as a crash, which would also skip the on-stop backup.
        val verdict = lifecycle.stopRequested(serverId)
        try {
            if (verdict.verdict != "notRunning") {
                runCatching { stopServer(serverId, verdict.verdict == "graceful") }
            }
        } finally {
            lifecycle.callFinished(serverId)
        }
    }

    companion object {
        private const val TAG = "HomerunTunnelSession"

        /**
         * The two `native-server-network-error` kinds the contract defines.
         *
         * Here rather than on a backend: they describe what happened to a
         * tunnel, and both backends have one.
         */
        const val PROVISIONING = "provisioning"
        const val HANDSHAKE = "handshake"
    }
}

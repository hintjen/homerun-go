package app.gethomerun.mobile

import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.io.File
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * Being found on the local network: the toggle, and the announcement that puts a
 * server in Minecraft's "local network" list.
 *
 * # One switch, two effects
 *
 * "Expose Minecraft server to your local network" is a checkbox the shared UI
 * shows for every native Java server, backed by `get-` and
 * `set-native-local-network`. Until this file existed Android answered
 * `false` and refused the set, so the checkbox was decoration — while a
 * Pumpkin server on the same phone was listening on every interface anyway,
 * because nothing set Pumpkin's address and its default is `0.0.0.0`.
 *
 * Now it means what it says on the desktop and more. **Off**, the default:
 * every server binds loopback and is reachable through the gateway tunnel and
 * nowhere else. **On**: it binds `0.0.0.0` so a device on the same Wi-Fi can
 * connect to the phone's address directly, *and* it is announced so that
 * device does not need to know the address. Which decision is the core's
 * (`minecraft.lan.bind`); the socket is this file's.
 *
 * # How a server gets into the list
 *
 * A Java client "scanning for games on your local network" listens on the
 * multicast group `224.0.2.60:4445` and lists whatever shouts at it. Only the
 * client's own integrated server ("Open to LAN") ever does — a dedicated
 * Paper or vanilla server never announces itself — so this host sends the
 * announcement on the JVM's behalf. The bytes are the core's (`minecraft.lan.announce`),
 * the same ones Pumpkin's own broadcaster sends, so the two cannot drift.
 * Pumpkin sends its own when the flag is on; PowerNukkitX needs none, because
 * a Bedrock client does the shouting and the server merely answers.
 *
 * # Why a multicast lock
 *
 * Android's Wi-Fi driver drops multicast and broadcast frames it was not
 * asked to keep, to save battery. [WifiManager.MulticastLock] asks, for the
 * whole device — which is what covers the child process: the JVM's replies
 * and PowerNukkitX's answers to a Bedrock client's broadcast ping arrive at
 * the driver first. Held only while a server is exposed and running, because
 * it costs battery exactly as advertised.
 */
object LocalNetwork {

    /** Beside the world, so it is deleted with the server. */
    private const val FILE = "homerun-local-network.json"

    private const val TAG = "HomerunLan"

    fun isEnabled(context: Context, serverId: String): Boolean = runCatching {
        val file = File(directory(context, serverId), FILE)
        file.exists() && file.readText().contains("\"enabled\":true")
    }.getOrDefault(false)

    /** Takes effect on the next start; the UI only offers it while stopped. */
    fun set(context: Context, serverId: String, enabled: Boolean) {
        val dir = directory(context, serverId).apply { mkdirs() }
        File(dir, FILE).writeText("""{"enabled":$enabled}""")
    }

    private fun directory(context: Context, serverId: String): File =
        File(context.filesDir, "servers/${requireValidServerId(serverId)}")

    /**
     * The announcement for one running server: what the JVM cannot say for itself.
     *
     * Sends the core's datagram every `intervalMs` from a socket bound to any
     * port — the client cares about the *contents*, which carry the game
     * port — and holds the multicast lock for as long as it runs. [stop] is
     * idempotent; a launch that never got as far as running has nothing to
     * stop.
     */
    class Announcer(private val context: Context, private val scope: CoroutineScope) {
        private var job: Job? = null
        private var lock: WifiManager.MulticastLock? = null

        fun start(serverId: String, motd: String, port: Int) {
            stop()
            val announcement = runCatching { Core.lanAnnounce(motd, port) }
                .onFailure { Log.w(TAG, "$serverId: no announcement: ${it.message}") }
                .getOrNull() ?: return

            lock = runCatching {
                (context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager)
                    .createMulticastLock("homerun-lan")
                    .apply { setReferenceCounted(false); acquire() }
            }.onFailure { Log.w(TAG, "$serverId: no multicast lock: ${it.message}") }.getOrNull()

            job = scope.launch(Dispatchers.IO) {
                val group = InetAddress.getByName(announcement.group)
                val bytes = announcement.payload.toByteArray(Charsets.UTF_8)
                DatagramSocket().use { socket ->
                    socket.broadcast = true
                    Log.i(TAG, "$serverId: announcing on ${announcement.group}:${announcement.port} every ${announcement.intervalMs} ms")
                    while (true) {
                        runCatching { socket.send(DatagramPacket(bytes, bytes.size, group, announcement.port)) }
                            .onFailure { Log.w(TAG, "$serverId: announcement not sent: ${it.message}") }
                        delay(announcement.intervalMs)
                    }
                }
            }
        }

        fun stop() {
            job?.cancel()
            job = null
            runCatching { lock?.takeIf { it.isHeld }?.release() }
            lock = null
        }
    }
}

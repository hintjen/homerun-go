package app.gethomerun.mobile

import java.time.Instant

/**
 * A Minecraft server this device can host.
 *
 * The mobile analogue of the desktop app's `nativeServerManager`. The bridge
 * layer talks only to this interface, so the `native-server-*` channels are
 * implemented once regardless of engine.
 *
 * Android has two implementations:
 *  - [PumpkinBackend]    in-process Rust via JNI (shared with iOS)
 *  - [JavaServerBackend] a real JVM subprocess
 *
 * The JVM path is why Android gets full parity, and it carries a hard
 * platform constraint: since API 29, executables cannot be run from writable
 * storage, so the bundled JRE must ship inside the APK as jniLibs and land in
 * `nativeLibraryDir`. Server jars are data the JVM reads, so those may still
 * be downloaded at runtime.
 */
interface ServerBackend {
    /** Engine identity, matching `ServerBackendKind` in the UI's capabilities. */
    val kind: String

    // --- Lifecycle ---

    /** Create on-disk state. Must be idempotent. */
    fun create(serverId: String)

    /** Delete a server and everything it owns. Must refuse while running. */
    fun delete(serverId: String)

    /**
     * Start and return once the server accepts connections, or throw.
     *
     * Long-running by design — the bridge must not time out (PROTOCOL.md §5).
     * Report progress through [onLog] rather than returning early.
     */
    suspend fun start(serverId: String, config: ServerConfig)

    /**
     * Stop gracefully: save the world, then exit. Send the console `stop`
     * command and wait — killing the process risks the world save.
     */
    suspend fun stop(serverId: String)

    /** Ids currently running. One-running-server hosts return 0 or 1. */
    val runningServerIds: List<String>

    // --- Introspection ---

    fun status(serverId: String): ServerState
    fun players(serverId: String): PlayerRoster?
    fun uptime(serverId: String): Instant?
    fun memoryUsage(serverId: String): MemoryUsage?
    fun cpuUsage(serverId: String): Double?
    fun port(serverId: String): Int?

    /** Console lines since [cursor], plus a cursor for the next call. */
    fun logs(serverId: String, cursor: Int): LogSlice

    /**
     * Recent samples for the metrics graphs, oldest first. Empty when the
     * backend cannot sample — an empty graph is honest, a fabricated one is
     * not.
     */
    fun perfHistory(serverId: String): List<PerfSample> = emptyList()

    /**
     * Run a console command. The JVM backend can use RCON; Pumpkin dispatches
     * in-process. Either way the reply arrives on `native-server-rcon-response`.
     */
    suspend fun command(serverId: String, command: String)

    // --- Events ---

    /** Set by the bridge layer. Backends must dispatch on the main thread. */
    var onStateChanged: ((String, ServerState) -> Unit)?
    var onLog: ((String, String) -> Unit)?
    var onPlayersChanged: ((String) -> Unit)?

    /**
     * The network tunnel failed, and the server is being stopped for it.
     *
     * `kind` is `provisioning` (never came up) or `handshake` (came up, then
     * the gateway stopped answering). Both stop the server through the normal
     * clean path, so without this the UI just sees it flip to stopped with no
     * explanation — the shared UI already toasts this event, worded per kind.
     */
    var onNetworkError: ((String, String) -> Unit)?
}

enum class ServerState(val wire: String) {
    STOPPED("stopped"),
    STARTING("starting"),
    RUNNING("running"),
    STOPPING("stopping"),
    CRASHED("crashed"),
}

data class ServerConfig(
    val name: String,
    /**
     * Heap ceiling. Android will kill the whole app under memory pressure,
     * not just the server, so size this against the device rather than the
     * desktop defaults.
     */
    val memoryMb: Int,
    /**
     * Minecraft version to host. Null means the latest release — the same
     * meaning the desktop gives an absent `VERSION`.
     *
     * The UI does not send this; the bridge reads it from the backend at
     * launch ([HomerunApi.serverSettings]), so a version changed on the web
     * dashboard takes effect on the next start.
     */
    val version: String? = null,
    /** `vanilla`, `paper`, … Which server jar [ServerJar] fetches. */
    val loader: String = "vanilla",
    /**
     * Resolves the gateway tunnel, or null when this host cannot tunnel.
     *
     * A function rather than a value for two reasons. It is slow — the
     * gateway provisions the peer asynchronously and the poll runs up to a
     * minute — so the backend runs it *alongside* the server booting instead
     * of before it. And it closes over the user's access token, which stays
     * in the bridge layer and never becomes backend state that could reach
     * the server process's environment.
     */
    val resolveTunnel: (suspend () -> WireProxy.Link?)? = null,
    /**
     * Forwarded into the server process's environment, so it must never carry
     * anything secret — no tokens, no credentials.
     */
    val extra: Map<String, Any> = emptyMap(),
)

data class PlayerRoster(
    val players: List<Player>,
    val max: Int?,
) {
    data class Player(val name: String, val uuid: String?)
}

data class MemoryUsage(val usedKb: Int?, val maxMb: Int?)

/** [cursor] is per-run, not durable across restarts. */
data class LogSlice(val lines: List<String>, val cursor: Int)

/** One point on the metrics graphs. Null fields render as "unavailable". */
data class PerfSample(
    val t: Long,
    val memUsedMb: Int?,
    val cpuPercent: Int?,
    val playerCount: Int?,
)

sealed class ServerBackendException(message: String) : Exception(message) {
    class NotFound(id: String) : ServerBackendException("No server with id $id")
    class AlreadyRunning(id: String) : ServerBackendException("Server $id is already running")
    class NotRunning(id: String) : ServerBackendException("Server $id is not running")
    class PortUnavailable(port: Int) : ServerBackendException("Port $port is already in use")

    /** Surfaced to players, so phrased for them. */
    class AnotherServerRunning(id: String) : ServerBackendException(
        "Another server is already running. Stop it first — this device can host one at a time."
    )

    class Engine(message: String) : ServerBackendException(message)
}

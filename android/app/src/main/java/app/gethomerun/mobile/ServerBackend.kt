package app.gethomerun.mobile

import kotlinx.serialization.json.JsonObject
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
     * Stop the server, now.
     *
     * [graceful] is the core's verdict, not a preference: true when the engine
     * has reached a console that can hear `stop` and save its world, false
     * when it has not — a server still generating terrain has nothing saved to
     * protect, and waiting for it to finish starting so it can be asked
     * politely is not a stop. See `homerun-core::lifecycle::StopVerdict`.
     */
    suspend fun stop(serverId: String, graceful: Boolean)

    /**
     * Ids currently running. One-running-server hosts return 0 or 1.
     *
     * **Not** the answer to "does this device own this server" — a server
     * that is still downloading a jar, or still saving its world on the way
     * down, is this device's and is not running. That question belongs to
     * `homerun-core::lifecycle` (reached through [Core.Lifecycle], held by
     * [ServerHost]), and answering it from here is what let the UI's reconcile
     * loop reprovision the gateway underneath a live launch. A backend reports
     * what its engine is doing; it does not adjudicate ownership.
     */
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
    /**
     * Third argument: this stop is about to be followed by a backup.
     *
     * It rides along with the state change because it must reach the API on
     * the *same* `stopped` ack — that ack is what opens the backup lease, and
     * a separate call afterwards would race a co-host's launch.
     */
    var onStateChanged: ((String, ServerState, Boolean) -> Unit)?
    var onLog: ((String, String) -> Unit)?
    var onPlayersChanged: ((String) -> Unit)?

    /**
     * The on-stop backup is over, however it went.
     *
     * The counterpart to [onStateChanged]'s third argument, and the reason it
     * has to exist: that flag says a backup is *starting*, on a state change
     * that says the server has *stopped*. Nothing else afterwards marks this
     * device idle, and until something does, a host cannot know when it is safe
     * to let the process be reclaimed — which on Android means killing an
     * upload of the session that just finished.
     *
     * Backends with no backup path leave it unused.
     */
    var onBackupFinished: ((String) -> Unit)?

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
     * The server's `environment_variables`, as the API returned them.
     *
     * Every world setting a player chose — game mode, difficulty, seed, ops,
     * whitelist — arrives here and reaches the world only by being written
     * into `server.properties` and friends before the JVM starts
     * ([ServerSettingsWriter]). Null means the settings could not be read, and
     * the server's own defaults apply.
     *
     * Note this is **not** [extra]: nothing in here is forwarded into the
     * server process's environment. It is read, resolved by the core, and
     * written to files.
     */
    val settingsEnv: JsonObject? = null,
    /**
     * The API's game type verbatim (`java`, `native-crossplay`, …).
     *
     * Needed unreduced because crossplay decides online mode.
     */
    val gameType: String = "java",
    /**
     * Everything the backup lifecycle needs, or null when this server has no
     * repository (the feature is off, or it has no volume yet).
     *
     * Deliberately **not** in [extra]: that map is forwarded into the server
     * process's environment, and this carries the repository password.
     */
    val backupContext: BackupContext? = null,
    /**
     * Forwarded into the server process's environment, so it must never carry
     * anything secret — no tokens, no credentials.
     */
    val extra: Map<String, Any> = emptyMap(),
)

/**
 * What a backup needs that only the launch path knows.
 *
 * The device id is the identity restic records as the snapshot hostname, and
 * the API resolves `pushed_by` from it — so it must be the same id the API
 * issued, not a locally-generated one.
 */
data class BackupContext(
    val settings: HomerunApi.ServerSettings,
    val deviceId: String,
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
    /**
     * Fractional on purpose. An idle Minecraft server sits well under one
     * percent, and truncating that to an `Int` drew a flat zero line for a
     * server that was demonstrably working — 0.6 % measured, 0 % shown.
     */
    val cpuPercent: Double?,
    val playerCount: Int?,
)

/**
 * Check a `serverId` before anything builds a path out of it.
 *
 * The id arrives verbatim from JavaScript and both backends spell their
 * storage `filesDir/servers/$serverId`, so `../..` is the app's private root —
 * and `native-server-delete` would take `shared_prefs` (credentials and the
 * device token), the unpacked JRE and every world with it. The membership
 * check against the lifecycle's active ids is not a guard: any invented id
 * passes it, because it is asking whether the id is *busy*, not whether it is
 * real.
 *
 * An allowlist on the id rather than a canonical-path assertion at each sink,
 * because the id is a path segment in more places than the filesystem —
 * `/api/server/<id>/`, restic's recorded basename
 * (`homerun_core::backup::recorded_basename`), `cacheDir/restore-<id>`. A
 * containment check has to be repeated at every one of those and is missing
 * from whichever is added next; a rule about the id itself holds everywhere at
 * once.
 *
 * The set is what an API-issued id can already be: anything outside it would
 * corrupt the URLs this host builds from the same string long before it
 * reached a directory. A leading dot is refused separately — the character is
 * legal in the middle of an id, and refusing it only at the front is what
 * rules out `.` and `..` themselves.
 *
 * Nothing exploits this today: the page is the bundle inside the APK. It goes
 * live the moment that bundle comes over the air (`docs/ota-bundles.md`), and
 * any XSS in the shared UI reaches it now.
 */
fun requireValidServerId(serverId: String): String {
    if (!SERVER_ID.matches(serverId) || serverId.startsWith(".")) {
        throw ServerBackendException.InvalidId()
    }
    return serverId
}

/**
 * Matched with `matches`, which is a whole-string test — deliberately not an
 * `^…$` pattern, whose `$` also matches before a trailing newline and would
 * admit `"s1\n../.."`.
 */
private val SERVER_ID = Regex("[A-Za-z0-9._-]{1,128}")

sealed class ServerBackendException(message: String) : Exception(message) {
    class NotFound(id: String) : ServerBackendException("No server with id $id")
    class AlreadyRunning(id: String) : ServerBackendException("Server $id is already running")
    class NotRunning(id: String) : ServerBackendException("Server $id is not running")
    class PortUnavailable(port: Int) : ServerBackendException("Port $port is already in use")

    /** Surfaced to players, so phrased for them. */
    class AnotherServerRunning(id: String) : ServerBackendException(
        "Another server is already running. Stop it first — this device can host one at a time."
    )

    /**
     * A `serverId` this host will not build a path from — see
     * [requireValidServerId].
     *
     * Thrown rather than swallowed: the bridge answers the invoke with this
     * message, and a handler that returned quietly would leave the UI's promise
     * pending for the life of the page. The id is deliberately not in the text
     * — it came from the page, and this is read by a player.
     */
    class InvalidId : ServerBackendException(
        "Homerun cannot open that server — its id is not one this app recognises."
    )

    class Engine(message: String) : ServerBackendException(message)
}

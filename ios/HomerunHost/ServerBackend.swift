import Foundation

/// A Minecraft server this device can host.
///
/// The mobile analogue of the desktop app's `nativeServerManager`. The bridge
/// layer talks only to this protocol, so the `native-server-*` channels are
/// implemented once regardless of which engine is underneath.
///
/// iOS has exactly one implementation (`PumpkinBackend`, in-process Rust FFI)
/// because the platform cannot spawn processes. Android adds a JVM backend.
/// Nothing here may assume a child process, a pid, or stdio pipes.
///
/// Main-actor isolated: every caller is the bridge, which is main-actor, and
/// the event callbacks below must arrive on the main queue anyway. Making
/// that explicit is what stops a backend from touching this state off the
/// thread the UI reads it from.
@MainActor
protocol ServerBackend: AnyObject {
    /// Engine identity, matching `ServerBackendKind` in the UI's capabilities.
    var kind: String { get }

    // MARK: Lifecycle

    /// Create on-disk state for a server. Must be idempotent.
    func create(serverId: String) throws

    /// Delete a server and everything it owns. Must refuse while running.
    func delete(serverId: String) throws

    /// Start a server and return once it is accepting connections, or throw.
    ///
    /// Long-running by design — the bridge must not time out (PROTOCOL.md §5).
    /// Report progress through `onLog`, not by returning early.
    func start(serverId: String, config: ServerConfig) async throws

    /// Stop a running server.
    ///
    /// `graceful` asks the engine to save and exit, and is what a stop after
    /// the console is up must use — a forced end risks the world save, which
    /// also breaks the on-stop backup. It is false only when the engine cannot
    /// hear a console command yet, which the core decides: a server still
    /// generating terrain has saved no world to protect, and waiting for it to
    /// finish booting so it can be asked politely is not a stop.
    func stop(serverId: String, graceful: Bool) async throws

    /// Ids currently running. One-running-server hosts return 0 or 1.
    ///
    /// Not what `native-server-active-ids` answers — that is
    /// `Core.Lifecycle.activeIds()`, which also counts a server coming up or
    /// winding down. This is only for the parts of the host that mean
    /// "running", such as the instance report.
    var runningServerIds: [String] { get }

    // MARK: Introspection

    func status(serverId: String) -> ServerState
    func players(serverId: String) -> PlayerRoster?
    func uptime(serverId: String) -> Date?
    func memoryUsage(serverId: String) -> MemoryUsage?
    func cpuUsage(serverId: String) -> Double?
    func port(serverId: String) -> Int?

    /// Points for the Insights graphs, oldest first.
    ///
    /// Empty when this is not the running server, and empty for a backend that
    /// cannot sample — an empty graph is honest, and a fabricated one is a
    /// claim about the past that a player reads as fact.
    func perfSamples(serverId: String) -> [PerfSample]

    /// Console lines since `cursor`, with a cursor for the next call.
    func logs(serverId: String, since cursor: Int) -> LogSlice

    /// Run a console command. Pumpkin has no RCON, so this dispatches
    /// in-process; the reply arrives on `native-server-rcon-response`.
    func command(serverId: String, command: String) async throws

    // MARK: Events

    /// Set by the bridge layer. Backends must invoke these on the main queue.
    ///
    /// The third argument is `backupInProgress`, and it is only ever true on a
    /// `stopped` ack. It rides along with the state change rather than going in
    /// a call of its own because that ack is what **opens the backup lease** —
    /// a separate call afterwards would race a co-host's launch. The page
    /// ignores it; only the API report carries it.
    var onStateChanged: ((String, ServerState, Bool) -> Void)? { get set }
    var onLog: ((String, String) -> Void)? { get set }
    var onPlayersChanged: ((String) -> Void)? { get set }

    /// The server is being stopped because it could not be reached, not
    /// because anyone asked.
    ///
    /// Load-bearing: the stop that follows goes through the ordinary clean
    /// path, so without this event the UI cannot tell it apart from the player
    /// pressing Stop. Emitted *before* the stop, for the same reason.
    var onNetworkError: ((String, NetworkErrorKind) -> Void)? { get set }
}

extension ServerBackend {
    /// A backend that does not sample says so by saying nothing.
    func perfSamples(serverId: String) -> [PerfSample] { [] }
}

/// Why a server became unreachable. The shared UI words each one differently.
enum NetworkErrorKind: String {
    /// The gateway never handed over tunnel credentials.
    case provisioning
    /// Credentials arrived but the tunnel never came up, or stopped.
    case handshake
}

enum ServerState: String {
    case stopped
    case starting
    case running
    case stopping
    case crashed
}

struct ServerConfig {
    let name: String
    /// Heap ceiling. Mobile devices are memory-constrained and the OS will
    /// jetsam the whole app — not just the server — so pick conservatively.
    let memoryMb: Int
    var extra: [String: Any] = [:]

    /// Fetches the tunnel credentials, once the server is up.
    ///
    /// A closure rather than data so the user's access token never becomes
    /// backend state — it stays captured in the bridge layer, which is the
    /// only place that legitimately has it.
    var resolveTunnel: (() async -> WireProxy.Link?)?

    /// What the backup subsystem needs, or nil for a server without backups.
    ///
    /// A field of its own rather than a key in `extra`: `extra` is the UI's
    /// config dict and is forwarded into the server's environment, and this
    /// carries the repository password.
    var backupContext: BackupContext?

    /// The API's `environment_variables` — what the player chose in the
    /// creation wizard. Empty when the settings could not be read, which is a
    /// server on the engine's defaults rather than a refused launch.
    ///
    /// Its own field rather than a key in `extra` for the same reason as
    /// `backupContext`: `extra` is forwarded into the server's environment,
    /// and these are the host's inputs, not the server's.
    var settingsEnv: [String: Any] = [:]

    /// The API's `game_type`, verbatim. See `HomerunAPI.ServerSettings`.
    var gameType: String = "java"
}

/// The settings and identity one launch needs to back itself up.
///
/// Captured at `native-server-start` and held by the backend until the run
/// ends — by the time the server exits, the caller's `ServerConfig` is long
/// gone and the on-stop backup still needs both.
struct BackupContext {
    let settings: HomerunAPI.ServerSettings
    /// This device's registry id. Written as the snapshot hostname, which is
    /// how the API resolves `pushed_by` — and how `backup.restoreDecision`
    /// tells our own snapshots from another device's.
    let deviceId: String
}

struct PlayerRoster {
    struct Player {
        let name: String
        let uuid: String?
    }
    let players: [Player]
    let max: Int?
}

struct MemoryUsage {
    let usedKb: Int?
    let maxMb: Int?
}

/// One point on the Insights graphs. Every field is optional, and a nil means
/// "not measured" — which the UI draws as a gap, not as a zero.
struct PerfSample {
    /// Epoch milliseconds. The host's clock, because only the host has one.
    let t: Int
    let memUsedMb: Int?
    /// May exceed 100: a server uses more than one core. Absent on the first
    /// point of a run, because a rate needs two readings.
    let cpuPercent: Double?
    let playerCount: Int?
}

struct LogSlice {
    let lines: [String]
    /// Pass back as `since` next time. Cursors are per-run, not durable: the
    /// buffer clears on restart but sequence numbers keep climbing, so a stale
    /// cursor reports `dropped` rather than silently replaying a new run as a
    /// continuation of the old one.
    let cursor: Int
    /// Lines were evicted before this read. Show the gap — a console that
    /// quietly skips output is worse than one that admits it.
    let dropped: Bool
}

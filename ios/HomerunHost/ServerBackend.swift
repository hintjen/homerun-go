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

    /// Stop gracefully: save the world, then exit. A forced kill risks the
    /// world save, which on desktop also breaks the on-stop backup.
    func stop(serverId: String) async throws

    /// Ids currently running. One-running-server hosts return 0 or 1.
    var runningServerIds: [String] { get }

    /// Ids with an engine behind them — running, or on their way down.
    ///
    /// Not the same question as `runningServerIds`. A graceful stop saves the
    /// world before it exits; for that whole window the server is no longer
    /// *running* but still very much *here*. Report it gone and the UI's
    /// reconcile loop sees "nothing active locally" against an API
    /// `target_state` that still reads `running` — the dashboard PATCHes
    /// `stopped` only once the stop call returns — decides another device
    /// started it, and `force_link_up`s. That regenerates the gateway keys and
    /// relaunches the server the user just stopped. Android shipped that bug;
    /// do not inherit it.
    var activeServerIds: [String] { get }

    // MARK: Introspection

    func status(serverId: String) -> ServerState
    func players(serverId: String) -> PlayerRoster?
    func uptime(serverId: String) -> Date?
    func memoryUsage(serverId: String) -> MemoryUsage?
    func cpuUsage(serverId: String) -> Double?
    func port(serverId: String) -> Int?

    /// Console lines since `cursor`, with a cursor for the next call.
    func logs(serverId: String, since cursor: Int) -> LogSlice

    /// Run a console command. Pumpkin has no RCON, so this dispatches
    /// in-process; the reply arrives on `native-server-rcon-response`.
    func command(serverId: String, command: String) async throws

    // MARK: Events

    /// Set by the bridge layer. Backends must invoke these on the main queue.
    var onStateChanged: ((String, ServerState) -> Void)? { get set }
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
    /// A backend whose "running" set already holds an id until its engine is
    /// genuinely gone needs nothing more. One that drops the id the moment a
    /// stop begins must override this — read the doc above before deciding
    /// which you are.
    var activeServerIds: [String] { runningServerIds }
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

enum ServerBackendError: LocalizedError {
    case notFound(String)
    case alreadyRunning(String)
    case notRunning(String)
    case portUnavailable(Int)
    /// Only one server may run at a time on this host.
    case anotherServerRunning(String)
    case engine(String)

    var errorDescription: String? {
        switch self {
        case .notFound(let id): return "No server with id \(id)"
        case .alreadyRunning(let id): return "Server \(id) is already running"
        case .notRunning(let id): return "Server \(id) is not running"
        case .portUnavailable(let p): return "Port \(p) is already in use"
        case .anotherServerRunning:
            // Surfaced to players, so phrase it for them.
            return "Another server is already running. Stop it first — this device can host one at a time."
        case .engine(let message): return message
        }
    }
}

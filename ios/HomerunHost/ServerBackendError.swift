import Foundation

/// Why a server operation failed.
///
/// In its own file because it is thrown from far more than the backend — the
/// bridge router, the launch order, the backup manager — and because keeping
/// it free of the protocol's dependencies lets `ios/coretest` compile the
/// pieces that throw it without pulling in the WebView half of the app.
///
/// Every message here reaches a player. Write them for someone who wants to
/// play Minecraft.

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

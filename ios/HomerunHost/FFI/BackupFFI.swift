import Foundation

/// The backup engine, over the C ABI.
///
/// Separate from ``HomerunFFI`` because these calls behave differently from
/// everything else that crosses this boundary. `homerun_core_call` and the
/// server getters answer instantly and are safe to call from anywhere; two of
/// the calls here open TLS connections and block for minutes, and one of them
/// needs a thread with a stack the cooperative pool will not give it.
///
/// `nonisolated` on purpose: the long calls must not run on the main actor,
/// and marking them so is what stops a well-meaning caller from `await`ing
/// them somewhere that looks safe.
enum BackupFFI {

    /// False on Android and on host builds. The symbols exist regardless; a
    /// build without the engine answers every call with a refusal.
    static var isAvailable: Bool { homerun_backup_available() == 1 }

    /// The newest snapshot, or nil if the repository has none.
    ///
    /// Networked — seconds, not milliseconds. Runs off the main actor.
    static func latestSnapshot(_ request: [String: Any]) async -> HomerunFFI.Reply {
        await onWorkerThread(name: "homerun-backup-list", stackSize: 4 * 1024 * 1024) {
            withRequest(request) { homerun_backup_latest_snapshot($0) }
        }
    }

    /// Run one backup or restore to completion.
    ///
    /// > **Blocks for minutes, on a thread with an 8 MB stack.** The engine
    /// > walks a directory tree and fans work out across its own pool, and the
    /// > 512 KB an iOS cooperative-pool thread gets is not enough — the same
    /// > lesson, and the same silent stack overflow, as the server thread.
    /// > `Task.detached` would not fix it: the stack size is not something a
    /// > `Task` lets you set.
    static func run(_ request: [String: Any]) async -> HomerunFFI.Reply {
        await onWorkerThread(name: "homerun-backup", stackSize: 8 * 1024 * 1024) {
            withRequest(request) { homerun_backup_run($0) }
        }
    }

    /// Progress since `cursor`. Cheap; safe to call from the main actor while
    /// ``run(_:)`` blocks another thread.
    struct Progress {
        let lines: [String]
        let cursor: Int
        let dropped: Bool
        let phase: String
        let current: Int
        /// Zero means "not known yet", which is most of the scanning phase.
        let total: Int
        let running: Bool

        /// Nil until there is a real denominator, so a caller cannot render
        /// "0%" for work whose size is still unknown.
        var fraction: Double? {
            guard total > 0 else { return nil }
            return min(1, Double(current) / Double(total))
        }
    }

    static func progress(since cursor: Int) -> Progress {
        let object = HomerunFFI.decode(homerun_backup_progress_since(UInt64(max(0, cursor)))).object
        return Progress(
            lines: object?["lines"] as? [String] ?? [],
            cursor: object?["cursor"] as? Int ?? cursor,
            dropped: object?["dropped"] as? Bool ?? false,
            phase: object?["phase"] as? String ?? "",
            current: object?["current"] as? Int ?? 0,
            total: object?["total"] as? Int ?? 0,
            running: object?["running"] as? Bool ?? false)
    }

    /// Ask the running job to stop.
    ///
    /// Cooperative and coarse — it lands at the next phase boundary and cannot
    /// interrupt a transfer already under way. Never blocks, and is not an
    /// error when nothing is running, which matters because the caller is
    /// usually a background-task expiry handler with seconds to live.
    static func cancel() {
        _ = HomerunFFI.decode(homerun_backup_cancel())
    }

    // MARK: - Plumbing

    private static func withRequest(
        _ request: [String: Any],
        _ call: (UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>?
    ) -> HomerunFFI.Reply {
        guard let data = try? JSONSerialization.data(withJSONObject: request),
            let json = String(data: data, encoding: .utf8)
        else {
            return HomerunFFI.Reply(object: [
                "ok": false,
                "error": "The backup request could not be prepared.",
                "message": "the request was not encodable as JSON",
            ])
        }
        return json.withCString { HomerunFFI.decode(call($0)) }
    }

    /// Run `body` on a dedicated thread with a chosen stack, and await it.
    private static func onWorkerThread(
        name: String,
        stackSize: Int,
        _ body: @escaping @Sendable () -> HomerunFFI.Reply
    ) async -> HomerunFFI.Reply {
        await withCheckedContinuation { continuation in
            let thread = Thread { continuation.resume(returning: body()) }
            thread.name = name
            thread.stackSize = stackSize
            thread.start()
        }
    }
}

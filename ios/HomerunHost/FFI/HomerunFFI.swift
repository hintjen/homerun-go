import Foundation

/// Swift-typed access to the Rust server library.
///
/// Every function in the C surface returns a heap-allocated JSON string that
/// the caller owns. That rule is enforced in exactly one place — `decode`
/// below — because it applies to *failed* calls too, and those are the easy
/// ones to forget: they are the error paths, they are rare, and the leak they
/// cause is the console growing without bound over a long session.
enum HomerunFFI {

    /// Bumped whenever the C surface changes shape. A mismatch means the
    /// staged `.a` is not the one this source was written against.
    static var abiVersion: UInt32 { homerun_abi_version() }

    // MARK: - Calls

    /// Blocks for the server's entire lifetime.
    ///
    /// > Must run on a thread with at least a 16 MB stack. See
    /// > `PumpkinBackend.startServerThread`.
    static func serverStart(serverId: String, dataDir: String, port: UInt16) -> Reply {
        serverId.withCString { id in
            dataDir.withCString { dir in
                decode(homerun_server_start(id, dir, port))
            }
        }
    }

    static func serverStop() -> Reply {
        decode(homerun_server_stop())
    }

    static func serverCommand(_ command: String) -> Reply {
        command.withCString { decode(homerun_server_command($0)) }
    }

    static func state() -> ServerState {
        guard let raw = decode(homerun_server_state()).object?["state"] as? String,
            let state = ServerState(rawValue: raw)
        else { return .stopped }
        return state
    }

    /// `{running, state, serverId?, startedAtMs?, port?}`
    static func stats() -> [String: Any] {
        decode(homerun_server_stats()).object ?? [:]
    }

    /// Null when no server is running — the UI must not render a roster for a
    /// server nobody can join.
    static func players() -> PlayerRoster? {
        guard let object = decode(homerun_server_players()).object else { return nil }
        let entries = (object["players"] as? [[String: Any]] ?? []).map { entry in
            PlayerRoster.Player(
                name: entry["name"] as? String ?? "?",
                uuid: entry["uuid"] as? String)
        }
        return PlayerRoster(players: entries, max: object["max"] as? Int)
    }

    static func logs(since cursor: Int) -> LogSlice {
        guard let object = decode(homerun_server_logs_since(UInt64(max(0, cursor)))).object else {
            return LogSlice(lines: [], cursor: cursor, dropped: false)
        }
        return LogSlice(
            lines: object["lines"] as? [String] ?? [],
            // The engine's cursor is a u64; Int is 64-bit on every device this
            // runs on, and the value is a sequence number that will not get
            // near the range where that matters.
            cursor: object["cursor"] as? Int ?? cursor,
            dropped: object["dropped"] as? Bool ?? false)
    }

    // MARK: - Decoding

    /// A decoded reply. Fallible calls carry `{"ok":bool}` and an `error`.
    struct Reply {
        let object: [String: Any]?

        var ok: Bool { object?["ok"] as? Bool ?? false }

        /// Player-facing already — the Rust side has a test asserting these
        /// messages contain no `errno`, `unwrap`, or `panicked at`.
        var error: String? { object?["error"] as? String }
    }

    private static func decode(_ raw: UnsafeMutablePointer<CChar>?) -> Reply {
        // A null return means the allocation failed, which is the one case
        // there is nothing to free.
        guard let raw else { return Reply(object: nil) }
        defer { homerun_free_string(raw) }

        let data = Data(String(cString: raw).utf8)
        let parsed = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        return Reply(object: parsed as? [String: Any])
    }
}

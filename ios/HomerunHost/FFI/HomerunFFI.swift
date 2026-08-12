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
    ///
    /// `settings` is optional and its absence is not an error — it starts the
    /// server on the engine's own configuration and says so on the console.
    static func serverStart(
        serverId: String, dataDir: String, port: UInt16, settings: StartRequest.Settings? = nil
    ) -> Reply {
        withRequest(
            StartRequest.encode(
                serverId: serverId, dataDir: dataDir, port: port, settings: settings),
            "The server could not be started."
        ) { homerun_server_start($0) }
    }

    /// What `serverStart` would apply, without starting anything.
    ///
    /// Pure — for tests, and for a host that wants to log the effective
    /// settings without waiting for a server to come up.
    static func settingsPreview(
        serverId: String, dataDir: String, port: UInt16, settings: StartRequest.Settings?
    ) -> Reply {
        withRequest(
            StartRequest.encode(
                serverId: serverId, dataDir: dataDir, port: port, settings: settings),
            "The server settings could not be read."
        ) { homerun_server_settings_preview($0) }
    }

    static func serverStop() -> Reply {
        decode(homerun_server_stop())
    }

    static func serverCommand(_ command: String) -> Reply {
        command.withCString { decode(homerun_server_command($0)) }
    }

    /// A line from Homerun itself into the server's console — a world
    /// restoring, the tunnel coming up, a launch waiting on its predecessor.
    ///
    /// Before this, the host emitted those as `native-server-log` events and
    /// nowhere else, so a console opened *after* a slow launch showed nothing
    /// of it. They now go into the same buffer the engine writes to, which is
    /// also what Android does.
    ///
    /// Appends only — ``beginConsole()`` is what clears.
    @discardableResult
    static func note(_ line: String) -> Reply {
        line.withCString { decode(homerun_server_note($0)) }
    }

    /// A launch is beginning; the previous run's console goes.
    ///
    /// Called at the top of a launch rather than left to `serverStart`, which
    /// happens after the slow part a player most wants explained.
    @discardableResult
    static func beginConsole() -> Reply {
        decode(homerun_server_console_begin())
    }

    /// Serve the dashboard's console and RCON on a loopback port.
    ///
    /// Answers "this build cannot serve a device websocket" on iOS today, and
    /// that is the honest answer rather than a gap to close: the socket would
    /// only be alive while the app is in front of the player, so advertising
    /// one would promise something the platform takes away — see
    /// `plans/ios-background-execution.md`. Wired so the shape is already right
    /// if that calculus changes.
    @discardableResult
    static func startDeviceWebsocket(_ config: String) -> Reply {
        config.withCString { decode(homerun_device_ws_start($0)) }
    }

    @discardableResult
    static func stopDeviceWebsocket() -> Reply {
        decode(homerun_device_ws_stop())
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

    /// Encode a request and hand it to a C entry point.
    ///
    /// `JSONSerialization` can only fail here on a value it cannot encode, so
    /// this path is unreachable in practice — but the alternative to answering
    /// it is a force-unwrap in the launch path, and the C surface's whole
    /// premise is that no input crashes the app.
    private static func withRequest(
        _ request: [String: Any],
        _ failure: String,
        _ call: (UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>?
    ) -> Reply {
        guard let data = try? JSONSerialization.data(withJSONObject: request),
            let json = String(data: data, encoding: .utf8)
        else {
            return Reply(object: ["ok": false, "error": failure])
        }
        return json.withCString { decode(call($0)) }
    }

    /// Internal rather than private because `BackupFFI` decodes the same way:
    /// the free-on-every-path rule is the thing worth having in one place.
    static func decode(_ raw: UnsafeMutablePointer<CChar>?) -> Reply {
        // A null return means the allocation failed, which is the one case
        // there is nothing to free.
        guard let raw else { return Reply(object: nil) }
        defer { homerun_free_string(raw) }

        let data = Data(String(cString: raw).utf8)
        let parsed = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        return Reply(object: parsed as? [String: Any])
    }
}

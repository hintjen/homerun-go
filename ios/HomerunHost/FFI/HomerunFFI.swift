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

    /// The settings a launch carries: the API's `environment_variables`
    /// verbatim, the game type that decides whether online mode is even
    /// possible, and whatever identities the host managed to resolve.
    ///
    /// Nothing here is interpreted on this side. Rust decides what each key
    /// means, so iOS and Android cannot drift on it — which is the whole
    /// reason the settings cross the boundary raw rather than pre-chewed.
    struct LaunchSettings {
        let env: [String: String]
        /// The API's value **verbatim** — `native-crossplay` is what forces
        /// offline mode, and a value reduced to java/bedrock cannot say so.
        let gameType: String
        /// Name → UUID for the players named in the env. A name missing here
        /// is one the lookup could not answer, which Rust then derives
        /// offline or drops; it is never a reason to fail a launch.
        let resolved: [String: String]
    }

    /// The wire form of a start request. One builder, used by both the launch
    /// and the preview, so `ios/coretest` exercises the same encoding the app
    /// does — a key misspelled here is caught there rather than by a player
    /// getting a server on defaults.
    static func startRequest(
        serverId: String, dataDir: String, port: UInt16, settings: LaunchSettings?
    ) -> [String: Any] {
        var request: [String: Any] = [
            "serverId": serverId,
            "dataDir": dataDir,
            "port": Int(port),
        ]
        if let settings {
            request["settings"] = [
                "env": settings.env,
                "gameType": settings.gameType,
                "resolved": settings.resolved.map { ["name": $0.key, "id": $0.value] },
            ]
        }
        return request
    }

    /// Blocks for the server's entire lifetime.
    ///
    /// > Must run on a thread with at least a 16 MB stack. See
    /// > `PumpkinBackend.startServerThread`.
    ///
    /// `settings` is optional and its absence is not an error — it starts the
    /// server on the engine's own configuration and says so on the console.
    static func serverStart(
        serverId: String, dataDir: String, port: UInt16, settings: LaunchSettings? = nil
    ) -> Reply {
        withRequest(
            startRequest(serverId: serverId, dataDir: dataDir, port: port, settings: settings),
            "The server could not be started."
        ) { homerun_server_start($0) }
    }

    /// What `serverStart` would apply, without starting anything.
    ///
    /// Pure — for tests, and for a host that wants to log the effective
    /// settings without waiting for a server to come up.
    static func settingsPreview(
        serverId: String, dataDir: String, port: UInt16, settings: LaunchSettings?
    ) -> Reply {
        withRequest(
            startRequest(serverId: serverId, dataDir: dataDir, port: port, settings: settings),
            "The server settings could not be read."
        ) { homerun_server_settings_preview($0) }
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

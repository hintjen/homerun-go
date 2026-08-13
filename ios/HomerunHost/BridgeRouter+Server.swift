import Foundation

/// The `native-server-*` family: everything about the server this device hosts.
///
/// These route through `ServerBackend` and never touch the FFI directly, so
/// the same channel code would work against a different engine. The name is
/// desktop-legacy — it meant "not WSL" — and must not be renamed in v1
/// because three repositories depend on the strings.
///
/// > **Watch the parameter shapes.** Six metrics getters and
/// > `get-native-server-logs` take a *bare string* server id, not an object,
/// > because the desktop preload forwarded a single argument. The rest take
/// > `{ serverId: ... }`. Reading the wrong one yields nil and the screen
/// > renders empty with no error.
extension BridgeRouter {

    // MARK: - Lifecycle

    func nativeServerStart(_ params: Any?) async throws -> Any? {
        guard let payload = params as? [String: Any],
            let serverId = payload["serverId"] as? String
        else { return ["success": false, "error": "Homerun could not tell which server to start."] }

        // Admission, before anything slow. The core counts the server active
        // from here, which is what the contract means by "from the moment the
        // call arrives": everything below — the settings fetch especially — is
        // a round trip during which this host has committed to a launch, and a
        // server not yet counted is one the reconcile loop starts for itself.
        //
        // `callFinished` in a `defer` on **every** verdict, not just the one
        // that launches. A duplicate that returns `alreadyRunning` still
        // counted the call; leaving without finishing retires the winner's
        // marker. Android wrote that bug and caught it.
        let admission = backend.lifecycle.startRequested(serverId)
        defer { backend.lifecycle.callFinished(serverId) }

        switch admission.verdict {
        case "proceed":
            break
        case "alreadyRunning":
            // Not an error the player needs to see — the server they asked for
            // is already up, or on its way, which is the outcome they wanted.
            return ["success": true, "alreadyRunning": true]
        case "anotherServerRunning":
            return [
                "success": false,
                "error": "\(admission.serverId ?? "Another server") is already running. "
                    + "Stop it before starting this one.",
            ]
        default:
            // Only reached when the core could not answer at all. Refusing is
            // the safe direction: proceeding would mean launching without the
            // one thing that knows whether anything else holds the slot.
            return [
                "success": false,
                "error": "Homerun could not work out whether this server can start.",
            ]
        }

        let raw = payload["config"] as? [String: Any] ?? [:]
        var config = ServerConfig(
            name: raw["name"] as? String ?? serverId,
            memoryMb: raw["memoryMb"] as? Int ?? 1024,
            extra: raw)

        // The UI passes the user's token on this call specifically so the host
        // can fetch tunnel credentials. Captured in a closure rather than put
        // in `extra`, which is forwarded to the engine — a token has no
        // business there.
        let token = payload["userToken"] as? String ?? TokenStore.accessToken ?? ""
        let apiURL = HostStore.apiURL ?? ""

        // Fetched once, on the critical path, because two things need it before
        // the engine starts: the backup lease gate, and the tunnel baseline. It
        // costs a round trip that this host did not previously pay — the same
        // one Android has always paid.
        let settings = token.isEmpty || apiURL.isEmpty
            ? nil
            : await HomerunAPI.serverSettings(apiURL: apiURL, serverId: serverId, token: token)

        // Ahead of everything expensive, and ahead of the lease gate: this host
        // links its engine, and a linked engine does not refuse a modpack — it
        // starts vanilla and looks like it worked, leaving the player a world
        // with their mods missing and no error anywhere. The rule lives in
        // `homerun-core` so both apps refuse the same servers in the same words.
        //
        // Nil settings are not a refusal, for the reason `serverSettings`
        // documents: a lookup that failed is a far worse reason not to start a
        // server than to start one on defaults.
        if let settings {
            do {
                if let refusal = try Core.hostingRefusal(
                    gameType: settings.gameType, env: settings.env)
                {
                    return ["success": false, "error": refusal]
                }
            } catch {
                // Same direction as the admission check above. A core that
                // cannot answer means a build mismatch, and launching anyway is
                // precisely the silent-wrong-world outcome this exists to stop.
                return [
                    "success": false,
                    "error": "Homerun could not work out whether this server can start.",
                ]
            }
        }

        // A launch is refused only when another device is *actively* backing
        // this world up. No settings, no backup block and no device id all mean
        // "host without backups" — never "refuse to host".
        if let settings, let deviceId = HostStore.registeredDeviceId {
            let force = payload["force"] as? Bool ?? false
            if let reason = backups.leaseBlockedReason(
                settings: settings, deviceId: deviceId, force: force)
            {
                // Emitted as well as returned: the contract declares this event
                // for hosts that advertise `backups`, and the error return is
                // what the calling promise actually sees.
                events?.emit("native-server-backup-lease-blocked", [["serverId": serverId]])
                return ["success": false, "error": reason]
            }
            if settings.backup != nil {
                config.backupContext = BackupContext(settings: settings, deviceId: deviceId)
            }
        }

        // What the player chose in the creation wizard. Absent when the
        // settings could not be read at all, which starts a server on the
        // engine's defaults rather than refusing to start one.
        if let settings {
            config.settingsEnv = settings.env
            config.gameType = settings.gameType
        }

        if !token.isEmpty, !apiURL.isEmpty {
            config.resolveTunnel = {
                // The baseline came with the settings above, so this no longer
                // re-fetches the same endpoint a second time per launch.
                await HomerunAPI.awaitTunnel(
                    apiURL: apiURL, serverId: serverId, token: token,
                    stale: settings?.tunnelBefore)
            }
        }

        // Before the backend starts, so a crash on the way up still has
        // somewhere to be reported from.
        Reporting.starting(serverId: serverId, settings: settings)

        do {
            try await backend.start(serverId: serverId, config: config)
            if let port = backend.port(serverId: serverId) {
                events?.emit("native-server-port", [["serverId": serverId, "port": port]])
            }
            return ["success": true]
        } catch {
            return ["success": false, "error": playerFacing(error)]
        }
    }

    func nativeServerStop(_ params: Any?) async throws -> Any? {
        guard let serverId = (params as? [String: Any])?["serverId"] as? String else {
            return ["success": false, "error": "Homerun could not tell which server to stop."]
        }

        // The core records the intent and keeps the server counted active for
        // the whole of the shutdown — the dashboard PATCHes `stopped` only
        // after this call returns, so until then the API still reads `running`,
        // and a host that reports itself idle in that window gets the server
        // restarted underneath it.
        let stop = backend.lifecycle.stopRequested(serverId)
        defer { backend.lifecycle.callFinished(serverId) }

        if stop.verdict == "notRunning" {
            return ["success": false, "error": "That server is not running."]
        }

        do {
            // `abandonLaunch` needs no branch: the intent is recorded either
            // way, and a launch with nothing spawned yet gives up at its next
            // checkpoint rather than being told to stop something.
            try await backend.stop(serverId: serverId, graceful: stop.verdict == "graceful")
            return ["success": true]
        } catch {
            return ["success": false, "error": playerFacing(error)]
        }
    }

    func nativeServerDelete(_ params: Any?) async throws -> Any? {
        guard let serverId = (params as? [String: Any])?["serverId"] as? String else {
            return ["success": false, "error": "Homerun could not tell which server to delete."]
        }
        do {
            try backend.delete(serverId: serverId)
            return ["success": true]
        } catch {
            return ["success": false, "error": playerFacing(error)]
        }
    }

    /// Pumpkin has no RCON. The command is dispatched in-process and its
    /// output arrives in the console; the acknowledgement goes back on
    /// `native-server-rcon-response` so the UI's console can echo it.
    func nativeServerRcon(_ params: Any?) async throws -> Any? {
        guard let payload = params as? [String: Any],
            let serverId = payload["serverId"] as? String,
            let command = payload["command"] as? String
        else { return ["success": false, "error": "That command could not be sent."] }

        do {
            try await backend.command(serverId: serverId, command: command)
            // An `op` or a `ban` typed here has to reach the server's settings
            // too, or the next launch rewrites ops.json from the API and
            // quietly takes it back. Signed as the person who typed it.
            Reporting.consoleCommand(serverId: serverId, command: command)
            events?.emit(
                "native-server-rcon-response", [["serverId": serverId, "response": NSNull()]])
            return ["success": true]
        } catch {
            return ["success": false, "error": playerFacing(error)]
        }
    }

    /// Running *or coming up*. The second half is not a nicety: see
    /// `BridgeRouter.startsInFlight`.
    func nativeServerActiveIds(_ params: Any?) async throws -> Any? {
        backend.lifecycle.activeIds()
    }

    // MARK: - Metrics (bare-string params)

    func nativeServerGetUptime(_ params: Any?) async throws -> Any? {
        guard let serverId = params as? String, let started = backend.uptime(serverId: serverId)
        else { return ["startedAt": NSNull()] }
        return ["startedAt": Int(started.timeIntervalSince1970 * 1000)]
    }

    func nativeServerGetOps(_ params: Any?) async throws -> Any? {
        guard let serverId = params as? String else { return ["ops": []] }
        return ["ops": backend.ops(serverId: serverId)]
    }

    func nativeServerGetMemUsage(_ params: Any?) async throws -> Any? {
        guard let serverId = params as? String, let usage = backend.memoryUsage(serverId: serverId)
        else { return ["usedKb": NSNull(), "maxMb": NSNull()] }
        return ["usedKb": orNull(usage.usedKb), "maxMb": orNull(usage.maxMb)]
    }

    func nativeServerGetCpuUsage(_ params: Any?) async throws -> Any? {
        guard let serverId = params as? String, let cpu = backend.cpuUsage(serverId: serverId)
        else { return ["cpuPercent": NSNull()] }
        return ["cpuPercent": cpu]
    }

    /// Null when nothing is running — the UI must not show a roster for a
    /// server nobody can join.
    func nativeServerGetPlayers(_ params: Any?) async throws -> Any? {
        guard let serverId = params as? String, let roster = backend.players(serverId: serverId)
        else { return nil }

        return [
            "players": roster.players.map { ["name": $0.name, "uuid": orNull($0.uuid)] },
            "max": orNull(roster.max),
        ]
    }

    func nativeServerGetPerfHistory(_ params: Any?) async throws -> Any? {
        guard let serverId = params as? String else { return [] }
        // The JSON encoding lives here rather than in the backend: `NSNull` is
        // a bridge concern, and a backend that speaks it is one the engine
        // interface cannot describe.
        return backend.perfSamples(serverId: serverId).map { sample in
            [
                "t": sample.t,
                "memUsedMb": orNull(sample.memUsedMb),
                "cpuPercent": orNull(sample.cpuPercent),
                "playerCount": orNull(sample.playerCount),
            ]
        }
    }

    func getNativeServerLogs(_ params: Any?) async throws -> Any? {
        guard let serverId = params as? String else { return [] }
        // A full read for the UI's console pane. The event stream is what
        // keeps it live; this is what fills it on open.
        return backend.logs(serverId: serverId, since: 0).lines
    }

    // MARK: - Connectivity

    func getNativeServerPort(_ params: Any?) async throws -> Any? {
        guard let serverId = (params as? [String: Any])?["serverId"] as? String else {
            return ["port": NSNull()]
        }
        return ["port": orNull(backend.port(serverId: serverId))]
    }

    /// LAN play is always on: the device is on the player's Wi-Fi and the
    /// address is how a friend joins. There is nothing to toggle, so this
    /// reports enabled rather than pretending a switch exists.
    func getNativeLocalNetwork(_ params: Any?) async throws -> Any? {
        ["enabled": true]
    }

    func setNativeLocalNetwork(_ params: Any?) async throws -> Any? {
        ["success": true]
    }

    // MARK: - Files

    /// The UI hides its server-files surface when `exists` is null. iOS has no
    /// file manager to hand a folder to, so that is the honest answer.
    func serverFilesExist(_ params: Any?) async throws -> Any? {
        ["native": true, "exists": NSNull()]
    }

    func openServerFiles(_ params: Any?) async throws -> Any? {
        // Only reachable if `server-files-exist` said yes, which it does not.
        ["error": "Opening server files is not available on iPhone."]
    }

    /// Not a backlog item: the contract gates this behind `fileImport`, which
    /// iOS declares false, so the UI hides world import and its entry points
    /// entirely. Getting a world `.zip` onto a phone in the first place is the
    /// flow that is not worth building.
    ///
    /// Answered anyway rather than dropped — a capability-gated channel called
    /// regardless must return an error, never a silent success or a hang
    /// (PROTOCOL.md §5).
    func importMinecraftWorld(_ params: Any?) async throws -> Any? {
        ["success": false, "error": "Importing a world isn't supported on iPhone."]
    }

    // MARK: -

    /// JSON `null` for an absent value.
    ///
    /// Written as a generic rather than `?? NSNull()` because the operands
    /// have different types, and written with an `if let` rather than a cast
    /// because promoting `T?` to `Any?` wraps a nil in a second optional —
    /// which serialises as a present value.
    private func orNull<T>(_ value: T?) -> Any {
        if let value { return value }
        return NSNull()
    }

    /// Backend errors already carry player-facing text; anything else would
    /// leak a system message into the UI.
    private func playerFacing(_ error: Error) -> String {
        if let backendError = error as? ServerBackendError {
            return backendError.errorDescription ?? "The server could not do that."
        }
        if let bridgeError = error as? BridgeError {
            return bridgeError.message
        }
        return "The server could not do that."
    }
}

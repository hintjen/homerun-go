import Foundation

/// Runs Pumpkin in this process, over the Rust FFI.
///
/// iOS cannot spawn processes, so there is no pid, no stdio pipe and no JVM —
/// the server is a library call that blocks a thread for as long as the world
/// is up. Everything else here follows from that.
@MainActor
final class PumpkinBackend: ServerBackend {
    let kind = "pumpkin"

    var onStateChanged: ((String, ServerState) -> Void)?
    var onLog: ((String, String) -> Void)?
    var onPlayersChanged: ((String) -> Void)?

    /// The id whose thread is currently up. One at a time, enforced here and
    /// again in the engine.
    private var activeServerId: String?
    private var startedAt: Date?
    private var listeningPort: Int?

    /// Set when the blocking start call returns. A run that ends on its own
    /// crashed; a run that ends after a stop request did not.
    private var runFailure: String?
    private var threadFinished = false
    private var stopRequested = false

    private var logCursor = 0
    private var logTimer: Timer?
    private var pollTimer: Timer?
    private var lastPlayerSignature = ""

    private var perfHistory: [(t: Date, memUsedMb: Double?, cpuPercent: Double?, players: Int?)] = []
    private let cpuSampler = CPUSampler()

    // MARK: - Lifecycle

    func create(serverId: String) throws {
        // Idempotent by contract: createDirectory with intermediates does not
        // complain about an existing directory.
        let directory = HostStore.serverDirectory(id: serverId)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        excludeFromBackup(directory)
    }

    func delete(serverId: String) throws {
        guard activeServerId != serverId else {
            throw ServerBackendError.alreadyRunning(serverId)
        }
        let directory = HostStore.serverDirectory(id: serverId)
        if FileManager.default.fileExists(atPath: directory.path) {
            try FileManager.default.removeItem(at: directory)
        }
    }

    func start(serverId: String, config: ServerConfig) async throws {
        if let active = activeServerId {
            throw active == serverId
                ? ServerBackendError.alreadyRunning(serverId)
                : ServerBackendError.anotherServerRunning(active)
        }

        try create(serverId: serverId)

        activeServerId = serverId
        startedAt = nil
        runFailure = nil
        threadFinished = false
        stopRequested = false
        logCursor = 0
        perfHistory.removeAll()
        emitState(serverId, .starting)

        let port = (config.extra["port"] as? Int).map(UInt16.init) ?? 25565
        startServerThread(serverId: serverId, port: port)
        startPumps(serverId: serverId)

        // No timeout, deliberately: first boot generates a world and can take
        // minutes. The UI shows console output the whole time, and the only
        // thing that ends this wait is the server coming up or the run ending.
        while true {
            if threadFinished {
                // `serverThreadExited` has already torn the run down and
                // emitted the final state — doing it again here would send the
                // UI a second, contradictory state change.
                throw ServerBackendError.engine(
                    runFailure ?? "The server stopped before it finished starting.")
            }

            if HomerunFFI.state() == .running {
                startedAt = Date()
                listeningPort = HomerunFFI.stats()["port"] as? Int ?? Int(port)
                emitState(serverId, .running)
                return
            }

            try await Task.sleep(nanoseconds: 200_000_000)
        }
    }

    /// > **The 16 MB stack is load-bearing.** The 512 KB default overflows
    /// > inside the engine and takes the app down with no panic report and no
    /// > crash log — the single most confusing failure this code can produce.
    /// > `Task` and `Thread.detachNewThread` both give you the default; only
    /// > a configured `Thread` lets you set it.
    private func startServerThread(serverId: String, port: UInt16) {
        let directory = HostStore.serverDirectory(id: serverId).path

        let thread = Thread { [weak self] in
            let reply = HomerunFFI.serverStart(
                serverId: serverId, dataDir: directory, port: port)

            // Hop back: every property here is main-actor state, and the UI
            // reads it.
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    self?.serverThreadExited(reply)
                }
            }
        }
        thread.name = "homerun-server"
        thread.stackSize = 16 * 1024 * 1024
        thread.start()
    }

    private func serverThreadExited(_ reply: HomerunFFI.Reply) {
        threadFinished = true
        // A clean shutdown after a stop request is not a failure; anything
        // else ended on its own and the player needs to know why.
        if !reply.ok, !stopRequested {
            runFailure = reply.error ?? "The server stopped unexpectedly."
        }

        guard let serverId = activeServerId else { return }
        finish(serverId: serverId, state: runFailure == nil ? .stopped : .crashed)
    }

    func stop(serverId: String) async throws {
        guard activeServerId == serverId else {
            throw ServerBackendError.notRunning(serverId)
        }

        stopRequested = true
        emitState(serverId, .stopping)

        // Graceful: the engine saves the world and then returns. Killing the
        // thread instead would risk the save, and a half-written world is the
        // worst outcome available here.
        let reply = await Task.detached { HomerunFFI.serverStop() }.value
        if !reply.ok, let error = reply.error {
            throw ServerBackendError.engine(error)
        }
    }

    /// Tear down one run's state and announce the final state, once.
    private func finish(serverId: String, state: ServerState) {
        // The last of the console — including whatever the engine said on its
        // way down, which is usually the reason.
        drainLogs(serverId: serverId)

        logTimer?.invalidate()
        logTimer = nil
        pollTimer?.invalidate()
        pollTimer = nil

        activeServerId = nil
        startedAt = nil
        listeningPort = nil
        emitState(serverId, state)
    }

    var runningServerIds: [String] {
        activeServerId.map { [$0] } ?? []
    }

    // MARK: - Introspection

    func status(serverId: String) -> ServerState {
        guard activeServerId == serverId else { return .stopped }
        return HomerunFFI.state()
    }

    func players(serverId: String) -> PlayerRoster? {
        guard activeServerId == serverId else { return nil }
        return HomerunFFI.players()
    }

    func uptime(serverId: String) -> Date? {
        guard activeServerId == serverId else { return nil }
        return startedAt
    }

    /// Whole-process footprint — the same number Xcode's memory gauge shows.
    /// The server dominates it while running, and there is no per-server
    /// figure to report because the server is not a separate process.
    func memoryUsage(serverId: String) -> MemoryUsage? {
        guard activeServerId == serverId else { return nil }
        return MemoryUsage(
            usedKb: DeviceMetrics.footprintKb(),
            maxMb: Int(ProcessInfo.processInfo.physicalMemory / 1_048_576))
    }

    func cpuUsage(serverId: String) -> Double? {
        guard activeServerId == serverId else { return nil }
        return cpuSampler.sample()
    }

    func port(serverId: String) -> Int? {
        guard activeServerId == serverId else { return nil }
        return listeningPort
    }

    func logs(serverId: String, since cursor: Int) -> LogSlice {
        HomerunFFI.logs(since: cursor)
    }

    func command(serverId: String, command: String) async throws {
        guard activeServerId == serverId else {
            throw ServerBackendError.notRunning(serverId)
        }
        let reply = await Task.detached { HomerunFFI.serverCommand(command) }.value
        if !reply.ok, let error = reply.error {
            throw ServerBackendError.engine(error)
        }
    }

    /// Operator names, read from the engine's own `ops.json`. Absent before
    /// the first op is added, which is not an error.
    func ops(serverId: String) -> [String] {
        let url = HostStore.serverDirectory(id: serverId).appendingPathComponent("ops.json")
        guard let data = try? Data(contentsOf: url),
            let entries = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
        else { return [] }
        return entries.compactMap { $0["name"] as? String }
    }

    func perfSamples(serverId: String) -> [[String: Any]] {
        perfHistory.map { sample in
            [
                "t": Int(sample.t.timeIntervalSince1970 * 1000),
                "memUsedMb": sample.memUsedMb ?? NSNull(),
                "cpuPercent": sample.cpuPercent ?? NSNull(),
                "playerCount": sample.players ?? NSNull(),
            ]
        }
    }

    // MARK: - Pumps

    private func startPumps(serverId: String) {
        // The console is polled rather than pushed: the engine buffers lines
        // and hands them over by cursor, which survives the UI not asking for
        // a while (a backgrounded phone may not poll for minutes).
        logTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.drainLogs(serverId: serverId) }
        }

        pollTimer = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.sample(serverId: serverId) }
        }
    }

    private func drainLogs(serverId: String) {
        let slice = HomerunFFI.logs(since: logCursor)

        // Say so rather than hiding it. A console that silently skips output
        // sends someone hunting for a message that was never written.
        if slice.dropped && logCursor > 0 {
            onLog?(serverId, "… earlier output skipped …")
        }

        for line in slice.lines {
            onLog?(serverId, line)
        }
        logCursor = slice.cursor
    }

    private func sample(serverId: String) {
        guard activeServerId == serverId else { return }

        let roster = HomerunFFI.players()
        perfHistory.append(
            (
                t: Date(),
                memUsedMb: DeviceMetrics.footprintKb().map { Double($0) / 1024.0 },
                cpuPercent: cpuSampler.sample(),
                players: roster?.players.count
            ))
        // Roughly an hour at one sample per five seconds.
        if perfHistory.count > 720 { perfHistory.removeFirst(perfHistory.count - 720) }

        // The UI redraws the roster on this, so only fire when it changed.
        let signature = (roster?.players ?? []).map(\.name).sorted().joined(separator: ",")
        if signature != lastPlayerSignature {
            lastPlayerSignature = signature
            onPlayersChanged?(serverId)
        }
    }

    private func emitState(_ serverId: String, _ state: ServerState) {
        onStateChanged?(serverId, state)
    }

    /// Minecraft worlds are large and change constantly. Syncing one to iCloud
    /// is a bug, not a feature — it burns the player's quota and their data.
    private func excludeFromBackup(_ url: URL) {
        var url = url
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try? url.setResourceValues(values)
    }
}

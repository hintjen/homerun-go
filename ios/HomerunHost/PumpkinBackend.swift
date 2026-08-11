import Foundation

/// Runs Pumpkin in this process, over the Rust FFI.
///
/// iOS cannot spawn processes, so there is no pid, no stdio pipe and no JVM —
/// the server is a library call that blocks a thread for as long as the world
/// is up. Everything else here follows from that.
@MainActor
final class PumpkinBackend: ServerBackend {
    let kind = "pumpkin"

    var onStateChanged: ((String, ServerState, Bool) -> Void)?
    var onLog: ((String, String) -> Void)?
    var onPlayersChanged: ((String) -> Void)?
    var onNetworkError: ((String, NetworkErrorKind) -> Void)?

    /// The tunnel that makes this server reachable off the local Wi-Fi.
    private let wireProxy = WireProxy()
    private var tunnelTask: Task<WireProxy.Link?, Never>?

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
    private var heartbeat: Timer?
    private var pollTimer: Timer?
    private var lastPlayerSignature = ""

    private var perfHistory: [(t: Date, memUsedMb: Double?, cpuPercent: Double?, players: Int?)] = []
    private let cpuSampler = CPUSampler()

    private let backups = BackupManager()

    /// Held for the life of one run. By the time the server exits, the
    /// caller's `ServerConfig` is long gone and the on-stop backup still needs
    /// the repository credentials and this device's id.
    private var backupContext: BackupContext?

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
        // Ownership passes from the claim to the real id here, so the two are
        // never both set and `runningServerIds` never reports twice.
        claimedServerId = nil
        startedAt = nil
        runFailure = nil
        threadFinished = false
        stopRequested = false
        logCursor = 0
        perfHistory.removeAll()
        backupContext = config.backupContext
        emitState(serverId, .starting)

        // Before anything reads or writes the world, and before the engine
        // thread exists — a restore loads the whole repository index into
        // memory, and doing that beside a running Pumpkin is how a phone gets
        // jetsammed.
        //
        // A failure here aborts the launch, which is correct: starting on a
        // world we have been told is stale is the divergence this exists to
        // prevent. The state has to be unwound by hand, because nothing has
        // been started yet for `finish` to tear down.
        if let backup = config.backupContext {
            do {
                try await backups.restoreBeforeLaunch(
                    serverId: serverId, dir: HostStore.serverDirectory(id: serverId),
                    context: backup,
                    onLog: { [weak self] line in self?.onLog?(serverId, line) })
            } catch {
                activeServerId = nil
                backupContext = nil
                emitState(serverId, .stopped)
                throw error
            }
        }

        let port = (config.extra["port"] as? Int).map(UInt16.init) ?? 25565

        // Kicked before the engine, so the gateway's ≤60 s provisioning poll
        // overlaps world generation rather than following it.
        if let resolveTunnel = config.resolveTunnel {
            tunnelTask = Task { await resolveTunnel() }
        }

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
                HostLog.host.info("engine reports running; opening tunnel")
                // The engine is listening on loopback. That is not the same as
                // being joinable, so the tunnel goes up before anyone is told
                // the server is running — desktop learned this the hard way,
                // as "a silently-rejected start masquerading as running".
                try await openTunnel(serverId: serverId, port: Int(port))

                startedAt = Date()
                listeningPort = HomerunFFI.stats()["port"] as? Int ?? Int(port)
                startHeartbeat(serverId: serverId)
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

    /// Bring the tunnel up, or stop the server.
    ///
    /// A tunnel failure is fatal, matching desktop and Android. A phone on
    /// cellular is behind CGNAT and has no port-forwarding fallback, so a
    /// server without a tunnel is one nobody can join — leaving it running
    /// would just be a slower way to fail.
    private func openTunnel(serverId: String, port: Int) async throws {
        guard let tunnelTask else { return }
        self.tunnelTask = nil

        HostLog.tunnel.info("awaiting credentials")
        onLog?(serverId, "[Homerun] Connecting to the Homerun gateway...")

        guard let link = await tunnelTask.value else {
            try failTunnel(
                serverId: serverId, kind: .provisioning,
                message: "Could not connect to the Homerun gateway: it did not provide a connection.")
            return
        }

        wireProxy.onHandshakeFailed = { [weak self] in
            guard let self else { return }
            Task { await self.stopForNetworkError(serverId: serverId, kind: .handshake) }
        }

        HostLog.tunnel.info("credentials in hand; bringing the interface up")
        do {
            try wireProxy.start(link: link, minecraftPort: port)
        } catch {
            let detail = (error as? ServerBackendError)?.errorDescription ?? "\(error)"
            try failTunnel(serverId: serverId, kind: .provisioning, message: detail)
            return
        }

        HostLog.tunnel.info("interface up")
        onLog?(serverId, "[Homerun] Connected to the Homerun gateway.")
    }

    /// Report, stop, and throw — so `native-server-start` answers with the
    /// reason rather than reporting a server nobody can reach.
    private func failTunnel(serverId: String, kind: NetworkErrorKind, message: String) throws {
        onLog?(serverId, "[Homerun] \(message) Stopping server.")
        Task { await stopForNetworkError(serverId: serverId, kind: kind) }
        throw ServerBackendError.engine(message)
    }

    /// The event goes out *before* the stop. The stop itself is the ordinary
    /// clean path, so without this the UI cannot tell it from the player
    /// pressing Stop.
    private func stopForNetworkError(serverId: String, kind: NetworkErrorKind) async {
        onNetworkError?(serverId, kind)
        try? await stop(serverId: serverId)
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

        // Tunnel first: one that outlives its server keeps the gateway's peer
        // slot occupied, and the next start fails for a reason that looks
        // nothing like this one.
        tunnelTask?.cancel()
        tunnelTask = nil
        wireProxy.stop()

        // Graceful: the engine saves the world and then returns. Killing the
        // thread instead would risk the save, and a half-written world is the
        // worst outcome available here.
        let reply = await Task.detached { HomerunFFI.serverStop() }.value
        if !reply.ok, let error = reply.error {
            throw ServerBackendError.engine(error)
        }
    }

    /// Tear down one run's state and announce the final state, once.
    ///
    /// Also reached when the engine exits on its own, which is why the tunnel
    /// is closed here as well as in `stop` — a crashed server must not leave
    /// the gateway holding a peer slot.
    private func finish(serverId: String, state: ServerState) {
        tunnelTask?.cancel()
        tunnelTask = nil
        wireProxy.stop()

        // The last of the console — including whatever the engine said on its
        // way down, which is usually the reason.
        drainLogs(serverId: serverId)

        heartbeat?.invalidate()
        heartbeat = nil

        logTimer?.invalidate()
        logTimer = nil
        pollTimer?.invalidate()
        pollTimer = nil

        // Cleared before the ack, so the instance report it sends is empty —
        // the backend stops believing this device hosts a server the moment it
        // does not.
        activeServerId = nil
        startedAt = nil
        listeningPort = nil

        // Every precondition, decided here, *before* the ack.
        //
        // The ack below is what opens the backup lease, and the lease has no
        // timeout — a device that claims it and then finds it has nothing to do
        // locks every other device out of this world until its own next
        // `running` ack. So the question "will we back up" is answered once,
        // in one variable, and the answer is what the ack carries. Android
        // decides it in two places and leaks the lease on two paths as a
        // result.
        //
        // A crash is never backed up: the world was not shut down cleanly and
        // pushing it over a good snapshot is how a corrupted save spreads.
        let context = backupContext
        backupContext = nil
        let willBackUp =
            state != .crashed
            && context?.settings.backup != nil
            && BackupFFI.isAvailable
            && backups.hasLocalWorld(HostStore.serverDirectory(id: serverId))

        emitState(serverId, state, backupInProgress: willBackUp)

        if willBackUp, let context, let repo = context.settings.backup {
            let dir = HostStore.serverDirectory(id: serverId)
            Task { [backups] in
                await backups.backupAfterStop(
                    serverId: serverId, dir: dir, repo: repo, deviceId: context.deviceId,
                    onLog: { [weak self] line in self?.onLog?(serverId, line) })
            }
        }
    }

    /// A start this host has accepted but not yet begun.
    ///
    /// The contract requires `native-server-active-ids` to answer "running
    /// **or coming up**, from the moment the call arrives". Everything between
    /// the call and the engine thread — the settings fetch, the backup lease
    /// gate — is time in which this host has committed to a launch and has no
    /// `activeServerId` to show for it.
    ///
    /// Reporting nothing there is not a cosmetic gap. The UI's reconcile loop
    /// reads a missing id as a start issued from *another* device and asks the
    /// API to `force_link_up`, which regenerates the gateway keys underneath a
    /// launch that has already resolved its tunnel config — the tunnel then
    /// connects and carries nothing, which looks like a server that came up
    /// and cannot be joined.
    private var claimedServerId: String?

    /// Claim the slot synchronously, before anything is awaited.
    func claimStart(serverId: String) {
        claimedServerId = serverId
    }

    /// Give it back. A no-op once `start` has taken ownership, so a caller can
    /// `defer` this without having to know which happened.
    func releaseStart(serverId: String) {
        if claimedServerId == serverId { claimedServerId = nil }
    }

    var runningServerIds: [String] {
        if let activeServerId { return [activeServerId] }
        return claimedServerId.map { [$0] } ?? []
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

    /// Report this server to the backend, now and then periodically.
    ///
    /// The backend treats a service with no recent report as unhealthy, and
    /// the UI shows that as a server still starting up — so this is what
    /// finishes the start, not the tunnel.
    private func startHeartbeat(serverId: String) {
        heartbeat?.invalidate()
        // No immediate report here: the `running` emit that follows sends the
        // instance report and the state ack together, in that order. This timer
        // is only the keep-alive after that.
        heartbeat = Timer.scheduledTimer(withTimeInterval: 15, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.report(instances: [serverId]) }
        }
    }

    private func report(instances: [String]) {
        guard let apiURL = HostStore.apiURL,
            let deviceId = HostStore.registeredDeviceId,
            let deviceToken = TokenStore.deviceToken
        else { return }

        Task {
            await HomerunAPI.reportInstances(
                apiURL: apiURL, deviceId: deviceId, deviceToken: deviceToken, instances: instances)
        }
    }

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

    /// Announce a state change, to the API and to the page.
    ///
    /// Both go out from here because this is the only funnel every state
    /// passes through, and the two are not interchangeable: the page event
    /// updates the screen in front of the player, and the API ack is what the
    /// dashboard and every other device see. Reporting only the first is what
    /// leaves a server looking stuck to everyone but its host.
    ///
    /// `backupInProgress` reaches the API only. It is true on exactly one kind
    /// of ack — a `stopped` whose world is about to be backed up — and sending
    /// it opens the backup lease.
    private func emitState(
        _ serverId: String, _ state: ServerState, backupInProgress: Bool = false
    ) {
        HostLog.host.info("state -> \(state.rawValue, privacy: .public)")
        report(state: state, serverId: serverId, backupInProgress: backupInProgress)
        onStateChanged?(serverId, state, backupInProgress)
    }

    /// The API's view of a server's lifecycle.
    ///
    /// Only the resting states are acked. `starting` and `stopping` are this
    /// host talking to itself about work in progress; the API models a server
    /// as running or not, and a `crashed` server is stopped as far as it is
    /// concerned — the reason belongs in the console, not the status field.
    /// **Two reports, not one, and in this order.** The state POST records what
    /// happened; the instance report is what the API derives *health* from, and
    /// health is what the UI shows. Sending only the state leaves a server that
    /// is genuinely up reading as not-running until the next heartbeat tick.
    /// Desktop and Android both push the pair together for this reason.
    private func report(state: ServerState, serverId: String, backupInProgress: Bool) {
        let status: String
        switch state {
        case .running: status = "running"
        case .stopped, .crashed: status = "stopped"
        case .starting, .stopping: return
        }

        guard let apiURL = HostStore.apiURL,
            let deviceId = HostStore.registeredDeviceId,
            let deviceToken = TokenStore.deviceToken
        else { return }

        // Read the running set now rather than capturing it: the transition has
        // already been applied by the time this runs, so it is the post-change
        // truth in both directions.
        let instances = runningServerIds

        Task {
            await HomerunAPI.reportInstances(
                apiURL: apiURL, deviceId: deviceId, deviceToken: deviceToken,
                instances: instances)
            await HomerunAPI.reportServerState(
                apiURL: apiURL, serverId: serverId, state: status,
                deviceToken: deviceToken, backupInProgress: backupInProgress)
        }
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

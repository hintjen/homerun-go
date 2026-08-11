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

    /// Who owns a server right now, and what an exit meant.
    ///
    /// Process-lifetime, and deliberately here rather than on the router: a
    /// launch and a stop must see the same instance, and this object outlives
    /// a render-process death that would take a router with it. The router
    /// reaches it as `backend.lifecycle`.
    let lifecycle = Core.Lifecycle()

    /// Why the blocking start call returned, when it was not asked to.
    private var runFailure: String?
    private var threadFinished = false

    private var logCursor = 0
    private var logTimer: Timer?
    private var heartbeat: Timer?
    private var pollTimer: Timer?
    private var lastPlayerSignature = ""

    /// This run's graph, kept by `homerun-core::metrics`.
    ///
    /// One per run: a graph covers a session, so `start` resets it rather than
    /// this being reused across launches. The host reads counters and offers
    /// them; every decision about them — the rate, the retention, whether a
    /// number can be trusted at all — is the core's.
    private let metrics = Core.Metrics()

    private let backups = BackupManager()

    /// Held for the life of one run. By the time the server exits, the
    /// caller's `ServerConfig` is long gone and the on-stop backup still needs
    /// the repository credentials and this device's id.
    private var backupContext: BackupContext?

    /// Where a backup's `[Backup] …` lines go: the player's console, and the
    /// device log.
    ///
    /// Both, because they answer different questions. The console is what a
    /// player watches while a world uploads. The device log is the only record
    /// left once the page is gone — and the on-stop backup outlives the screen
    /// that started it, so "did the backup run, and what did it say" is
    /// otherwise unanswerable after the fact.
    ///
    /// Safe to log: these are the player-facing strings, and nothing about the
    /// repository — no URL, no passphrase — is ever in them.
    private func backupLog(serverId: String) -> (String) -> Void {
        { [weak self] line in
            HostLog.host.info("\(line, privacy: .public)")
            self?.onLog?(serverId, line)
        }
    }

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

    /// Run one launch, in the order `homerun-core` gives.
    ///
    /// The order used to be written out longhand here and it drifted from the
    /// desktop's twice. Now the core hands over the steps and this walks them,
    /// with `LaunchOrder` refusing to take them out of sequence and honouring
    /// a stop at every checkpoint.
    ///
    /// The plan is asked for as a linked engine, so the two steps that are
    /// about a jar are not in it at all. Two others that sound like a JVM's
    /// are — `ensureRuntime` unpacks a bundled payload, `acceptEula` writes a
    /// file — and this host skips those by not arriving at them. Skipping is
    /// allowed, reordering is not.
    func start(serverId: String, config: ServerConfig) async throws {
        try create(serverId: serverId)

        activeServerId = serverId
        startedAt = nil
        runFailure = nil
        threadFinished = false
        logCursor = 0
        // A graph covers one session. This also drops the rate anchor, which
        // is what stops the first point of this run being measured against the
        // last run's CPU counter — a fabricated spike the old `CPUSampler`,
        // which nothing reset, produced on every relaunch.
        metrics.reset()
        lastPlayerSignature = ""
        backupContext = config.backupContext

        var order = LaunchOrder(
            steps: (try? Core.launchPlan(
                backups: config.backupContext != nil,
                // True even though nothing here writes a file: the step's
                // ordering constraints hold verbatim for a linked engine —
                // after `restoreWorld` because a restore can replace
                // `pumpkin.toml`, before `spawn` because the server snapshots
                // its MOTD and player cap while it is being constructed.
                settings: true,
                tunnel: config.resolveTunnel != nil)) ?? [],
            serverId: serverId,
            lifecycle: lifecycle)

        do {
            if try order.at("cancelOnStopBackup"), lifecycle.supersedesOnStopBackup(serverId) {
                // This device is relaunching this server, so its own on-stop
                // backup is now pointless work holding the world open. The
                // cancel is cooperative and lands at the engine's next phase
                // boundary, and is a documented no-op when nothing is running.
                //
                // The cancelled backup files no `backup-state`, and that is
                // deliberate — the lease looks leaked and is not. This launch's
                // own `running` ack closes it, and reporting a failure here
                // would race that ack.
                //
                // The *message* is gated separately, on there actually being a
                // job. `supersedesOnStopBackup` is true on every start — the
                // core counts a server active from the moment the call arrives,
                // so `is_active()` holds — and telling a player their backup is
                // being cancelled when they have never taken one is a small lie
                // told on every single launch.
                if BackupFFI.progress(since: 0).running {
                    backupLog(serverId: serverId)(
                        "[Backup] Cancelling the backup still running for this server…")
                }
                BackupFFI.cancel()
            }

            if try order.at("announceStarting") {
                emitState(serverId, .starting)
            }

            // Started here rather than after the restore, which is the one
            // reordering this port makes to the old code: the gateway's ≤60 s
            // provisioning poll now overlaps the restore instead of following
            // it. On a phone pulling a large world that is a whole minute.
            if try order.at("beginResolveTunnel"), let resolveTunnel = config.resolveTunnel {
                tunnelTask = Task { await resolveTunnel() }
            }

            if try order.at("awaitPreviousExit") {
                try await awaitPreviousExit(serverId: serverId)
            }

            if try order.at("restoreWorld"), let backup = config.backupContext {
                // Before anything writes the server directory, and before the
                // engine thread exists — a restore loads the whole repository
                // index into memory, and doing that beside a running Pumpkin
                // is how a phone gets jetsammed.
                //
                // A failure aborts the launch: starting on a world we have
                // been told is stale is the divergence this exists to prevent.
                try await backups.restoreBeforeLaunch(
                    serverId: serverId, dir: HostStore.serverDirectory(id: serverId),
                    context: backup,
                    onLog: backupLog(serverId: serverId))
            }

            let port = (config.extra["port"] as? Int).map(UInt16.init) ?? 25565

            var settings: StartRequest.Settings?
            if try order.at("writeSettings") {
                settings = await resolveSettings(serverId: serverId, config: config)
            }

            if try order.at("spawn") {
                startServerThread(serverId: serverId, port: port, settings: settings)
                lifecycle.spawned(serverId)
                startPumps(serverId: serverId)
            }

            _ = try order.at("awaitConsole")
            try await awaitConsole(serverId: serverId, port: port, order: &order)
        } catch {
            // Nothing has been announced as running and `finish` has nothing to
            // tear down unless the thread got as far as starting, so unwind by
            // hand. `serverThreadExited` owns the case where it did.
            if !threadFinished {
                activeServerId = nil
                backupContext = nil
                tunnelTask?.cancel()
                tunnelTask = nil
                lifecycle.abandoned(serverId)
                emitState(serverId, .stopped)
            }
            throw error
        }
    }

    /// Wait out an engine from a previous run that has not finished exiting.
    ///
    /// One slot, enforced here and again in the engine, so this is usually
    /// instant. It is not always: a graceful stop saves the world first, and a
    /// restart admitted while that was still happening is how Android got two
    /// concurrent launches. The wait sits ahead of the restore because a
    /// mobile launch writes the server directory before it spawns anything.
    private func awaitPreviousExit(serverId: String) async throws {
        guard lifecycle.awaitPreviousExit(serverId) else { return }
        onLog?(serverId, "[Homerun] Waiting for the previous server to finish saving…")
        while lifecycle.awaitPreviousExit(serverId) {
            try await Task.sleep(nanoseconds: 200_000_000)
        }
    }

    /// Wait for the engine to report it is accepting connections, then bring
    /// the tunnel up and announce.
    private func awaitConsole(
        serverId: String, port: UInt16, order: inout LaunchOrder
    ) async throws {
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
                // The console is up, which is what turns a stop from "end it"
                // into "ask it to save and exit".
                lifecycle.consoleReady(serverId)

                if try order.at("openTunnel") {
                    // The engine is listening on loopback. That is not the same
                    // as being joinable, so the tunnel goes up before anyone is
                    // told the server is running — desktop learned this the
                    // hard way, as "a silently-rejected start masquerading as
                    // running".
                    try await openTunnel(serverId: serverId, port: Int(port))
                }

                startedAt = Date()
                listeningPort = HomerunFFI.stats()["port"] as? Int ?? Int(port)
                startHeartbeat(serverId: serverId)

                if try order.at("announceRunning") {
                    emitState(serverId, .running)
                }
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
    /// Turn the API's settings into what the engine can be told.
    ///
    /// **Never fails a launch.** A name Mojang would not resolve is dropped and
    /// said out loud on the console; settings that could not be read at all
    /// mean the engine's own configuration applies. That last case is worse
    /// here than it is on Android, and worth naming: there, "defaults" means
    /// vanilla's, written into a file we control. Here it means *Pumpkin's*,
    /// which includes `online_mode = true` — a server nobody with a cracked
    /// client can join. Which is why settings are applied even when every
    /// lookup failed, rather than skipped as a set.
    private func resolveSettings(
        serverId: String, config: ServerConfig
    ) async -> StartRequest.Settings? {
        guard !config.settingsEnv.isEmpty else {
            HostLog.host.info("\(serverId, privacy: .public): no settings to apply")
            return nil
        }

        // Nothing at all for an offline server: its UUIDs are a function of
        // the name and the core derives them itself. That is the difference
        // between a launch with no signal costing nothing and costing one
        // ten-second timeout per operator.
        let names =
            (try? Core.requiredLookups(env: config.settingsEnv, gameType: config.gameType)) ?? []
        let resolved = await MojangDirectory.identities(for: names)

        for name in names where resolved[name] == nil {
            onLog?(serverId, "[Homerun] Could not look up \"\(name)\" — skipping.")
        }

        return StartRequest.Settings(
            env: config.settingsEnv, gameType: config.gameType, resolved: resolved)
    }

    private func startServerThread(
        serverId: String, port: UInt16, settings: StartRequest.Settings?
    ) {
        let directory = HostStore.serverDirectory(id: serverId).path

        let thread = Thread { [weak self] in
            let reply = HomerunFFI.serverStart(
                serverId: serverId, dataDir: directory, port: port, settings: settings)

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

        // **Record the intent first.** The router is what tells the core a
        // stop was asked for, and this path never goes through the router — so
        // without this the core sees an engine that ended on its own, reports a
        // crash, and the on-stop backup is skipped because a crash must not
        // overwrite a good snapshot. A tunnel failure would silently cost the
        // session's play. Android shipped exactly this regression.
        let verdict = lifecycle.stopRequested(serverId)
        try? await stop(serverId: serverId, graceful: verdict.verdict == "graceful")
        lifecycle.callFinished(serverId)
    }

    private func serverThreadExited(_ reply: HomerunFFI.Reply) {
        threadFinished = true
        guard let serverId = activeServerId else { return }

        let exit = lifecycle.exited(serverId, code: reply.ok ? 0 : 1)

        // A newer launch owns this id now; this exit belongs to the run it
        // replaced and says nothing about the one coming up. Tearing down here
        // would flip a starting server to stopped — the bug that bit Android.
        guard !exit.superseded else {
            HostLog.host.info("ignoring the exit of a superseded run")
            return
        }

        if !exit.intentional {
            runFailure = reply.error ?? "The server stopped unexpectedly."
        }
        finish(serverId: serverId, state: exit.state == "crashed" ? .crashed : .stopped,
               intentional: exit.intentional)
    }

    /// Stop a running server.
    ///
    /// `graceful` is the core's verdict, and on this host both verdicts reach
    /// the same call: a linked engine has one shutdown path, and there is no
    /// SIGTERM to send instead. The distinction is still worth carrying,
    /// because it says whether a world is at stake — a server that never
    /// reached its console has saved nothing to protect, so the up-to-30 s the
    /// engine may take to notice is time nobody is waiting on.
    func stop(serverId: String, graceful: Bool) async throws {
        guard activeServerId == serverId else {
            throw ServerBackendError.notRunning(serverId)
        }

        emitState(serverId, .stopping)

        // Tunnel first: one that outlives its server keeps the gateway's peer
        // slot occupied, and the next start fails for a reason that looks
        // nothing like this one.
        tunnelTask?.cancel()
        tunnelTask = nil
        wireProxy.stop()

        // The engine saves the world and then returns. Killing the thread
        // instead would risk the save, and a half-written world is the worst
        // outcome available here.
        HostLog.host.info("stopping (graceful: \(graceful, privacy: .public))")
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
    private func finish(serverId: String, state: ServerState, intentional: Bool) {
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
        // Only a stop someone asked for is backed up. `intentional` comes from
        // the core rather than from `state != .crashed`, and the two are not
        // the same question: a server terminated after a stop request exits
        // 143, which is a stop, and reading the code alone calls it a crash and
        // silently loses the session's play.
        let context = backupContext
        backupContext = nil
        let willBackUp =
            intentional
            && context?.settings.backup != nil
            && BackupFFI.isAvailable
            && backups.hasLocalWorld(HostStore.serverDirectory(id: serverId))

        emitState(serverId, state, backupInProgress: willBackUp)

        if willBackUp, let context, let repo = context.settings.backup {
            let dir = HostStore.serverDirectory(id: serverId)
            Task { [backups] in
                await backups.backupAfterStop(
                    serverId: serverId, dir: dir, repo: repo, deviceId: context.deviceId,
                    onLog: backupLog(serverId: serverId))
            }
        }
    }

    /// Ids with an engine thread behind them.
    ///
    /// Narrower than `lifecycle.activeIds()`, which is what
    /// `native-server-active-ids` answers and which also counts a server
    /// coming up or winding down. This one feeds the instance report, where
    /// the question really is "is a server running here".
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
            // The limit this app is killed for exceeding, not the device's
            // RAM — the same question Android's `largeMemoryClass` answers.
            maxMb: DeviceMetrics.memoryLimitKb().map { $0 / 1024 })
    }

    /// The most recent rate the core worked out — the same number the graph's
    /// last point shows.
    ///
    /// Deliberately not measured on demand. This used to call `CPUSampler`,
    /// which advanced its own baseline, so every UI poll of this channel stole
    /// the anchor from the next point on the graph: the Insights screen was
    /// changing the numbers it was reading. Nil until two readings exist,
    /// which the UI renders as "unavailable".
    func cpuUsage(serverId: String) -> Double? {
        guard activeServerId == serverId else { return nil }
        return metrics.samples().last?.cpuPercent
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
    ///
    /// Under `data/`, not the server directory: Pumpkin's `LoadJSONConfiguration`
    /// puts every one of these lists there. Read from the wrong path this
    /// returned an empty list for every server, forever, and looked like a
    /// server that simply had no operators.
    func ops(serverId: String) -> [String] {
        let url = HostStore.serverDirectory(id: serverId)
            .appendingPathComponent("data")
            .appendingPathComponent("ops.json")
        guard let data = try? Data(contentsOf: url),
            let entries = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
        else { return [] }
        return entries.compactMap { $0["name"] as? String }
    }

    /// Empty when this is not the running server — the rule every sibling
    /// getter follows, and one this method used to ignore, so a stopped
    /// server still drew the previous run's graph.
    func perfSamples(serverId: String) -> [PerfSample] {
        guard activeServerId == serverId else { return [] }
        return metrics.samples().map {
            PerfSample(
                t: $0.t, memUsedMb: $0.memUsedMb, cpuPercent: $0.cpuPercent,
                playerCount: $0.playerCount)
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
        // A repeating timer does not fire on creation, so without this the
        // graph starts five seconds late and the rate anchor with it.
        sample(serverId: serverId)
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

        // Offered every tick, not once per interval. The core keeps what is due
        // and remembers the rest as the anchor for the next rate, so a
        // five-second pump feeding a thirty-second graph measures CPU over the
        // last five seconds rather than averaging a spike away.
        //
        // Deliberately not gated on `metrics.query`'s `due`: the history
        // crosses the C ABI *by value*, so once the graph is full, asking
        // whether a reading is wanted ships 360 samples in each direction to
        // save two Mach calls. `due` is for hosts where reading the counter is
        // the expensive half; here that is inverted.
        metrics.record(
            atMs: Int(Date().timeIntervalSince1970 * 1000),
            memUsedKb: DeviceMetrics.footprintKb(),
            cpuSeconds: DeviceMetrics.cpuSeconds(),
            playerCount: roster?.players.count)

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
        // The core vetoes an announcement that would contradict a stop already
        // in flight — a `running` from a launch the user has since abandoned,
        // for instance.
        //
        // Note what it does *not* veto: a state merely because it repeats.
        // There are two clocks here, the core's and this host's, and the core's
        // must not be allowed to swallow an announcement the host needs to
        // make. Android suppressed repeats and got "the server never comes
        // online".
        guard lifecycle.mayAnnounce(serverId, state: state.rawValue) else {
            HostLog.host.info(
                "not announcing \(state.rawValue, privacy: .public) — a stop is in flight")
            return
        }

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

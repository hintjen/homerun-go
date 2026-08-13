import Foundation

/// What this device tells the API about the server it runs: crashes, stats,
/// player presence, minigame results, and operator changes typed into the
/// console.
///
/// A host that never reports looks fine from the inside and is broken from
/// everywhere else — a crashed server gives the player no explanation, the
/// dashboard's graphs stay empty, presence goes stale, and an `/op` typed here
/// is taken back on the next launch when `ops.json` is rewritten from the API.
///
/// **Every decision is in `homerun-core`. Every effect is here.** The core
/// answers with a `Core.Request` — method, path, body, and *which credential
/// signs it*; this signs it, sends it, and forgets it. Nothing retries and
/// nothing fails loudly: the next report supersedes the last one.
///
/// Ported from Android's `Reporting.kt`, which is the reference. What differs
/// and why is in `docs/ios-reporting.md`; the differences are all consequences
/// of two platform facts — the console buffer lives in Rust and outlives a
/// run, and this app has no background execution.
///
/// `@MainActor` because every collaborator already is (`PumpkinBackend`,
/// `BridgeController`, `BridgeRouter`) and the blocking calls hop off
/// explicitly, which is the same posture `ScreenAwake` and `Core.Metrics`
/// take.
@MainActor
enum Reporting {

    // MARK: - What a run is

    /// What this run is, beyond what the backend can say.
    private struct RunContext {
        let loader: String
        let onlineMode: Bool?
        /// `<gateway host>:<external port>`, once the gateway has assigned one.
        /// Nil until the post-launch tunnel poll resolves — the external port
        /// does not exist when a launch begins, so reading it earlier reliably
        /// answers nil.
        var gatewayAddress: String?
    }

    private static weak var backend: PumpkinBackend?

    /// The armed run. Set by ``starting(serverId:settings:)`` and deliberately
    /// **not** cleared on stop: a crash report is built after the run has ended
    /// and still needs the loader and the online-mode flag.
    private static var context: RunContext?

    /// The server the cadence loop is reporting on, or nil when nothing runs.
    private static var runningId: String?

    /// Opaque cadence state from `reporting.stats.schedule`. Never inspected —
    /// the core owns the interval, the debounce, and the rule that a presence
    /// report resets the periodic clock.
    private static var schedule: [String: Any]?

    private static var timer: Timer?

    /// Serialises operator changes. Each is a read-modify-write of the same
    /// environment variable, so two in flight would lose one.
    private static var opsChain: Task<Void, Never>?

    /// Process-lifetime, because it cannot change while the app runs and the
    /// request is a round trip to a third party. Held here rather than in
    /// `HomerunAPI`, which has no mutable state and would need a concurrency
    /// story to get one.
    private static var publicIP: String?

    // MARK: - Wiring

    /// Called once at launch. The backend outlives every page, and so does
    /// this.
    static func attach(backend: PumpkinBackend) {
        self.backend = backend
    }

    /// Arm reporting for a launch that is about to begin.
    ///
    /// Deliberately called from the **start handler**, before the backend is
    /// asked for anything — not on the transition to running. A launch that
    /// crashes on its way up is exactly the one worth explaining, and it needs
    /// a context that already exists when it fails.
    static func starting(serverId: String, settings: HomerunAPI.ServerSettings?) {
        let loader = "vanilla"  // Pumpkin-only host; see Core.statsPoll.
        context = RunContext(
            loader: loader,
            onlineMode: settings.flatMap {
                Core.onlineMode(env: $0.env, gameType: $0.gameType, loader: loader)
            },
            gatewayAddress: nil)
        HostLog.reporting.info("reporting armed for \(serverId, privacy: .public)")
    }

    /// The address a player actually connects to, once the gateway has
    /// assigned one. Until this arrives, `gateway_ping` is null — which is
    /// correct rather than missing, and matches the desktop.
    static func gatewayAddressResolved(serverId: String, address: String) {
        guard var current = context, current.gatewayAddress != address else { return }
        current.gatewayAddress = address
        context = current
        HostLog.reporting.info(
            "\(serverId, privacy: .public) is reachable at \(address, privacy: .public)")
    }

    // MARK: - The server's own output

    /// One line of console output.
    ///
    /// Order matters and matches Android: a minigame result first, then what
    /// the line means, then the presence nudge that earns an early report.
    static func onLog(serverId: String, line: String) {
        guard serverId == runningId else { return }

        if let request = Core.minigameReport(serverId: serverId, line: line) {
            send(request)
        }

        guard let meaning = try? Core.classify(line) else { return }

        // A line that plainly announces a join but that the core did not read
        // as one. Always a parser that was written against vanilla's console
        // meeting an engine that words or formats it differently — three of
        // those have been found on Pumpkin alone (`Gametime is`,
        // `commands.list.nameandid`, and the log prefix), and every one was
        // silent: the count stayed right, the timing quietly stopped working.
        //
        // Logged with the line verbatim, because the fix is always "match the
        // string the engine actually printed" and the expensive part is
        // finding out what that was.
        if meaning.joined == nil, meaning.left == nil,
            line.contains(" joined the game") || line.contains(" left the game")
        {
            HostLog.reporting.error(
                "a presence line the core did not recognise: \(line, privacy: .public)")
        }

        if meaning.joined != nil || meaning.left != nil {
            // A join or a leave is worth reporting sooner than the next
            // periodic beat, and the core coalesces a burst of them.
            schedule = Core.schedule(held: schedule, nowMs: nowMs(), event: "presence").held
            reschedule(after: 0)
        }
    }

    /// A run began, ended, or died.
    static func onStateChanged(serverId: String, state: ServerState) {
        switch state {
        case .running:
            start(serverId: serverId)
        case .crashed:
            // Stopped *first*: the loop must not fire a stats report for a
            // server that has already died.
            stop()
            reportCrash(serverId: serverId)
        case .stopped:
            stop()
        case .starting, .stopping:
            break
        }
    }

    // MARK: - The cadence

    private static func start(serverId: String) {
        guard runningId != serverId || timer == nil else { return }  // idempotent
        runningId = serverId
        // Nil means "first call", which makes the first report due
        // immediately — a run that ends before the interval elapses would
        // otherwise never appear anywhere.
        schedule = nil
        reschedule(after: 0)
    }

    private static func stop() {
        timer?.invalidate()
        timer = nil
        runningId = nil
        schedule = nil
    }

    /// Ask the core when to report next, and set one timer to do it.
    ///
    /// A `Timer` rather than a sleeping task because that is how every other
    /// clock in this host works — the instance heartbeat, the log pump, the
    /// sampler and the tunnel watchdog. It also gives the suspend behaviour
    /// this platform needs for free: a suspended app's timers do not fire, and
    /// since the core's schedule is wall-clock, the first tick after the user
    /// comes back is due immediately. See `docs/ios-reporting.md`.
    private static func reschedule(after seconds: TimeInterval) {
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: max(seconds, 0.05), repeats: false) { _ in
            MainActor.assumeIsolated { tick() }
        }
    }

    private static func tick() {
        guard let serverId = runningId else { return }

        let decision = Core.schedule(held: schedule, nowMs: nowMs())
        schedule = decision.held

        guard let trigger = decision.trigger else {
            reschedule(after: Double(decision.waitMs) / 1_000)
            return
        }

        Task {
            await report(serverId: serverId, trigger: trigger)
            // Re-asked only once the report is done, so a slow poll cannot
            // stack two reports on top of each other.
            if runningId == serverId { reschedule(after: 0) }
        }
    }

    // MARK: - The stats report

    private static func report(serverId: String, trigger: String) async {
        guard let apiURL = HostStore.apiURL,
            let deviceId = HostStore.registeredDeviceId,
            let backend
        else { return }

        // Guarded before the round trips and again after them: a stop during a
        // poll would otherwise report a server that is no longer running.
        guard backend.runningServerIds.contains(serverId) else { return }

        let run = context
        let loader = run?.loader ?? "vanilla"

        // Blocking — a console round trip and a socket. Off the main actor,
        // the same way the backend dispatches a console command.
        let poll = await Task.detached { Core.statsPoll(loader: loader) }.value

        let cpu = backend.cpuUsage(serverId: serverId).flatMap {
            Core.cpuPercentOfDevice(
                perCorePercent: $0, cores: ProcessInfo.processInfo.activeProcessorCount)
        }

        var ping: Double?
        if let address = run?.gatewayAddress {
            ping = await Task.detached { Core.gatewayPing(address: address) }.value
        }

        if publicIP == nil { publicIP = await HomerunAPI.fetchPublicIPAddress() }

        guard backend.runningServerIds.contains(serverId) else { return }

        // Every field omitted when nil rather than sent as a zero: the API
        // distinguishes "unknown" from "none", and a zero it did not measure
        // draws a line on a graph that never happened.
        var stats: [String: Any] = [:]
        if let started = backend.uptime(serverId: serverId) {
            stats["serverRuntime"] = ISO8601DateFormatter().string(from: started)
        }
        if let usedKb = backend.memoryUsage(serverId: serverId)?.usedKb {
            stats["memoryKb"] = usedKb
        }
        if let cpu { stats["cpuPercent"] = cpu }
        if let roster = poll.roster { stats["roster"] = roster }
        if let onlineMode = run?.onlineMode { stats["onlineMode"] = onlineMode }
        if let age = poll.ageSecs { stats["serverAgeSecs"] = age }
        if let publicIP { stats["hostIp"] = publicIP }
        if let ping { stats["gatewayPingMs"] = ping }

        guard
            let request = Core.statsReport(
                serviceId: serverId, deviceId: deviceId, stats: stats)
        else { return }

        await perform(request, apiURL: apiURL)

        // Names every field it sent, so checking a report is a reading
        // exercise rather than an investigation. Matches Android's line
        // exactly — the same words are searched for on both platforms.
        let players = (poll.roster?["count"] as? Int).map(String.init) ?? "?"
        let age = poll.ageSecs.map { String(Int($0)) } ?? "?"
        let cpuText = cpu.map { String(format: "%.1f", $0) } ?? "?"
        let pingText = ping.map { String(Int($0)) } ?? "?"
        HostLog.reporting.info(
            "reported \(serverId, privacy: .public) (\(trigger, privacy: .public)): players=\(players, privacy: .public) age=\(age, privacy: .public)s cpu=\(cpuText, privacy: .public)% ping=\(pingText, privacy: .public)ms"
        )
    }

    // MARK: - Crashes

    /// Report a run that died on its own.
    ///
    /// The console comes from the engine's own buffer rather than a copy kept
    /// here. On this platform that buffer outlives the run — it is cleared by
    /// the *next* launch, and `finish()` drains it once more on the way out to
    /// catch the dying words — so a second copy would be a copy of something
    /// that is already there. Android keeps a tail because its backend stops
    /// answering the moment a run ends.
    private static func reportCrash(serverId: String) {
        guard let apiURL = HostStore.apiURL,
            let deviceId = HostStore.registeredDeviceId,
            let backend
        else { return }

        let lines = backend.logs(serverId: serverId, since: 0).lines
        guard !lines.isEmpty else {
            HostLog.reporting.error(
                "\(serverId, privacy: .public) crashed with an empty console — nothing to report")
            return
        }

        // The local reading first, because it is what the player sees while
        // the API is still deciding. Nil is the ordinary outcome here: these
        // patterns are JVM strings and Pumpkin produces none of them, so the
        // player gets the API's own message rather than a wrong local one.
        if let diagnosis = Core.crashDiagnosis(lines: lines) {
            HostLog.reporting.error(
                "\(serverId, privacy: .public) crashed: \(diagnosis.cause, privacy: .public)")
            backend.note(serverId: serverId, line: diagnosis.message)
        }

        guard
            let request = Core.crashReport(
                serverId: serverId, deviceId: deviceId, lines: lines)
        else { return }
        Task { await perform(request, apiURL: apiURL) }
    }

    // MARK: - Operator changes

    /// A command the player typed into the console.
    ///
    /// Most are not operator changes and cost one core call to establish.
    static func consoleCommand(serverId: String, command: String) {
        guard let parsed = Core.opsCommand(command) else { return }

        // Captured **before** the task is created. Reading the property from
        // inside the task would read it as it stands when the body runs — by
        // which point it is already this task, so it would wait for itself and
        // the sync would never happen. Android shipped exactly that, and it
        // failed in the worst way available: no error, no timeout, no log line.
        let previous = opsChain
        opsChain = Task {
            await previous?.value
            await syncOps(serverId: serverId, command: parsed)
        }
    }

    /// Save an operator change to the server's settings.
    ///
    /// Every branch says what it decided. The two most likely failures are a
    /// missing signature and a settings read that did not come back, and both
    /// are silent by nature.
    private static func syncOps(serverId: String, command: [String: Any]) async {
        guard let apiURL = HostStore.apiURL else { return }

        // The **user's** token, and no fall back to the device's. This is a
        // settings change, and the API answers a device-signed one with 200
        // and strips it — a silent success is worse than a refusal.
        guard let token = TokenStore.accessToken, !token.isEmpty else {
            HostLog.reporting.info(
                "nobody is signed in — \(serverId, privacy: .public) keeps this change only until it restarts"
            )
            return
        }

        guard let body = await HomerunAPI.serverBody(apiURL: apiURL, serverId: serverId, token: token)
        else {
            HostLog.reporting.error(
                "could not read \(serverId, privacy: .public) to change its operators")
            return
        }

        guard let change = Core.opsSync(command: command, serverBody: body, serverId: serverId)
        else {
            HostLog.reporting.info(
                "\(serverId, privacy: .public): the settings already say this, nothing to save")
            return
        }

        HostLog.reporting.info(
            "\(serverId, privacy: .public): saving \(change.request.method, privacy: .public) \(change.request.path, privacy: .public)"
        )

        guard await HomerunAPI.perform(apiURL: apiURL, request: change.request, token: token) != nil
        else {
            HostLog.reporting.error(
                "the API did not accept the change for \(serverId, privacy: .public)")
            return
        }

        // Only once it is saved. Telling the player it was kept and then
        // losing it on the next launch is the failure this whole path exists
        // to prevent.
        backend?.note(serverId: serverId, line: change.line)
    }

    // MARK: - Sending

    private static func send(_ request: Core.Request) {
        guard let apiURL = HostStore.apiURL else { return }
        Task { await perform(request, apiURL: apiURL) }
    }

    /// Sign a request the way the **core** said to, never the way the call
    /// site assumed.
    private static func perform(_ request: Core.Request, apiURL: String) async {
        let token = request.userSigned ? TokenStore.accessToken : TokenStore.deviceToken
        guard let token, !token.isEmpty else {
            HostLog.reporting.error(
                "no credential for \(request.path, privacy: .public) — not sent")
            return
        }
        _ = await HomerunAPI.perform(apiURL: apiURL, request: request, token: token)
    }

    /// Wall clock, in milliseconds, which is what the core's schedule expects.
    private static func nowMs() -> Int { Int(Date().timeIntervalSince1970 * 1_000) }
}

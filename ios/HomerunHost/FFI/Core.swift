import Foundation

/// The decisions this app shares with the desktop and Android, in
/// `homerun-core`.
///
/// # Why these live in Rust
///
/// Everything reachable through here was, until recently, written once in the
/// desktop's TypeScript and again in each mobile app, from the same reference.
/// They had already drifted. So the *decisions* moved to one tested place and
/// the platforms kept what only they can do: this app still makes every HTTP
/// request and owns every file; it just stops deciding what any of it means.
///
/// # The shape
///
/// One native entry point — `homerun_core_call(method, argsJson)` — replying
/// `{ok:true, value}` or `{ok:false, error}`. This is the same dispatch Android
/// reaches over JNI (`core_dispatch::call` in `homerun-pumpkin-ffi`), so the
/// two platforms cannot disagree about what a method means, only about how a
/// string crosses the boundary.
///
/// Failures are ``CoreError`` and they are *verdicts* — "this loader needs an
/// installer", "that version does not exist" — carrying text meant for a
/// player, not a stack trace. Do not reword them here; fix them in the core,
/// where every platform shares the wording.
enum Core {

    /// The game this app hosts today. The only place it is named.
    static let minecraft = "minecraft-java"

    struct CoreError: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    // MARK: - The boundary

    /// Call into the core.
    ///
    /// - Throws: ``CoreError`` when the core says no, with its wording intact.
    static func call(_ method: String, _ args: [String: Any] = [:]) throws -> Any? {
        let argsData = try JSONSerialization.data(withJSONObject: args)
        guard let argsJSON = String(data: argsData, encoding: .utf8) else {
            throw CoreError(message: "Arguments could not be encoded.")
        }

        // The reply is owned by Rust until freed, so free it on every path —
        // leaking these leaks a JSON document per call.
        guard let reply = homerun_core_call(method, argsJSON) else {
            throw CoreError(message: "The native core did not answer.")
        }
        defer { homerun_free_string(reply) }

        let text = String(cString: reply)
        guard
            let data = text.data(using: .utf8),
            let envelope = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            throw CoreError(message: "The native core answered with nonsense.")
        }

        guard envelope["ok"] as? Bool == true else {
            throw CoreError(message: envelope["error"] as? String ?? "The native core refused.")
        }
        return envelope["value"]
    }

    private static func object(_ method: String, _ args: [String: Any]) throws -> [String: Any] {
        guard let value = try call(method, args) as? [String: Any] else {
            throw CoreError(message: "\(method) did not return an object.")
        }
        return value
    }

    private static func array(_ method: String, _ args: [String: Any]) throws -> [Any] {
        guard let value = try call(method, args) as? [Any] else {
            throw CoreError(message: "\(method) did not return a list.")
        }
        return value
    }

    private static func string(_ method: String, _ args: [String: Any]) throws -> String {
        guard let value = try call(method, args) as? String else {
            throw CoreError(message: "\(method) did not return a string.")
        }
        return value
    }

    private static func bool(_ method: String, _ args: [String: Any]) throws -> Bool {
        guard let value = try call(method, args) as? Bool else {
            throw CoreError(message: "\(method) did not return a yes or no.")
        }
        return value
    }

    /// A JSON number, or nil for a JSON null and for anything that is not one.
    ///
    /// Through `NSNumber` on purpose. Swift's conditional bridge is *exact*, so
    /// `0.6 as? Int` is nil rather than 0 — and a field like `cpuPercent` is
    /// fractional by design, because an idle server sits well under one
    /// percent. `NSNull as? NSNumber` is nil, which is what keeps "the platform
    /// would not say" from decoding as a measured zero.
    private static func number(_ raw: Any?) -> NSNumber? {
        raw as? NSNumber
    }

    // MARK: - Config, through the game capability surface

    /// How a config file must be read and written.
    ///
    /// Carried per file so a host never has to know that one game's config is
    /// latin-1 and another's is UTF-8. Getting it wrong is invisible until a
    /// player sees mojibake in a server list.
    enum Encoding: String {
        case utf8
        case latin1

        var stringEncoding: String.Encoding {
            switch self {
            case .utf8: return .utf8
            // `§` — the colour-code marker in a MOTD — is one byte here and
            // two in UTF-8. Reading or writing with the wrong one destroys it.
            case .latin1: return .isoLatin1
            }
        }

        static func parse(_ raw: Any?) -> Encoding {
            Encoding(rawValue: raw as? String ?? "") ?? .utf8
        }
    }

    struct ConfigInput {
        let path: String
        let encoding: Encoding
    }

    struct ConfigFile {
        let path: String
        let contents: String
        let encoding: Encoding
    }

    struct Identity {
        let name: String
        let id: String
    }

    /// Which files to read before building config, and how to decode each.
    static func configInputs(env: [String: Any], game: String = minecraft) throws -> [ConfigInput] {
        try array("game.configInputs", ["game": game, "env": env]).compactMap { entry in
            guard let entry = entry as? [String: Any], let path = entry["path"] as? String else {
                return nil
            }
            return ConfigInput(path: path, encoding: .parse(entry["encoding"]))
        }
    }

    /// Names the host must resolve over the network.
    ///
    /// Only what the game cannot derive itself: an offline Minecraft server
    /// returns nothing here and costs no requests, because its UUIDs are a
    /// function of the name and the core derives them internally.
    static func requiredLookups(
        env: [String: Any],
        gameType: String,
        game: String = minecraft
    ) throws -> [String] {
        try array("game.requiredLookups", ["game": game, "env": env, "gameType": gameType])
            .compactMap { ($0 as? [String: Any])?["name"] as? String }
    }

    /// Dash Mojang's 32-character hex id.
    ///
    /// The one game-specific call the host still makes, because fetching the
    /// profile is the host's job and the response shape comes with it. Throws
    /// if the value is not a 32-character hex id.
    static func dashUuid(_ undashed: String) throws -> String {
        try string("minecraft.settings.dashUuid", ["undashed": undashed])
    }

    /// The files to write, given everything the host gathered.
    ///
    /// `existing` is keyed by the paths ``configInputs(env:game:)`` named; a
    /// file that does not exist is simply left out. `resolved` carries whatever
    /// identities the host managed to fetch — a name missing from it is the
    /// game's to handle, and Minecraft skips it rather than writing an id that
    /// cannot match.
    static func configFiles(
        env: [String: Any],
        gameType: String,
        port: Int,
        bindAddress: String,
        existing: [String: String],
        resolved: [Identity],
        now: String,
        game: String = minecraft
    ) throws -> [ConfigFile] {
        let context: [String: Any] = [
            "env": env,
            "game_type": gameType,
            "port": port,
            "bind_address": bindAddress,
            "existing": existing,
            "resolved": resolved.map { ["name": $0.name, "id": $0.id] },
            "now": now,
        ]
        return try array("game.configFiles", ["game": game, "context": context])
            .compactMap { entry in
                guard
                    let entry = entry as? [String: Any],
                    let path = entry["path"] as? String,
                    let contents = entry["contents"] as? String
                else { return nil }
                return ConfigFile(
                    path: path, contents: contents, encoding: .parse(entry["encoding"]))
            }
    }

    // MARK: - The tunnel

    /// Render the wireproxy config.
    ///
    /// Byte-exact against the desktop's generator, and tested that way — the
    /// gateway is the same on every platform, so a divergence is a bug by
    /// definition. Every `ListenPort` is fixed by what the gateway DNATs to;
    /// only `Target` follows the local port.
    static func renderTunnel(
        link: [String: Any],
        port: Int,
        exposure: String = "java",
        geyserPort: Int? = nil,
        voiceChatPort: Int? = nil,
        game: String = minecraft
    ) throws -> String {
        var args: [String: Any] = [
            "game": game, "link": link, "port": port, "exposure": exposure,
        ]
        if let geyserPort { args["geyserPort"] = geyserPort }
        if let voiceChatPort { args["voiceChatPort"] = voiceChatPort }
        return try string("tunnel.render", args)
    }

    /// The tunnel on a `/api/server/<id>/` body, or nil if none yet.
    static func linkFromServerBody(_ body: [String: Any]) throws -> [String: Any]? {
        try call("link.fromServerBody", ["body": body]) as? [String: Any]
    }

    // MARK: - The device's own link

    /// The tunnel on a `link_up` result body, or nil while the task runs.
    ///
    /// Null is not a failure. The API answers with no `native_config` for the
    /// first several seconds, and a caller that treats that as one abandons a
    /// link that was about to be provisioned.
    ///
    /// A **device** link, not a server one: it arrives flat rather than nested
    /// under `config.links[]`, which is why this is not `linkFromServerBody`.
    static func deviceLinkFromBody(_ body: [String: Any]) throws -> DeviceLink? {
        guard let object = try call("deviceWs.fromLinkUpBody", ["body": body]) as? [String: Any],
            let link = object["link"] as? [String: Any]
        else { return nil }

        return DeviceLink(
            link: link,
            fqdn: (object["fqdn"] as? String).flatMap { $0.isEmpty ? nil : $0 },
            // The core answers `gateway_v2`; this host cares about the
            // consequence rather than the provenance. Getting it backwards is
            // not a warning — the header lands where a ClientHello is expected
            // and every handshake fails.
            expectsProxyProtocol: (object["gateway_v2"] as? Bool) != true)
    }

    /// What `POST /api/device/<id>/link_up/` provisioned.
    struct DeviceLink {
        /// Opaque here: handed straight back to `deviceWs.tunnelConfig`, so
        /// this host never learns the shape of a key it does not need to read.
        let link: [String: Any]
        /// The ACME identifier, the TLS SNI, and what the dashboard dials.
        /// Absent means the API has not named this device, which is a link
        /// that carries traffic but cannot be reached by name.
        let fqdn: String?
        let expectsProxyProtocol: Bool
    }

    /// The wireproxy config for the device websocket's own tunnel.
    ///
    /// A nil `httpTarget` omits the ACME forward, which is the shape a device
    /// with no certificate takes — forwarding a port at a listener that was
    /// never started is worse than not forwarding it.
    static func deviceWsTunnelConfig(
        link: [String: Any], httpsTarget: Int, httpTarget: Int?
    ) throws -> String {
        var args: [String: Any] = ["link": link, "httpsTarget": httpsTarget]
        if let httpTarget { args["httpTarget"] = httpTarget }
        return try string("deviceWs.tunnelConfig", args)
    }

    /// False when these are the dead credentials from the previous session.
    static func linkIsUsable(polled: [String: Any], before: [String: Any]?) throws -> Bool {
        var args: [String: Any] = ["polled": polled]
        if let before { args["before"] = before }
        return try call("link.isUsable", args) as? Bool ?? false
    }

    // MARK: - Lifecycle

    // `state.exit` had a wrapper here and no caller. The backend classified
    // exits itself, which was the drift the lifecycle port ended — it now asks
    // `Lifecycle.exited`, and the core reaches `exit_state` on its own behalf.
    // The dispatch method stays; only this host's unused door onto it is gone.

    /// One line of tunnel output against a running count.
    ///
    /// `watch` is opaque state, held by the caller and handed back each line —
    /// so there is no native allocation to remember to free, and the threshold
    /// and its reset rule stay in one place shared with the desktop.
    struct Handshake {
        let watch: [String: Any]
        let giveUp: Bool
        let recovered: Bool
    }

    static func observeHandshake(watch: [String: Any]?, line: String) throws -> Handshake {
        var args: [String: Any] = ["line": line]
        if let watch { args["watch"] = watch }
        let reply = try object("state.handshake", args)
        return Handshake(
            watch: reply["watch"] as? [String: Any] ?? [:],
            giveUp: reply["giveUp"] as? Bool ?? false,
            recovered: reply["recovered"] as? Bool ?? false
        )
    }

    // MARK: - The launch order

    /// One thing a host does during a launch, in order.
    struct Step: Equatable {
        let name: String
        /// A stop that arrived during the launch must be honoured *before*
        /// this step, not after it.
        let checkpoint: Bool
    }

    /// The steps this launch runs, given what it has to work with.
    ///
    /// `engine: "linked"` is what this host is — Pumpkin is compiled in, so
    /// there is no jar to fetch and no `Main-Class` to read, and the core
    /// leaves those two out rather than handing over steps that have no
    /// meaning here.
    ///
    /// Two JVM-sounding steps stay in the plan regardless: `ensureRuntime`
    /// unpacks a bundled payload and `acceptEula` writes a file into the server
    /// directory, and neither is about the jar. This host skips both by not
    /// asking for them, which is allowed — `LaunchOrder` requires
    /// monotonicity, not exhaustiveness.
    static func launchPlan(
        backups: Bool, settings: Bool, tunnel: Bool, engine: String = "linked"
    ) throws -> [Step] {
        try array(
            "launch.plan",
            ["backups": backups, "settings": settings, "tunnel": tunnel, "engine": engine])
            .compactMap { entry in
                guard let entry = entry as? [String: Any], let name = entry["step"] as? String
                else { return nil }
                return Step(name: name, checkpoint: entry["checkpoint"] as? Bool ?? false)
            }
    }

    /// Why this device will not host this server, or nil to go ahead.
    ///
    /// Asked before the launch plan, because everything expensive comes after
    /// it. This host links its engine, and a linked engine does not refuse a
    /// modpack — it starts vanilla and looks like it worked, so the player
    /// sees a world with their mods missing and no error anywhere.
    ///
    /// `gameType` must be the API's verbatim value: the reduced java/bedrock
    /// form cannot tell `native-crossplay` apart, and crossplay needs Geyser.
    /// `bedrock` is false because no phone ships Bedrock Dedicated Server.
    static func hostingRefusal(
        gameType: String, env: [String: Any], engine: String = "linked", bedrock: Bool = false
    ) throws -> String? {
        let reply = try call(
            "minecraft.hosting.refuse",
            [
                "host": ["engine": engine, "bedrock": bedrock],
                "server": ["gameType": gameType, "env": env],
            ])
        guard let refusal = reply as? [String: Any] else { return nil }
        return refusal["message"] as? String
    }

    // MARK: - Who owns a server right now

    /// The lifecycle of the servers this device hosts.
    ///
    /// The host reports what only it can see — a call arrived, a thread
    /// spawned, a run ended — and the core answers what any of it means.
    ///
    /// # Why this is not a handful of flags on the backend any more
    ///
    /// It was: an `activeServerId`, a `claimedServerId`, a `stopRequested`
    /// bool, and an `inFlight` count on the router. Four places that had to
    /// agree about one question, and the same class of bug kept coming back —
    /// a server that is *starting* or *stopping* is still this device's, and
    /// reporting otherwise makes the UI's reconcile loop take a launch for a
    /// remote start and reprovision the gateway underneath it. That is a
    /// tunnel that handshakes and carries nothing.
    ///
    /// State is opaque and lives here, exactly as ``Handshake`` does: it goes
    /// in, a new one comes back, and there is no native handle to free.
    ///
    /// `@MainActor` rather than a lock. Two clocks reach this — the bridge's
    /// handlers and the engine thread's hop back to main — and both are
    /// already main-actor by the rule `ServerBackend` states. Anything calling
    /// from `BackupFFI`'s dedicated threads must hop first.
    @MainActor
    final class Lifecycle {
        private let concurrency: String
        private var state: [String: Any]?

        /// `one` matches `multipleRunningServers: false` in the iOS profile.
        init(concurrency: String = "one") {
            self.concurrency = concurrency
        }

        /// Everything the core answers about a server after an event.
        struct View {
            let verdict: String?
            /// On `anotherServerRunning`, the one in the way.
            let serverId: String?
            let activeIds: [String]
            let runningIds: [String]
            let state: String
            let shouldAbandon: Bool
            /// A previous engine is still alive; do not spawn until it is gone.
            let awaitPreviousExit: Bool
            /// Starting cancels any on-stop backup of this server still running.
            let supersedesOnStopBackup: Bool
            let intentional: Bool
            let superseded: Bool
            /// Only meaningful when a state was asked about; true otherwise.
            let mayAnnounce: Bool
        }

        // MARK: Events

        /// A start call arrived. Call this **first**, before the lookups a
        /// start needs: a server not yet counted active is one the reconcile
        /// loop will try to start for itself.
        @discardableResult
        func startRequested(_ serverId: String) -> View { apply("startRequested", serverId) }

        /// `graceful`, `terminate`, `abandonLaunch`, or `notRunning`.
        @discardableResult
        func stopRequested(_ serverId: String) -> View { apply("stopRequested", serverId) }

        /// Always, in a `defer`, whatever the verdict was — including the
        /// verdicts that did nothing. A duplicate start that returns
        /// `alreadyRunning` without finishing retires the winner's marker.
        func callFinished(_ serverId: String) { apply("callFinished", serverId) }

        func spawned(_ serverId: String) { apply("spawned", serverId) }
        func consoleReady(_ serverId: String) { apply("consoleReady", serverId) }
        func abandoned(_ serverId: String) { apply("abandoned", serverId) }

        /// What the exit meant: the state, whether anyone asked for it, and
        /// whether it belongs to a launch that has since been replaced.
        @discardableResult
        func exited(_ serverId: String, code: Int) -> View {
            apply("exited", serverId, code: code)
        }

        // MARK: Queries

        /// `native-server-active-ids`: running, coming up, or winding down.
        func activeIds() -> [String] { query("").activeIds }
        func runningIds() -> [String] { query("").runningIds }
        func shouldAbandon(_ serverId: String) -> Bool { query(serverId).shouldAbandon }

        /// Asked immediately before spawning rather than at admission: the
        /// outgoing engine usually exits while the new launch is preparing.
        func awaitPreviousExit(_ serverId: String) -> Bool { query(serverId).awaitPreviousExit }

        func supersedesOnStopBackup(_ serverId: String) -> Bool {
            query(serverId).supersedesOnStopBackup
        }

        /// False when announcing this would contradict a stop already in
        /// flight.
        ///
        /// Takes the wire string rather than `ServerState` so this file stays
        /// independent of the backend's types — it is the FFI layer, and the
        /// core answers in the same strings.
        func mayAnnounce(_ serverId: String, state: String) -> Bool {
            query(serverId, announcing: state).mayAnnounce
        }

        // MARK: Plumbing

        @discardableResult
        private func apply(_ event: String, _ serverId: String, code: Int? = nil) -> View {
            var args: [String: Any] = [
                "concurrency": concurrency, "event": event, "serverId": serverId,
            ]
            if let state { args["lifecycle"] = state }
            if let code { args["code"] = code }
            return absorb(call: "lifecycle.apply", args, keepState: true)
        }

        private func query(_ serverId: String, announcing: String? = nil) -> View {
            var args: [String: Any] = ["concurrency": concurrency, "serverId": serverId]
            if let state { args["lifecycle"] = state }
            if let announcing { args["state"] = announcing }
            return absorb(call: "lifecycle.query", args, keepState: false)
        }

        /// Make the call and take the new state from it.
        ///
        /// Non-throwing on purpose. Every caller is on a path where there is
        /// nothing useful to do with a failure — a start that cannot ask the
        /// core is refused below, and an exit still has to tear the run down —
        /// and a throw out of the engine thread's hop would be worse than a
        /// conservative answer. A failure here is a bug in the arguments,
        /// which `ios/coretest` is there to catch before a device does.
        private func absorb(call method: String, _ args: [String: Any], keepState: Bool) -> View {
            guard let reply = try? Core.object(method, args) else {
                HostLog.host.error("lifecycle \(method, privacy: .public) failed")
                return View(
                    verdict: nil, serverId: nil, activeIds: [], runningIds: [], state: "stopped",
                    shouldAbandon: false, awaitPreviousExit: false,
                    supersedesOnStopBackup: false, intentional: false, superseded: false,
                    mayAnnounce: true)
            }
            if keepState, let carried = reply["lifecycle"] as? [String: Any] {
                state = carried
            }
            return View(
                verdict: reply["verdict"] as? String,
                serverId: reply["serverId"] as? String,
                activeIds: reply["activeIds"] as? [String] ?? [],
                runningIds: reply["runningIds"] as? [String] ?? [],
                state: reply["state"] as? String ?? "stopped",
                shouldAbandon: reply["shouldAbandon"] as? Bool ?? false,
                awaitPreviousExit: reply["awaitPreviousExit"] as? Bool ?? false,
                supersedesOnStopBackup: reply["supersedesOnStopBackup"] as? Bool ?? false,
                intentional: reply["intentional"] as? Bool ?? false,
                superseded: reply["superseded"] as? Bool ?? false,
                // Absent means "not asked", which is not a veto.
                mayAnnounce: reply["mayAnnounce"] as? Bool ?? true)
        }
    }

    // MARK: - What a run is costing

    /// One run's performance graph, kept by `homerun-core::metrics`.
    ///
    /// The host reads **counters** — resident KiB, cumulative CPU seconds — and
    /// offers them here. It never computes a percentage: that is a difference
    /// between two moments, and it is where wrong graphs come from. The core
    /// decides what a reading means, whether it is due, and how much history to
    /// keep, so a phone's graph of a server covers the same span as the
    /// desktop's graph of the same server.
    ///
    /// One instance per **run**. A graph covers a session; a restart starts a
    /// new one, so ``reset()`` is called from `start`, not from a constructor.
    ///
    /// State is opaque and lives here, exactly as ``Lifecycle``'s does.
    ///
    /// `@MainActor` where the Kotlin twin is `@Synchronized`: its sampler runs
    /// on its own coroutine while the bridge reads the graph from another,
    /// where every caller on this side is already main-actor. The asymmetry is
    /// deliberate — do not "fix" it by inventing a concurrency story this host
    /// does not have.
    @MainActor
    final class Metrics {
        private var state: [String: Any]?

        /// The interval the core is currently keeping, from the last
        /// ``record(atMs:memUsedKb:cpuSeconds:playerCount:)``.
        ///
        /// Diagnostics only — nothing on iOS schedules off it, because the
        /// sampler offers a reading every tick and lets the core drop what it
        /// does not want. Nil until the first reading rather than defaulting to
        /// a number this host has not been told.
        private(set) var intervalMs: Int?

        /// Start a fresh session. Everything sampled so far is dropped.
        func reset() {
            state = nil
            intervalMs = nil
        }

        /// Offer a reading. Returns whether it became a point on the graph.
        ///
        /// Offering more often than the core keeps is fine: the extra readings
        /// still anchor the next rate, so a five-second pump feeding a
        /// thirty-second graph measures CPU over the last five seconds rather
        /// than averaging a spike away.
        ///
        /// `atMs` is a parameter rather than a `Date()` taken in here because
        /// the clock belongs to the host — the core deliberately has none — and
        /// because it is what lets `ios/coretest` drive three hours of history
        /// without waiting for it.
        @discardableResult
        func record(atMs: Int, memUsedKb: Int?, cpuSeconds: Double?, playerCount: Int?) -> Bool {
            var reading: [String: Any] = ["atMs": atMs]
            // Omitted rather than sent as null, matching the Kotlin twin. The
            // core defaults each one to `None`, which is "the platform would
            // not say" — never a zero it did not measure.
            if let memUsedKb { reading["memUsedKb"] = memUsedKb }
            if let cpuSeconds { reading["cpuSeconds"] = cpuSeconds }
            if let playerCount { reading["playerCount"] = playerCount }

            var args: [String: Any] = ["reading": reading]
            if let state { args["history"] = state }

            guard let reply = try? Core.object("metrics.record", args) else {
                // The state is deliberately left alone: dropping the history
                // over one failed call would silently restart the graph, which
                // reads as a server that just launched.
                HostLog.host.error("metrics.record failed — this reading is lost")
                return false
            }

            state = reply["history"] as? [String: Any] ?? state

            let interval = Core.number(reply["intervalMs"])?.intValue
            if let interval, interval != intervalMs {
                // The one moment a graph visibly changes meaning. Otherwise
                // invisible, and "why did my graph get chunky" is a support
                // conversation without it.
                HostLog.host.info("metrics now keep one point per \(interval / 1000)s")
            }
            intervalMs = interval

            return reply["appended"] as? Bool ?? false
        }

        /// The graph, oldest first.
        func samples() -> [Sample] {
            var args: [String: Any] = [:]
            if let state { args["history"] = state }

            guard let reply = try? Core.object("metrics.query", args),
                let entries = reply["samples"] as? [Any]
            else {
                HostLog.host.error("metrics.query failed")
                return []
            }

            return entries.compactMap { entry in
                guard let entry = entry as? [String: Any] else { return nil }
                return Sample(
                    t: Core.number(entry["t"])?.intValue ?? 0,
                    memUsedMb: Core.number(entry["memUsedMb"])?.intValue,
                    // Not rounded here: an idle server is a fraction of a
                    // percent, and the graph is the only thing entitled to
                    // decide how to show that.
                    cpuPercent: Core.number(entry["cpuPercent"])?.doubleValue,
                    playerCount: Core.number(entry["playerCount"])?.intValue)
            }
        }

        /// One point on a graph. Nulls render as "unavailable", not as zero.
        struct Sample {
            let t: Int
            let memUsedMb: Int?
            let cpuPercent: Double?
            let playerCount: Int?
        }
    }

    // MARK: - Console

    /// What one line of server output means, if anything.
    struct Line {
        let ready: Bool
        let joined: String?
        let left: String?
    }

    static func classify(_ line: String, game: String = minecraft) throws -> Line {
        let reply = try object("game.classify", ["game": game, "line": line])
        return Line(
            ready: reply["ready"] as? Bool ?? false,
            joined: reply["joined"] as? String,
            left: reply["left"] as? String
        )
    }

    // MARK: - Backups
    //
    // Decisions only. Nothing here opens a repository or moves a byte — that is
    // the engine's job, and keeping the two apart is what lets a host that
    // spawns a binary and a host that links a library reach the same answers.

    /// What to do with the local world before launching.
    enum Restore {
        /// The dashboard pinned a snapshot. Unconditional, and one-shot.
        case rollback(snapshotId: String)
        /// Pull the newest snapshot over the local world. `reason` is
        /// `anotherDeviceIsNewer` or `localWorldMissing`, and the two are
        /// worded differently to the player.
        case latest(snapshotId: String, reason: String)
        /// Keep what is on disk.
        case skip(reason: String)
    }

    static func restoreDecision(
        pinned: String?,
        latest: [String: Any]?,
        deviceId: String,
        hasLocalWorld: Bool
    ) throws -> Restore {
        var args: [String: Any] = ["deviceId": deviceId, "hasLocalWorld": hasLocalWorld]
        if let pinned { args["pinned"] = pinned }
        // Omitted rather than sent as null: absent is how the core reads "no
        // snapshot to compare against", which is a normal first launch.
        if let latest { args["latest"] = latest }

        let reply = try object("backup.restoreDecision", args)
        // Variant names are camelCase; the fields inside them are not — the
        // core tags variants with `action` and leaves `snapshot_id` snake.
        let snapshotId = reply["snapshot_id"] as? String
        let reason = reply["reason"] as? String ?? ""

        switch reply["action"] as? String {
        case "rollback":
            guard let snapshotId else {
                throw CoreError(message: "The backup to roll back to was not named.")
            }
            return .rollback(snapshotId: snapshotId)
        case "restoreLatest":
            guard let snapshotId else {
                throw CoreError(message: "The backup to restore was not named.")
            }
            return .latest(snapshotId: snapshotId, reason: reason)
        default:
            return .skip(reason: reason)
        }
    }

    /// Whether the backup lease permits a launch.
    enum Lease {
        case launch
        case blocked(device: String)
        case forced(takenFrom: String)
    }

    static func leaseDecision(leaseDevice: String?, deviceId: String, force: Bool) throws -> Lease {
        var args: [String: Any] = ["deviceId": deviceId, "force": force]
        if let leaseDevice { args["leaseDevice"] = leaseDevice }

        let reply = try object("backup.leaseDecision", args)
        switch reply["action"] as? String {
        case "blocked":
            return .blocked(device: reply["device"] as? String ?? "")
        case "forced":
            return .forced(takenFrom: reply["taken_from"] as? String ?? "")
        default:
            return .launch
        }
    }

    /// The no-world guard: refuses to push an empty snapshot over a good one.
    static func shouldBackUp(hasLocalWorld: Bool) throws -> Bool {
        try bool("backup.shouldBackUp", ["hasLocalWorld": hasLocalWorld])
    }

    /// What an engine failure means.
    struct Failure {
        /// `authRace`, `staleLocalLock`, `lockedByOther`, `completedWithWarnings`,
        /// `transient` or `fatal`.
        let kind: String
        let retryable: Bool
        /// True when the snapshot exists despite the failure.
        let succeeded: Bool
    }

    /// Normalise an engine failure.
    ///
    /// > **Only ever call this on a failure.** The core reads an empty message
    /// > with no exit code as `fatal`, so classifying a success reports one.
    /// > A snapshot came back or it did not; that is the success test.
    ///
    /// The core's `classify` also takes an exit code, and this deliberately
    /// does not offer one. A linked engine has no exit code to report, and the
    /// one value that would matter — restic's exit 3, "completed with
    /// warnings" — is reachable *only* from a real code. Synthesising one here
    /// would move the meaning of "3" out of the core and into this host, which
    /// is the drift the core exists to prevent. If a linked engine needs to say
    /// "written, but something was skipped", that belongs in `backup.rs`.
    static func classifyBackupFailure(message: String, host: String) throws -> Failure {
        let reply = try object("backup.classify", ["message": message, "host": host])
        return Failure(
            kind: (reply["failure"] as? [String: Any])?["kind"] as? String ?? "fatal",
            retryable: reply["retryable"] as? Bool ?? false,
            succeeded: reply["succeeded"] as? Bool ?? false
        )
    }

    /// The directory name a snapshot recorded a path under, if it has one.
    static func recordedBasename(_ path: String) throws -> String? {
        try call("backup.recordedBasename", ["path": path]) as? String
    }

    /// A recorded path in the form an engine selector wants.
    ///
    /// Folds `C:\Users\me\srv` to `/C/Users/me/srv`. The drive colon
    /// disappearing is the point: a `SNAP:PATH` selector splits on the first
    /// colon, so a Windows-written path that skipped this selects nothing —
    /// silently, without erroring.
    static func internalPath(_ path: String) throws -> String {
        try string("backup.internalPath", ["path": path])
    }

    /// The `POST /backup-state/` body, and whether sending it closes the lease.
    struct Report {
        let body: [String: Any]
        let releasesLease: Bool
    }

    /// Build a backup-state report.
    ///
    /// Passing `error` makes it a failure; omitting it makes it a success. Both
    /// release the lease for a backup, which is why a failed backup must still
    /// be reported — the lease has no timeout, and a device that claims it and
    /// stays quiet locks every other device out.
    static func backupReport(
        operation: String,
        snapshotId: String? = nil,
        error: String? = nil,
        bytes: Int = 0,
        durationSeconds: Double = 0
    ) throws -> Report {
        var args: [String: Any] = [
            "operation": operation, "bytes": bytes, "durationSeconds": durationSeconds,
        ]
        if let snapshotId { args["snapshotId"] = snapshotId }
        if let error { args["error"] = error }

        let reply = try object("backup.stateReport", args)
        guard let body = reply["body"] as? [String: Any] else {
            throw CoreError(message: "The backup report could not be built.")
        }
        return Report(body: body, releasesLease: reply["releasesLease"] as? Bool ?? false)
    }

    // MARK: - Reporting
    //
    // What this device tells the API about the server it runs. The core decides
    // everything — what to send, where, and *which credential signs it*; this
    // host signs it, sends it and forgets it. Mirrors `Core.kt` §Reporting, and
    // the two must stay in step because the field names are the contract.
    //
    // See `docs/ios-reporting.md`.

    /// One HTTP request the core has decided on.
    ///
    /// `auth` is the load-bearing field. Signing an operator change with the
    /// device token instead of the user's is not an error anybody sees: the API
    /// answers 200 and strips the change.
    struct Request {
        let method: String
        let path: String
        let body: [String: Any]
        let auth: String

        var userSigned: Bool { auth == "user" }

        /// A reply is only a request if it carries all four. A missing field
        /// yields nil so nothing is sent, rather than something sent wrongly.
        static func from(_ value: Any?) -> Request? {
            guard let object = value as? [String: Any],
                let method = object["method"] as? String,
                let path = object["path"] as? String,
                let body = object["body"] as? [String: Any],
                let auth = object["auth"] as? String
            else { return nil }
            return Request(method: method, path: path, body: body, auth: auth)
        }
    }

    // MARK: - App errors
    //
    // Four calls, one funnel. The core owns every decision — whether two
    // failures are the same bug, whether this one is worth sending again,
    // what has to be redacted, what the body looks like — so that this host
    // and the Android one cannot drift on any of them. See
    // `homerun-core::reporting::app_error` and `homerun-pumpkin-ffi::errors`.

    /// Point the core's crash artefacts at a directory this app owns.
    ///
    /// Once per process, at launch. Until this is called a panic in the native
    /// core has nowhere to go and ``appErrorDrain(context:)`` finds nothing.
    static func appErrorAttach(dataDir: String) {
        _ = try? call("error.attach", ["dataDir": dataDir])
    }

    /// Record one failure and get back the request to send, if any.
    ///
    /// **Nil is the ordinary answer**, not a failure: the core holds a
    /// sighting it has seen recently, and during a render loop it holds
    /// thousands. A caller that logs per nil reproduces, in the device log,
    /// the exact flood the core just prevented on the network.
    static func appErrorReport(
        context: [String: Any], occurrence: [String: Any]
    ) -> Request? {
        let value = try? call("error.report", ["context": context, "occurrence": occurrence])
        return Request.from((value as? [String: Any])?["request"])
    }

    /// Write one failure to disk for the next launch to send.
    ///
    /// Synchronous by necessity — the caller is an uncaught-exception handler
    /// on a process that is about to go, and anything asynchronous would not
    /// finish. That is also why this exists rather than the crash path calling
    /// ``appErrorReport(context:occurrence:)``: the request would be built
    /// correctly and then never sent.
    static func appErrorStash(context: [String: Any], occurrence: [String: Any]) {
        _ = try? call("error.stash", ["context": context, "occurrence": occurrence])
    }

    /// Everything the last launch left behind, as requests to send now.
    ///
    /// The core deletes each file before reading it, so nothing here can be
    /// seen twice however badly it goes.
    static func appErrorDrain(context: [String: Any]) -> [Request] {
        let value = try? call("error.drain", ["context": context])
        let requests = (value as? [String: Any])?["requests"] as? [Any] ?? []
        return requests.compactMap { Request.from($0) }
    }

    /// What the console said a crash was.
    struct Diagnosis {
        let cause: String
        let message: String
        let recovery: String

        var repairable: Bool { recovery == "redownloadAndRestart" }
    }

    /// Read a crash out of a server's last words.
    ///
    /// `retriesUsed` is the *host's* count, because only the host knows whether
    /// a launch ever reached running.
    ///
    /// Nil means the console said nothing this build recognises — which on iOS
    /// is the ordinary outcome, since these patterns are JVM strings and
    /// Pumpkin produces none of them. The player then gets the API's message
    /// instead of a wrong local one.
    static func crashDiagnosis(lines: [String], retriesUsed: Int = 0) -> Diagnosis? {
        guard
            let reply = try? call(
                "reporting.crash.diagnose", ["lines": lines, "retriesUsed": retriesUsed])
                as? [String: Any],
            let cause = reply["cause"] as? String,
            let message = reply["message"] as? String
        else { return nil }
        return Diagnosis(
            cause: cause, message: message,
            recovery: reply["recovery"] as? String ?? "report")
    }

    static func crashReport(serverId: String, deviceId: String, lines: [String]) -> Request? {
        Request.from(
            try? call(
                "reporting.crash.report",
                ["serverId": serverId, "deviceId": deviceId, "lines": lines]))
    }

    /// Build a stats report. The core's argument is `serviceId`, not
    /// `serverId` — the endpoint speaks of services.
    static func statsReport(
        serviceId: String, deviceId: String, stats: [String: Any]
    ) -> Request? {
        Request.from(
            try? call(
                "reporting.stats.report",
                ["serviceId": serviceId, "deviceId": deviceId, "stats": stats]))
    }

    /// What the running server answered about itself.
    struct Poll {
        let roster: [String: Any]?
        let ageSecs: Double?
    }

    /// Ask the server for its roster and the world's age.
    ///
    /// > **Blocking, and not by a little.** The age is a console round trip
    /// > with a three-second timeout. Call it off the main actor —
    /// > `await Task.detached { Core.statsPoll(loader: …) }.value`.
    ///
    /// The roster no longer costs anything: a linked engine holds the player
    /// list, so the supervisor answers from it and skips the console entirely.
    ///
    /// Never throws. Every failure degrades to nulls, because a report that
    /// refuses to be built because one measurement failed is worse than a
    /// report with a hole in it.
    ///
    /// `loader` must be `vanilla` here. The core pins commands with a
    /// `minecraft:` prefix for Paper, and Pumpkin registers bare command names
    /// — a pinned command fails outright.
    static func statsPoll(loader: String = "vanilla") -> Poll {
        guard let reply = try? object("server.statsPoll", ["loader": loader]) else {
            return Poll(roster: nil, ageSecs: nil)
        }
        return Poll(
            roster: reply["roster"] as? [String: Any],
            ageSecs: number(reply["ageSecs"])?.doubleValue)
    }

    /// Rescale a per-core CPU percentage to a percentage of the device.
    ///
    /// Not optional, and not obvious: the backend reports percent of **one
    /// core** and legitimately exceeds 100, while the endpoint's `cpu_usage` is
    /// percent of the **machine**. They are identical on a single-core reading,
    /// so skipping this passes every test anybody writes and reports a phone on
    /// fire.
    static func cpuPercentOfDevice(perCorePercent: Double, cores: Int) -> Double? {
        guard
            let value = try? call(
                "reporting.stats.cpuPercentOfDevice",
                ["perCorePercent": perCorePercent, "cores": cores])
        else { return nil }
        return number(value)?.doubleValue
    }

    /// When the next report is due, and why.
    struct Schedule {
        /// Opaque cadence state to hand back next time. Never inspected here.
        let held: [String: Any]
        /// `periodic`, `presence`, or nil when nothing is due yet.
        let trigger: String?
        let waitMs: Int
    }

    /// Advance the reporting cadence.
    ///
    /// The core owns every number in it — the 120 s interval, the 1 s presence
    /// debounce, and the rule that a presence report resets the periodic clock.
    /// The host owns only the clock and the opaque state, exactly as it does
    /// for ``Metrics``.
    ///
    /// A nil `held` means "first call", which makes a report due immediately.
    static func schedule(held: [String: Any]?, nowMs: Int, event: String? = nil) -> Schedule {
        var args: [String: Any] = ["nowMs": nowMs]
        if let held { args["schedule"] = held }
        if let event { args["event"] = event }

        guard let reply = try? object("reporting.stats.schedule", args),
            let next = reply["schedule"] as? [String: Any]
        else {
            // Keeping the old state rather than dropping it: a lost call must
            // not restart the cadence, which would report on the wrong beat
            // forever after.
            return Schedule(held: held ?? [:], trigger: nil, waitMs: 1_000)
        }
        return Schedule(
            held: next,
            trigger: reply["trigger"] as? String,
            waitMs: number(reply["waitMs"])?.intValue ?? 1_000)
    }

    /// Round-trip time to where a player actually connects.
    ///
    /// > **Blocking**, up to a five-second deadline. Call it off the main actor.
    ///
    /// `address` is `host:port` from `link.publicAddress` — the gateway's
    /// public address, never the port the server listens on locally. Splitting
    /// it here rather than in the core matches Android; the core takes the two
    /// halves.
    static func gatewayPing(address: String) -> Double? {
        guard let separator = address.lastIndex(of: ":") else { return nil }
        let host = String(address[address.startIndex..<separator])
        guard let port = Int(address[address.index(after: separator)...]), !host.isEmpty
        else { return nil }

        guard let value = try? call("net.gatewayPing", ["host": host, "port": port])
        else { return nil }
        return number(value)?.doubleValue
    }

    /// Round-trip time to a region's gateway, in milliseconds, or nil when it
    /// could not be reached.
    ///
    /// > **Blocking**, up to a five-second deadline. Call it off the main actor.
    ///
    /// `domain` is the API's address for a region — a bare hostname, not a
    /// URL. Splitting it and opening the socket both belong to the native
    /// side: doing either here is what produced the bug where every region
    /// reported unreachable. See `docs/region-latency.md`.
    static func regionLatency(domain: String) -> Double? {
        guard let value = try? call("net.regionLatency", ["domain": domain])
        else { return nil }
        return number(value)?.doubleValue
    }

    /// The gateway address a player connects to, out of a server body.
    static func publicAddress(serverBody: [String: Any]) -> String? {
        guard let value = try? call("link.publicAddress", ["body": serverBody]) else { return nil }
        return value as? String
    }

    /// Whether a server's settings put it in online mode.
    static func onlineMode(env: [String: Any], gameType: String?, loader: String) -> Bool? {
        var args: [String: Any] = ["env": env, "loader": loader]
        if let gameType { args["gameType"] = gameType }
        guard let reply = try? object("minecraft.settings.fromEnv", args) else { return nil }
        return reply["onlineMode"] as? Bool
    }

    /// Read an `op`/`deop`/`ban`/`pardon` out of a console command.
    ///
    /// Nil for everything else, which is most of what anybody types.
    static func opsCommand(_ command: String) -> [String: Any]? {
        (try? call("minecraft.ops.parse", ["command": command])) as? [String: Any]
    }

    /// A settings change, and the line to echo once it has been saved.
    struct OpsChange {
        let request: Request
        let line: String
    }

    /// Work out what a parsed op command changes, against the settings the API
    /// holds *now* — not the ones this launch started with, because another
    /// device or the dashboard may have moved them since.
    ///
    /// Nil means the settings already say this, so there is nothing to save.
    static func opsSync(
        command: [String: Any], serverBody: [String: Any], serverId: String
    ) -> OpsChange? {
        guard
            let reply = try? call(
                "minecraft.ops.sync",
                ["command": command, "server": serverBody, "serverId": serverId])
                as? [String: Any],
            let request = Request.from(reply["request"]),
            let line = reply["line"] as? String
        else { return nil }
        return OpsChange(request: request, line: line)
    }

    /// A minigame result a plugin printed to the console, if this line is one.
    static func minigameReport(serverId: String, line: String) -> Request? {
        Request.from(
            try? call("reporting.minigame.fromLine", ["serverId": serverId, "line": line]))
    }

    // MARK: - Over-the-air UI bundles

    /// A manifest whose signature verified, and what to do about it.
    struct BundleOffer {
        /// True only when this bundle should be fetched.
        let install: Bool
        /// One sentence for the log, worded once in Rust so both hosts say it
        /// the same way.
        let reason: String
        let bundle: String
        let url: String
        let sha256: String
        let minHost: Int
        let serial: Int
    }

    /// Verify a manifest's signature and judge it against what is installed.
    ///
    /// One call rather than two, and that is load-bearing: there is no way to
    /// get the fields of a manifest whose signature has not been checked. A
    /// host that could do that would keep working perfectly against any
    /// manifest anyone served it — a bug with no symptom until it is an
    /// incident.
    ///
    /// - Throws: ``CoreError`` if the signature does not verify or the
    ///   manifest is malformed. Both mean the same thing to the caller: fetch
    ///   nothing.
    static func evaluateBundle(
        manifest: String, publicKey: String, installed: [String: Any]
    ) throws -> BundleOffer {
        guard
            let reply = try call(
                "bundle.evaluate",
                ["manifest": manifest, "publicKey": publicKey, "installed": installed])
                as? [String: Any],
            let verified = reply["manifest"] as? [String: Any],
            let bundle = verified["bundle"] as? String,
            let url = verified["url"] as? String,
            let sha256 = verified["sha256"] as? String
        else {
            throw CoreError(message: "The core verified a manifest but returned none.")
        }
        return BundleOffer(
            install: reply["install"] as? Bool ?? false,
            reason: reply["reason"] as? String ?? "no reason given",
            bundle: bundle,
            url: url,
            sha256: sha256,
            minHost: verified["minHost"] as? Int ?? 0,
            serial: verified["serial"] as? Int ?? 0)
    }

    /// Whether a digest this host computed is the one that was signed.
    ///
    /// In the core rather than `==` here, so the comparison cannot be written
    /// three subtly different ways across three platforms.
    static func digestMatches(expected: String, actual: String) throws -> Bool {
        try call("bundle.digestMatches", ["expected": expected, "actual": actual]) as? Bool ?? false
    }

    // MARK: - Minecraft accounts

    // The Microsoft sign-in chain. Every request body and every response shape
    // is the core's, because the chain is five calls deep and full of details
    // that fail silently when wrong: the `d=` prefix on the RPS ticket, the
    // relying party that has to be Minecraft's and not Xbox's, the identity
    // token's exact spelling. ``MinecraftAuth`` performs the calls and decides
    // nothing about them, and Android reaches the identical methods through
    // its own `Core` — which is the whole reason none of this is written twice.

    /// One HTTP call, as the core described it.
    struct HTTPRequest {
        let method: String
        let url: String
        /// Ordered pairs rather than a dictionary: the core emits them in the
        /// order the endpoint expects and nothing is gained by reordering.
        let headers: [(String, String)]
        let body: String?
    }

    /// A pending device-code sign-in, with the page to send the user to.
    struct DeviceCode {
        let userCode: String
        let deviceCode: String
        /// `microsoft.com/link` with the code already filled in.
        let approvalURL: String
        let intervalSecs: Double
        let expiresInSecs: Double
    }

    private static func httpRequest(_ value: Any?) throws -> HTTPRequest {
        guard
            let object = value as? [String: Any],
            let method = object["method"] as? String,
            let url = object["url"] as? String
        else {
            throw CoreError(message: "The core described a request this host cannot read.")
        }
        let headers = (object["headers"] as? [[Any]] ?? []).compactMap { pair -> (String, String)? in
            guard pair.count == 2, let name = pair[0] as? String, let value = pair[1] as? String
            else { return nil }
            return (name, value)
        }
        return HTTPRequest(method: method, url: url, headers: headers, body: object["body"] as? String)
    }

    static func accountDeviceCodeRequest() throws -> HTTPRequest {
        try httpRequest(call("minecraft.account.deviceCodeRequest", [:]))
    }

    static func accountDeviceCode(from body: Any) throws -> DeviceCode {
        let value = try object("minecraft.account.deviceCodeFrom", ["body": body])
        guard
            let userCode = value["userCode"] as? String,
            let deviceCode = value["deviceCode"] as? String,
            let approvalURL = value["approvalUrl"] as? String
        else {
            throw CoreError(message: "Microsoft did not return a sign-in code.")
        }
        return DeviceCode(
            userCode: userCode,
            deviceCode: deviceCode,
            approvalURL: approvalURL,
            intervalSecs: (value["intervalSecs"] as? NSNumber)?.doubleValue ?? 5,
            expiresInSecs: (value["expiresInSecs"] as? NSNumber)?.doubleValue ?? 900)
    }

    static func accountPollRequest(deviceCode: String) throws -> HTTPRequest {
        try httpRequest(call("minecraft.account.pollRequest", ["deviceCode": deviceCode]))
    }

    /// What a poll response meant: pending, slowDown, declined, expired, approved.
    static func accountPollOutcome(_ body: Any) throws -> [String: Any] {
        try object("minecraft.account.pollOutcome", ["body": body])
    }

    static func accountMsaTokens(from body: Any) throws -> [String: Any] {
        try object("minecraft.account.msaTokensFrom", ["body": body])
    }

    static func accountRefreshRequest(refreshToken: String) throws -> HTTPRequest {
        try httpRequest(call("minecraft.account.refreshRequest", ["refreshToken": refreshToken]))
    }

    static func accountXblRequest(msaAccessToken: String) throws -> HTTPRequest {
        try httpRequest(call("minecraft.account.xblRequest", ["msaAccessToken": msaAccessToken]))
    }

    static func accountXstsRequest(xblToken: String) throws -> HTTPRequest {
        try httpRequest(call("minecraft.account.xstsRequest", ["xblToken": xblToken]))
    }

    static func accountXboxToken(from body: Any) throws -> [String: Any] {
        try object("minecraft.account.xboxTokenFrom", ["body": body])
    }

    /// An XSTS refusal, in words naming what the player has to go and do.
    static func accountXstsRefusal(_ body: Any) throws -> String {
        try string("minecraft.account.xstsRefusal", ["body": body])
    }

    static func accountMinecraftLoginRequest(xsts: [String: Any]) throws -> HTTPRequest {
        try httpRequest(call("minecraft.account.minecraftLoginRequest", ["xsts": xsts]))
    }

    static func accountMinecraftToken(from body: Any) throws -> String {
        try string("minecraft.account.minecraftTokenFrom", ["body": body])
    }

    static func accountProfileRequest(minecraftToken: String) throws -> HTTPRequest {
        try httpRequest(call("minecraft.account.profileRequest", ["minecraftToken": minecraftToken]))
    }

    /// The stored session: identity plus the tokens that keep it alive.
    static func accountSession(
        profile: Any,
        minecraftToken: String,
        msa: Any,
        nowMs: Double
    ) throws -> [String: Any] {
        try object(
            "minecraft.account.sessionFrom",
            [
                "profile": profile,
                "minecraftToken": minecraftToken,
                "msa": msa,
                "nowMs": nowMs,
            ])
    }

    /// The only shape of a session allowed to cross into the WebView.
    ///
    /// The bridge type has token fields because the desktop's client launcher
    /// needs them to start a game. No phone surface reads one, so they go over
    /// as `"0"` and the real tokens stay in the Keychain.
    static func accountRedacted(_ session: [String: Any]) throws -> [String: Any] {
        try object("minecraft.account.redacted", ["session": session])
    }

    static func accountNeedsRefresh(expiresAt: Double, nowMs: Double) throws -> Bool {
        try bool("minecraft.account.needsRefresh", ["expiresAt": expiresAt, "nowMs": nowMs])
    }
}

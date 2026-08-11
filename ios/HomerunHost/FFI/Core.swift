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

    /// False when these are the dead credentials from the previous session.
    static func linkIsUsable(polled: [String: Any], before: [String: Any]?) throws -> Bool {
        var args: [String: Any] = ["polled": polled]
        if let before { args["before"] = before }
        return try call("link.isUsable", args) as? Bool ?? false
    }

    // MARK: - Lifecycle

    /// `stopped` or `crashed`. A server exits 0 on `stop`, so intent decides.
    static func exitState(intentional: Bool, code: Int) throws -> String {
        try string("state.exit", ["intentional": intentional, "code": code])
    }

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
}

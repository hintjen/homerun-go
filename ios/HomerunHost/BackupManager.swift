import Foundation
import UIKit

/// Backing a world up, and putting one back.
///
/// # Nothing here decides anything
///
/// What to restore and why, whether the lease permits a launch, what a failure
/// means, the exact `backup-state` body — every one of those is
/// `homerun-core::backup`, reached through ``Core``. This gathers facts, calls
/// the engine, and carries out the answer. That is what lets a host that
/// spawns a binary and a host that links a library agree.
///
/// # The lease, and the rule that follows from it
///
/// The API opens the backup lease when this device acks `stopped` with
/// `backup_in_progress`, and closes it only when this device reports
/// `backup-state`. **The lease has no timeout.** A device that claims it and
/// never reports locks every other device out of that world until its own next
/// `running` ack; the escape hatch is a force-launch, which shows the user a
/// data-loss warning for what is usually just a phone that got closed.
///
/// So: every precondition is decided *before* the ack — in
/// `PumpkinBackend.finish` — and after the ack ``backupAfterStop(serverId:dir:context:onLog:)``
/// reports on every path, including the ones where there is nothing to do. Its
/// parameters are non-optional so it cannot be written to return early.
///
/// # Foreground, with the screen awake
///
/// iOS suspends a backgrounded app within seconds, and a world upload is
/// minutes. There is no background mode that covers this and declaring one we
/// cannot honour is an App Review rejection, so the backup runs while the app
/// is in front of the player, with progress on screen. A background-task
/// assertion is taken anyway: it buys about five seconds if they leave, which
/// is exactly enough to report the backup failed and close the lease.
@MainActor
final class BackupManager {

    /// How often the console is given a progress line. The engine's counters
    /// move far faster than anyone can read.
    private static let progressInterval: TimeInterval = 1.0

    // MARK: - Facts

    /// Is there a world here worth backing up?
    ///
    /// Pumpkin writes `world/` beside the server; `worlds/` is what other
    /// engines use and is checked so this does not have to change when one
    /// appears. Empty directories do not count — an empty snapshot becoming
    /// the newest is how a co-host restores nothing over live work.
    func hasLocalWorld(_ dir: URL) -> Bool {
        ["world", "worlds"].contains { name in
            let candidate = dir.appendingPathComponent(name)
            let contents = try? FileManager.default.contentsOfDirectory(atPath: candidate.path)
            return !(contents ?? []).isEmpty
        }
    }

    // MARK: - The lease gate

    /// Why this launch must not happen, or nil to go ahead.
    ///
    /// Called before the server starts. A blocked launch is not an error in
    /// this host's sense — it is a sentence for the player, who can wait or
    /// force their way past it.
    func leaseBlockedReason(
        settings: HomerunAPI.ServerSettings, deviceId: String, force: Bool
    ) -> String? {
        guard let decision = try? Core.leaseDecision(
            leaseDevice: settings.backupLeaseDevice, deviceId: deviceId, force: force)
        else {
            // The core could not answer. Refusing to host over that would be
            // worse than the race it protects against.
            return nil
        }

        switch decision {
        case .launch:
            return nil
        case .forced(let takenFrom):
            HostLog.host.warning("took the backup lease from \(takenFrom, privacy: .public)")
            return nil
        case .blocked:
            return """
                Another device is still backing this world up. Wait for it to finish, \
                or start anyway to take over — which may lose that backup.
                """
        }
    }

    // MARK: - Before launch

    /// Restore the world if another device has a newer one, or the dashboard
    /// pinned one, or there is nothing here.
    ///
    /// Throws only when a restore was *required* and failed. Starting on a
    /// world we have been told is stale is the divergence this whole subsystem
    /// exists to prevent, so that case must stop the launch.
    func restoreBeforeLaunch(
        serverId: String,
        dir: URL,
        context: BackupContext,
        onLog: @escaping (String) -> Void
    ) async throws {
        guard let repo = context.settings.backup else { return }
        guard BackupFFI.isAvailable else {
            HostLog.host.info("no backup engine in this build — hosting without backups")
            return
        }

        // Any failure here is nil, deliberately: no signal, a backend hiccup
        // and an empty repository are indistinguishable from here and all mean
        // the same thing to the core — there is nothing to compare against. A
        // phone on a train must still be able to host.
        let latest = await latestSnapshot(repo: repo)

        let decision: Core.Restore
        do {
            decision = try Core.restoreDecision(
                pinned: context.settings.restoreFromSnapshot,
                latest: latest,
                deviceId: context.deviceId,
                hasLocalWorld: hasLocalWorld(dir))
        } catch {
            HostLog.host.error(
                "restore decision failed: \(error.localizedDescription, privacy: .public)")
            return
        }

        switch decision {
        case .skip(let reason):
            HostLog.host.info("keeping the local world (\(reason, privacy: .public))")

        case .rollback(let snapshotId):
            onLog("[Backup] Rolling back to an earlier backup…")
            try await pull(
                serverId: serverId, dir: dir, repo: repo, snapshotId: snapshotId, onLog: onLog)
            onLog("[Backup] Rollback complete.")

        case .latest(let snapshotId, let reason):
            onLog(
                reason == "anotherDeviceIsNewer"
                    ? "[Backup] Restoring the latest world (backed up by another device)…"
                    : "[Backup] No world here — restoring from backup…")
            try await pull(
                serverId: serverId, dir: dir, repo: repo, snapshotId: snapshotId, onLog: onLog)
            onLog("[Backup] World restored.")
        }
    }

    /// Fetch a snapshot over the top of the local world.
    ///
    /// # Why the world is moved aside rather than deleted
    ///
    /// A restore that fails halfway must not take the world with it. Android
    /// deletes `world/` before moving the restored copy into place, which
    /// leaves a window — a crash, a full disk, a dropped connection — where
    /// the player has neither. Renaming instead costs nothing (same volume,
    /// one inode operation) and means the old world is still there to put back.
    ///
    /// It does mean both copies exist at once, which on a phone with a large
    /// world is a real constraint. That is the trade being made: transient
    /// disk in exchange for never being the reason someone lost a world.
    private func pull(
        serverId: String,
        dir: URL,
        repo: [String: Any],
        snapshotId: String,
        onLog: @escaping (String) -> Void
    ) async throws {
        let files = FileManager.default
        let asideSuffix = ".replacing"
        var movedAside: [(live: URL, aside: URL)] = []

        // Move the current worlds out of the way, remembering how to undo it.
        for name in ["world", "worlds"] {
            let live = dir.appendingPathComponent(name)
            guard files.fileExists(atPath: live.path) else { continue }
            let aside = dir.appendingPathComponent(name + asideSuffix)
            try? files.removeItem(at: aside)
            do {
                try files.moveItem(at: live, to: aside)
                movedAside.append((live, aside))
            } catch {
                // Could not even rename. Put back whatever we did move and
                // stop before the engine writes anything.
                for (live, aside) in movedAside { try? files.moveItem(at: aside, to: live) }
                throw ServerBackendError.engine(
                    "The world could not be prepared for a restore, so the server was not started.")
            }
        }

        let reply = await runWatched(
            [
                "operation": "restore",
                "repo": repo,
                "cacheDir": cacheDirectory().path,
                "snapshotId": snapshotId,
                // The engine resolves the recorded path from the snapshot: a
                // desktop's is nothing like ours, and cross-device restore is
                // the point.
                "serverId": serverId,
                "targetDir": dir.path,
            ],
            onLog: onLog)

        // Reported either way, and before anything is thrown — a restore that
        // failed is still an outcome the API is waiting on.
        report(serverId: serverId, operation: "restore", reply: reply)

        guard reply.ok else {
            // Undo: drop whatever the engine managed to write, put the old
            // world back, and leave the launch refused.
            for (live, aside) in movedAside {
                try? files.removeItem(at: live)
                try? files.moveItem(at: aside, to: live)
            }
            throw ServerBackendError.engine(
                reply.error ?? "The world could not be restored, so the server was not started.")
        }

        for (_, aside) in movedAside { try? files.removeItem(at: aside) }
    }

    // MARK: - After a clean stop

    /// Back the world up, and report the outcome.
    ///
    /// > **Every path reports.** By the time this is called the `stopped` ack
    /// > has already gone out with `backup_in_progress`, which opened the
    /// > lease, and nothing else will close it. The parameters are
    /// > non-optional so there is no "nothing to do here" early return to
    /// > write by accident — that is the bug this shape exists to prevent.
    func backupAfterStop(
        serverId: String,
        dir: URL,
        repo: [String: Any],
        deviceId: String,
        onLog: @escaping (String) -> Void
    ) async {
        // Held for the whole operation, not taken on backgrounding: by the
        // time `didEnterBackground` arrives the seconds it buys are gone.
        let assertion = UIApplication.shared.beginBackgroundTask(withName: "homerun.backup") {
            // ~5 seconds before suspension. Not enough to finish, exactly
            // enough to stop pretending and let another device have the lease.
            BackupFFI.cancel()
        }
        UIApplication.shared.isIdleTimerDisabled = true
        defer {
            UIApplication.shared.isIdleTimerDisabled = false
            if assertion != .invalid { UIApplication.shared.endBackgroundTask(assertion) }
        }

        guard (try? Core.shouldBackUp(hasLocalWorld: hasLocalWorld(dir))) == true else {
            onLog("[Backup] No world to back up — skipping, to protect the existing backup.")
            // Still a report. The lease is open and this is the only thing
            // that will close it.
            report(
                serverId: serverId, operation: "backup",
                failure: "no world to back up")
            return
        }

        onLog("[Backup] Backing up the world…")
        let reply = await runWatched(
            [
                "operation": "backup",
                "repo": repo,
                "cacheDir": cacheDirectory().path,
                "sourceDir": dir.path,
                "deviceId": deviceId,
            ],
            onLog: onLog)

        report(serverId: serverId, operation: "backup", reply: reply)

        if reply.ok {
            onLog("[Backup] Backup complete.")
        } else {
            onLog("[Backup] Backup failed: \(reply.error ?? "unknown error")")
            if let raw = reply.object?["message"] as? String,
                let verdict = try? Core.classifyBackupFailure(message: raw, host: deviceId)
            {
                HostLog.host.error(
                    "backup failed (\(verdict.kind, privacy: .public), retryable=\(verdict.retryable, privacy: .public))"
                )
            }
        }
    }

    // MARK: - Reporting, and the outbox

    private func report(serverId: String, operation: String, reply: HomerunFFI.Reply) {
        if reply.ok {
            report(
                serverId: serverId, operation: operation,
                snapshotId: reply.object?["snapshotId"] as? String,
                bytes: reply.object?["bytes"] as? Int ?? 0,
                seconds: reply.object?["durationSeconds"] as? Double ?? 0)
        } else {
            report(
                serverId: serverId, operation: operation,
                failure: reply.object?["message"] as? String ?? reply.error ?? "backup failed")
        }
    }

    private func report(
        serverId: String, operation: String, snapshotId: String? = nil,
        bytes: Int = 0, seconds: Double = 0
    ) {
        guard let built = try? Core.backupReport(
            operation: operation, snapshotId: snapshotId, bytes: bytes, durationSeconds: seconds)
        else { return }
        send(serverId: serverId, body: built.body)
    }

    private func report(serverId: String, operation: String, failure: String) {
        guard let built = try? Core.backupReport(operation: operation, error: failure) else { return }
        send(serverId: serverId, body: built.body)
    }

    /// Persist first, then try. Anything still on disk is retried at launch.
    private func send(serverId: String, body: [String: Any]) {
        HostStore.rememberBackupReport(serverId: serverId, body: body)
        Task { await BackupManager.flushPendingReports() }
    }

    /// Deliver anything the last session could not.
    ///
    /// Called at launch and after each report. Static because the launch path
    /// runs before any backend exists.
    static func flushPendingReports() async {
        let pending = HostStore.pendingBackupReports
        guard !pending.isEmpty,
            let apiURL = HostStore.apiURL,
            let deviceToken = TokenStore.deviceToken
        else { return }

        for (serverId, body) in pending {
            do {
                try await HomerunAPI.reportBackupState(
                    apiURL: apiURL, serverId: serverId, body: body, deviceToken: deviceToken)
                HostStore.forgetBackupReport(serverId: serverId)
            } catch {
                // Left on disk on purpose: the lease stays open until this
                // lands, so dropping it would be the silent version of the
                // failure this outbox exists to prevent.
                HostLog.host.error(
                    "backup-state for \(serverId, privacy: .public) still undelivered: \(error.localizedDescription, privacy: .public)"
                )
            }
        }
    }

    // MARK: - Plumbing

    /// Run the engine, turning its progress into console lines while it works.
    private func runWatched(
        _ request: [String: Any], onLog: @escaping (String) -> Void
    ) async -> HomerunFFI.Reply {
        var cursor = 0
        var lastPercent = -1

        let pump = Task { @MainActor in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: UInt64(Self.progressInterval * 1_000_000_000))
                let progress = BackupFFI.progress(since: cursor)
                cursor = progress.cursor
                for line in progress.lines { onLog(line) }

                // Only when there is a real denominator, and only when it
                // moved — otherwise the console is nothing but percentages.
                if let fraction = progress.fraction {
                    let percent = Int(fraction * 100)
                    if percent != lastPercent, percent % 5 == 0 {
                        lastPercent = percent
                        onLog("[Backup] \(percent)%")
                    }
                }
            }
        }
        defer { pump.cancel() }

        let reply = await BackupFFI.run(request)

        // Whatever the engine said on its way out, which is usually the
        // reason.
        let tail = BackupFFI.progress(since: cursor)
        for line in tail.lines { onLog(line) }
        return reply
    }

    /// rustic's cache, under Caches/ so the OS may purge it — correct for a
    /// cache, and catastrophic for anything else. Never inside the server
    /// directory: that is the tree being backed up.
    private func cacheDirectory() -> URL {
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        let cache = base.appendingPathComponent("rustic", isDirectory: true)
        try? FileManager.default.createDirectory(at: cache, withIntermediateDirectories: true)
        return cache
    }

    private func latestSnapshot(repo: [String: Any]) async -> [String: Any]? {
        let reply = await BackupFFI.latestSnapshot([
            "repo": repo, "cacheDir": cacheDirectory().path,
        ])
        guard reply.ok else {
            HostLog.host.info("no snapshot to compare against: \(reply.error ?? "", privacy: .public)")
            return nil
        }
        return reply.object?["snapshot"] as? [String: Any]
    }
}

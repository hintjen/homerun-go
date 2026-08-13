import Foundation

/// Which UI bundle the WebView is served from, and what happens when a new one
/// turns out to be broken.
///
/// A port of Android's `BundleStore.kt`, deliberately line-for-line where it
/// can be: that version has been driven through every failure case on a device
/// (`docs/ota-bundles.md`), so divergence here is much more likely to be a bug
/// than an improvement. The *decisions* — is this manifest authentic, is it
/// newer, may this host run it — are not repeated on either side; they live in
/// `homerun_core::bundle` and both platforms ask the same code.
///
/// # The layout
///
/// ```
/// Application Support/ui/current     the bundle being served
/// Application Support/ui/previous    the last one known to have reached __bridge:ready
/// Application Support/ui/pending     downloaded and verified, not yet live
/// Application Support/ui/probation   how many launches current has left to prove itself
/// Application Support/ui/.staging    an archive mid-unpack; never a bundle
/// (Bundle.main/web)                  the floor — never deleted, never overwritten
/// ```
///
/// Application Support rather than Caches, which iOS may evict under storage
/// pressure — at any moment, including between the launch that downloads a
/// bundle and the launch that promotes it. It is excluded from iCloud backup:
/// this is re-downloadable content, and Apple rejects apps that back up
/// caches.
///
/// # Why probation is on disk
///
/// The failure it survives is a bundle that kills the app before the page can
/// say anything. An in-memory counter dies with the process without recording
/// the attempt, so a fatal bundle would retry for ever — bricking the app in a
/// way **no App Store update could fix**, because the broken bundle outranks
/// the one in the binary.
enum BundleStore {

    /// What ``resolve()`` settled on, and what ``AppSchemeHandler`` serves.
    struct Loaded {
        /// The bundle id, or ``shipped`` for the copy inside the app.
        let id: String
        /// The directory to serve, or nil to serve `Bundle.main/web`.
        let root: URL?
        /// Release ordering; 0 for the shipped copy, which every release outranks.
        let serial: Int
    }

    /// The id reported for the bundle compiled into the app.
    ///
    /// A name rather than nil deliberately: this shows up in `get-app-version`
    /// and therefore in bug reports, where nil reads as "the host did not say".
    static let shipped = "shipped"

    /// The `platform` a manifest must name to be for this host.
    static let platform = "ios"

    /// Launches a freshly activated bundle gets to reach `__bridge:ready`.
    ///
    /// Two, not one: the first launch after an update is also the one most
    /// likely to be killed for reasons that are not the bundle's fault — iOS
    /// reclaiming a backgrounded app, the user swiping away mid-splash.
    private static let attempts = 2

    private static let uiDirName = "ui"
    private static let currentName = "current"
    private static let previousName = "previous"
    private static let pendingName = "pending"
    private static let probationName = "probation"
    private static let stagingName = ".staging"
    private static let manifestName = "bundle.json"

    /// Everything below mutates the same directory tree. `resolve` runs on the
    /// main thread before a WebView exists; the updater stages from a
    /// background task. One queue rather than trusting that those never meet.
    private static let queue = DispatchQueue(label: "app.gethomerun.ios.bundle-store")

    private static var loaded: Loaded?
    private static var onProbation = false

    // MARK: - What is being served

    /// What is being served. ``shipped`` until ``resolve()`` has run.
    static func active() -> String {
        queue.sync { loaded?.id ?? shipped }
    }

    /// What the update check tells `homerun_core::bundle` about this device.
    ///
    /// Built from what ``resolve()`` settled on rather than re-read from disk,
    /// so it describes the bundle actually running — the one whose serial a
    /// newer release has to beat.
    static func installed(hostRevision: Int) -> [String: Any] {
        queue.sync {
            // NSNull, not "shipped": core distinguishes "no over-the-air
            // bundle" from "a bundle called shipped", and only the former may
            // be replaced by serial 1.
            let id: Any = (loaded?.root != nil) ? (loaded?.id ?? shipped) : NSNull()
            return [
                "bundle": id,
                "serial": loaded?.serial ?? 0,
                "hostRevision": hostRevision,
                "platform": platform,
            ]
        }
    }

    // MARK: - Promotion

    /// Make a downloaded bundle live, if one is waiting.
    ///
    /// Call before the WebView is built and not otherwise. Swapping the bundle
    /// under a running page cancels whatever bridge call is in flight, and
    /// `native-server-start` runs for minutes.
    static func activate() {
        queue.sync {
            let ui = uiDirectory()
            let pending = ui.appendingPathComponent(pendingName)
            guard FileManager.default.fileExists(atPath: pending.path) else { return }

            guard let bundle = readManifest(pending) else {
                // Verified before it was written, so this means the download
                // was interrupted or something else wrote here. Leaving it
                // would re-run the same judgement every launch.
                HostLog.bundle.warning("discarding an unusable pending bundle")
                try? FileManager.default.removeItem(at: pending)
                return
            }

            let current = ui.appendingPathComponent(currentName)
            let previous = ui.appendingPathComponent(previousName)
            try? FileManager.default.removeItem(at: previous)

            if FileManager.default.fileExists(atPath: current.path) {
                do {
                    try FileManager.default.moveItem(at: current, to: previous)
                } catch {
                    // Without a rollback target this update is one-way, so do
                    // not take it. The pending bundle keeps its name and is
                    // tried again next launch.
                    HostLog.bundle.error(
                        "could not move the live bundle aside; staying put: \(error.localizedDescription)"
                    )
                    return
                }
            }

            do {
                try FileManager.default.moveItem(at: pending, to: current)
            } catch {
                // A rename within one directory failing is close to
                // impossible, and the state it leaves — no `current` — is one
                // `resolve` copes with anyway. Say so loudly and let it.
                HostLog.bundle.error("could not promote the pending bundle: \(error.localizedDescription)")
                return
            }

            writeProbation(ui, id: bundle.id, attempts: attempts)
            HostLog.bundle.info("activated bundle \(bundle.id, privacy: .public); \(attempts) launches to prove itself")
        }
    }

    // MARK: - Staging, for BundleUpdater

    /// An empty directory to unpack an archive into.
    ///
    /// Cleared each time: a leftover tree from an interrupted download would
    /// otherwise be unpacked *over*, mixing two bundles' files.
    static func stagingDirectory() -> URL {
        queue.sync {
            let staging = uiDirectory().appendingPathComponent(stagingName)
            try? FileManager.default.removeItem(at: staging)
            try? FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
            return staging
        }
    }

    /// Make an unpacked tree the pending bundle. Returns false if it could not.
    ///
    /// The manifest is written **here**, from the signed values the update
    /// check verified, rather than trusted from inside the archive. The
    /// archive's own copy is covered by the signed digest and so is not
    /// untrusted exactly — but it is a second copy of facts that already have
    /// an authority, and two copies of a fact eventually disagree.
    @discardableResult
    static func stage(unpacked: URL, id: String, minHost: Int, serial: Int) -> Bool {
        queue.sync {
            let index = unpacked.appendingPathComponent("index.html")
            guard FileManager.default.fileExists(atPath: index.path) else {
                // The same completeness marker `scripts/build-ui.js` uses. An
                // archive without one is not a UI, and staging it would trade
                // a working app for a blank screen on the next launch.
                HostLog.bundle.error("the unpacked bundle \(id, privacy: .public) has no index.html; discarding it")
                try? FileManager.default.removeItem(at: unpacked)
                return false
            }

            let record: [String: Any] = ["id": id, "minHost": minHost, "serial": serial]
            do {
                let data = try JSONSerialization.data(withJSONObject: record)
                try data.write(to: unpacked.appendingPathComponent(manifestName), options: .atomic)
            } catch {
                HostLog.bundle.error("could not write the manifest for \(id, privacy: .public): \(error.localizedDescription)")
                try? FileManager.default.removeItem(at: unpacked)
                return false
            }

            let pending = uiDirectory().appendingPathComponent(pendingName)
            try? FileManager.default.removeItem(at: pending)
            do {
                try FileManager.default.moveItem(at: unpacked, to: pending)
            } catch {
                HostLog.bundle.error("could not stage bundle \(id, privacy: .public) as pending: \(error.localizedDescription)")
                try? FileManager.default.removeItem(at: unpacked)
                return false
            }

            HostLog.bundle.info("bundle \(id, privacy: .public) is staged; it goes live on the next launch")
            return true
        }
    }

    /// The id already waiting to go live, if any. Keeps the updater from refetching it.
    static func pending() -> String? {
        queue.sync { readManifest(uiDirectory().appendingPathComponent(pendingName))?.id }
    }

    // MARK: - Resolution

    /// Decide what to serve, and spend one of the current bundle's attempts.
    ///
    /// Synchronous file work, deliberately: the answer is needed before the
    /// WebView is created and it is a handful of `stat`s. Doing it off-thread
    /// would only mean the page loads later.
    @discardableResult
    static func resolve() -> Loaded {
        queue.sync {
            let ui = uiDirectory()

            // Each demotion changes what `current` is, so the question has to
            // be asked again. Bounded because every demotion removes a
            // directory: current, then previous, then there is nothing left.
            for _ in 0..<3 {
                let dir = ui.appendingPathComponent(currentName)
                guard let bundle = readManifest(dir) else {
                    let hasCurrent = FileManager.default.fileExists(atPath: dir.path)
                    let hasPrevious = FileManager.default.fileExists(
                        atPath: ui.appendingPathComponent(previousName).path
                    )
                    if !hasCurrent && !hasPrevious { return floor(ui) }
                    HostLog.bundle.warning("the live bundle is unusable; falling back")
                    demote(ui)
                    continue
                }

                let remaining = probation(ui, for: bundle.id)
                if let remaining, remaining <= 0 {
                    HostLog.bundle.warning(
                        "bundle \(bundle.id, privacy: .public) never reached the bridge handshake in \(attempts) launches; rolling back"
                    )
                    demote(ui)
                    continue
                }
                if let remaining {
                    writeProbation(ui, id: bundle.id, attempts: remaining - 1)
                    onProbation = true
                    HostLog.bundle.info(
                        "serving bundle \(bundle.id, privacy: .public) on probation, \(remaining - 1) attempts left"
                    )
                } else {
                    onProbation = false
                    HostLog.bundle.info("serving bundle \(bundle.id, privacy: .public)")
                }

                let result = Loaded(id: bundle.id, root: dir, serial: bundle.serial)
                loaded = result
                return result
            }

            return floor(ui)
        }
    }

    /// The copy inside the app. Always present, which is the whole point.
    private static func floor(_ ui: URL) -> Loaded {
        clearProbation(ui)
        onProbation = false
        HostLog.bundle.info("serving the shipped bundle")
        let result = Loaded(id: shipped, root: nil, serial: 0)
        loaded = result
        return result
    }

    /// `current` is bad: drop it and promote `previous` into its place.
    ///
    /// Moving rather than serving `previous` where it lies, so the state on
    /// disk converges. Left in place, a broken `current` would be re-judged
    /// every launch for ever and the next update would land on top of it.
    private static func demote(_ ui: URL) {
        clearProbation(ui)
        try? FileManager.default.removeItem(at: ui.appendingPathComponent(currentName))
        let previous = ui.appendingPathComponent(previousName)
        guard FileManager.default.fileExists(atPath: previous.path) else { return }
        do {
            try FileManager.default.moveItem(at: previous, to: ui.appendingPathComponent(currentName))
        } catch {
            HostLog.bundle.error("could not restore the previous bundle; falling through to the shipped one")
            try? FileManager.default.removeItem(at: previous)
        }
    }

    /// The page reached `__bridge:ready`: the bundle works.
    ///
    /// The only evidence worth trusting. A bundle that renders nothing still
    /// runs its scripts; one that throws on its first chunk never gets here,
    /// which is exactly the case the counter exists for.
    static func confirm() {
        queue.sync {
            guard onProbation else { return }
            onProbation = false
            clearProbation(uiDirectory())
            HostLog.bundle.info("bundle \(loaded?.id ?? shipped, privacy: .public) confirmed")
        }
    }

    // MARK: - Disk

    /// Created on first use, and excluded from backup.
    private static func uiDirectory() -> URL {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        var ui = support.appendingPathComponent(uiDirName)
        if !FileManager.default.fileExists(atPath: ui.path) {
            try? FileManager.default.createDirectory(at: ui, withIntermediateDirectories: true)
            // Re-downloadable content. Apple rejects apps that back this up,
            // and restoring a device should not restore a bundle that has
            // since been rolled back.
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            try? ui.setResourceValues(values)
        }
        return ui
    }

    private struct Manifest {
        let id: String
        let minHost: Int
        let serial: Int
    }

    /// Read a bundle directory's identity, or nil if it is not one we may serve.
    ///
    /// Three ways to fail, all meaning "do not serve this": no `index.html`,
    /// no readable `bundle.json` with an `id`, or a `minHost` above this
    /// host's revision.
    ///
    /// The last is belt and braces — the manifest server already filters on
    /// the revision the update check sent it. The server is not the only thing
    /// that can be wrong, and the cost of being wrong is a UI calling channels
    /// this binary has never heard of, which hangs silently for ever.
    private static func readManifest(_ dir: URL) -> Manifest? {
        guard FileManager.default.fileExists(atPath: dir.appendingPathComponent("index.html").path),
              let data = try? Data(contentsOf: dir.appendingPathComponent(manifestName)),
              let parsed = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let id = parsed["id"] as? String,
              !id.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return nil }

        let minHost = parsed["minHost"] as? Int ?? 0
        guard minHost <= BridgeRouter.hostRevision else {
            HostLog.bundle.warning(
                "bundle \(id, privacy: .public) needs host revision \(minHost) and this host is \(BridgeRouter.hostRevision); refusing it"
            )
            return nil
        }
        // Absent means a bundle staged by hand, which is worth keeping
        // working. Serial 0 makes it replaceable by any real release.
        return Manifest(id: id, minHost: minHost, serial: parsed["serial"] as? Int ?? 0)
    }

    /// Attempts remaining for `id`, or nil if it is not on probation.
    private static func probation(_ ui: URL, for id: String) -> Int? {
        guard let data = try? Data(contentsOf: ui.appendingPathComponent(probationName)),
              let parsed = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return nil }
        // A record naming a different bundle is stale — the bundle it judged
        // is gone. Treating it as ours would spend a confirmed bundle's
        // attempts.
        guard parsed["bundle"] as? String == id else { return nil }
        return parsed["attempts"] as? Int
    }

    /// Write the record, atomically.
    ///
    /// A torn file reads as absent, which reads as "confirmed" — the one wrong
    /// answer that matters, because it lets a fatal bundle retry for ever.
    /// `.atomic` writes beside the target and renames, so the file is either
    /// the old record or the new one.
    private static func writeProbation(_ ui: URL, id: String, attempts: Int) {
        let record: [String: Any] = ["bundle": id, "attempts": attempts]
        do {
            let data = try JSONSerialization.data(withJSONObject: record)
            try data.write(to: ui.appendingPathComponent(probationName), options: .atomic)
        } catch {
            HostLog.bundle.error("could not record probation for \(id, privacy: .public): \(error.localizedDescription)")
        }
    }

    private static func clearProbation(_ ui: URL) {
        try? FileManager.default.removeItem(at: ui.appendingPathComponent(probationName))
    }
}

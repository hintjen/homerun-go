import CryptoKit
import Foundation
import ZIPFoundation

/// Fetching a new UI bundle. The half that goes and looks.
///
/// ``BundleStore`` is the half that decides what to serve and rolls back what
/// does not work; this one only ever produces a `pending` directory for it.
/// The split matters: everything here is best-effort and may fail silently,
/// because nothing here is on the path to showing the user a screen.
///
/// A port of Android's `BundleUpdater.kt`. Three things differ, and only
/// because the platform forces them:
///
///  - **The unzip.** Android has `java.util.zip`; iOS ships no zip API at all,
///    so this takes ZIPFoundation. Entries are walked by hand rather than
///    calling `unzipItem`, so the path-traversal and size guarantees are ours
///    and match Android's exactly.
///  - **When it runs.** Android checks on launch and on resume from a
///    lifecycle scope. iOS gets `didBecomeActive`, which is the same two
///    moments.
///  - **No `applicationContext`.** Everything here is static because the
///    store is.
///
/// Nothing here decides anything. The manifest's signature, whether this host
/// may run the bundle, and whether it is newer than what is installed are all
/// answered by `homerun_core::bundle` in one call, so iOS and Android cannot
/// judge the same manifest differently.
enum BundleUpdater {

    /// How long after a check before another is worth making.
    ///
    /// Six hours; the ceiling on how stale a device can be is this plus one
    /// launch. Shorter would mean a request every time the user switches back
    /// to the app, for a bundle that changes a few times a week at most.
    private static let throttle: TimeInterval = 6 * 60 * 60

    /// Refuse an archive bigger than this. A bundle is ~3.5 MB; 64 MB is far
    /// above any real one and far below "fills the user's phone".
    private static let maxArchiveBytes = 64 * 1024 * 1024

    /// The same, for what an archive expands to. Zip bombs are cheap to make.
    private static let maxUnpackedBytes = 256 * 1024 * 1024
    private static let maxEntries = 10_000

    private static let lastCheckKey = "bundleUpdater.lastCheckAt"

    /// One check at a time. `didBecomeActive` can arrive twice in quick
    /// succession; without this the same archive downloads twice and the two
    /// unpack into one staging directory.
    private static let lock = NSLock()
    private static var checking = false

    /// Called when a bundle becomes `pending`.
    ///
    /// This is what turns a silent background download into an offer the user
    /// can accept — the bridge controller wires it to `update-available`. A
    /// callback rather than a direct emit because this type has no page and
    /// must keep working when there is no WebView at all.
    static var onBundleStaged: ((String) -> Void)?

    // MARK: - Entry points

    /// Ask, if it is time to ask. Returns immediately; everything happens later.
    static func check(force: Bool = false) {
        Task.detached(priority: .utility) {
            _ = await checkNow(force: force)
        }
    }

    /// Check now and answer when it is done: the id of the bundle waiting to
    /// go live, or nil.
    ///
    /// Backs `wait-for-update-check`, which is an **invoke** — so this must
    /// always return. Every failure inside is swallowed for that reason: an
    /// update check that cannot reach the network is no reason to leave a UI
    /// promise unresolved, and an unanswered invoke hangs for ever.
    ///
    /// Reports what is *staged*, not what this call downloaded, so a bundle
    /// fetched by an earlier background check is still offered.
    @discardableResult
    static func checkNow(force: Bool = true) async -> String? {
        guard !Capabilities.bundlePublicKey.isEmpty else {
            // No key compiled in means no way to tell a real manifest from any
            // other. The only safe behaviour is to do nothing at all — never
            // to fetch and trust.
            HostLog.bundle.debug("no bundle signing key in this build; over-the-air updates are off")
            return nil
        }

        lock.lock()
        if checking { lock.unlock(); return BundleStore.pending() }
        checking = true
        lock.unlock()
        defer { lock.lock(); checking = false; lock.unlock() }

        let defaults = UserDefaults.standard
        let since = Date().timeIntervalSince1970 - defaults.double(forKey: lastCheckKey)
        guard force || since >= throttle else { return BundleStore.pending() }

        guard let token = TokenStore.deviceToken, !token.isEmpty else {
            // Before the user has signed in there is nobody to ask on behalf
            // of. Not an error, and not worth a timestamp either — the check
            // should happen promptly once they do.
            HostLog.bundle.debug("no device registration yet; not checking for a bundle")
            return BundleStore.pending()
        }

        // Written before the work, not after: a check that fails slowly must
        // not be retried on every activation.
        defaults.set(Date().timeIntervalSince1970, forKey: lastCheckKey)

        do {
            guard let manifestJSON = try await fetchManifest(deviceToken: token) else {
                return BundleStore.pending()
            }
            let offer = try Core.evaluateBundle(
                manifest: manifestJSON,
                publicKey: Capabilities.bundlePublicKey,
                installed: BundleStore.installed(hostRevision: BridgeRouter.hostRevision)
            )
            guard offer.install else {
                HostLog.bundle.info("no update: \(offer.reason, privacy: .public)")
                return BundleStore.pending()
            }
            if BundleStore.pending() == offer.bundle {
                HostLog.bundle.info("bundle \(offer.bundle, privacy: .public) is already staged")
                return offer.bundle
            }

            HostLog.bundle.info("fetching bundle \(offer.bundle, privacy: .public) from \(offer.url, privacy: .public)")
            try await install(offer)
            return BundleStore.pending()
        } catch {
            HostLog.bundle.warning("update check failed: \(error.localizedDescription)")
            return BundleStore.pending()
        }
    }

    // MARK: - The request

    /// The manifest, as text.
    ///
    /// Returned unparsed on purpose. Parsing it here would mean a manifest
    /// existed in Swift before anything had checked its signature, and the
    /// first person to use one of its fields would have introduced a hole with
    /// no symptom. The core takes the raw string.
    private static func fetchManifest(deviceToken: String) async throws -> String? {
        guard let apiURL = HostStore.apiURL, !apiURL.isEmpty else {
            HostLog.bundle.debug("no API URL yet; not checking for a bundle")
            return nil
        }
        var components = URLComponents(string: apiURL + "/api/mobile/bundle/")
        components?.queryItems = [
            URLQueryItem(name: "platform", value: BundleStore.platform),
            URLQueryItem(name: "host", value: String(BridgeRouter.hostRevision)),
            URLQueryItem(name: "app", value: appVersion),
            URLQueryItem(name: "channel", value: channel),
        ]
        guard let url = components?.url else { throw Failure("the manifest URL could not be built") }

        var request = URLRequest(url: url)
        // The device token, not the user's. This is a property of the install,
        // and the repo's rule is that reporting-shaped traffic is device-signed.
        request.setValue("Bearer \(deviceToken)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.timeoutInterval = 30

        let (data, response) = try await URLSession.shared.data(for: request)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        // 204 is how the server says "nothing for you" — a channel with no
        // release, or a rollout this device is not in yet.
        if status == 204 {
            HostLog.bundle.info("the server has no bundle for this host")
            return nil
        }
        guard status == 200 else {
            // The URL belongs in the message: a wrong base URL, an undeployed
            // endpoint and a typo'd path otherwise look identical.
            throw Failure("HTTP \(status) from \(url.absoluteString)")
        }
        return String(data: data, encoding: .utf8)
    }

    // MARK: - The download

    private static func install(_ offer: Core.BundleOffer) async throws {
        var request = URLRequest(url: URL(string: offer.url)!)
        // No Authorization header: the CDN is public and signed for, and
        // sending the device token to a host outside our API would leak it to
        // whoever the manifest named.
        request.setValue("application/zip", forHTTPHeaderField: "Accept")
        request.timeoutInterval = 120

        let (temporary, response) = try await URLSession.shared.download(for: request)
        defer { try? FileManager.default.removeItem(at: temporary) }

        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard status == 200 else { throw Failure("HTTP \(status) fetching the bundle") }

        // An unreadable size is refused rather than treated as zero: the point
        // of the ceiling is that nothing unbounded reaches the unpack, and
        // "we could not tell" is not evidence that it is small.
        guard let size = (try? FileManager.default.attributesOfItem(atPath: temporary.path))?[.size] as? Int else {
            throw Failure("the size of the downloaded bundle archive could not be read")
        }
        guard size <= maxArchiveBytes else {
            throw Failure("the bundle archive is larger than \(maxArchiveBytes) bytes")
        }

        guard try Core.digestMatches(expected: offer.sha256, actual: digest(of: temporary)) else {
            // The signed digest and the delivered bytes disagree. This is the
            // check the whole signature exists to enable, so say so at error
            // level: a truncated download and a substituted archive look
            // identical here, and both are worth seeing.
            HostLog.bundle.error("bundle \(offer.bundle, privacy: .public) does not match its signed digest; discarding it")
            return
        }

        let staging = BundleStore.stagingDirectory()
        try unpack(archive: temporary, into: staging)
        if BundleStore.stage(unpacked: staging, id: offer.bundle, minHost: offer.minHost, serial: offer.serial) {
            onBundleStaged?(offer.bundle)
        }
    }

    /// SHA-256 of a file, streamed. Lowercase hex.
    private static func digest(of file: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: file)
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try handle.read(upToCount: 64 * 1024), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }

    // MARK: - The unpack

    /// Expand `archive` into `into`, refusing anything that tries to leave it.
    ///
    /// The digest proves the archive is the one that was signed; it says
    /// nothing about whether the archive is *well behaved*. An entry named
    /// `../../Library/Preferences/x.plist` is a valid zip entry and a naive
    /// `appendingPathComponent` resolves it happily — that is Zip Slip, and it
    /// is a file write anywhere this app can reach.
    ///
    /// Walked by hand rather than `unzipItem(at:to:)` so this check is ours.
    /// The entry and size ceilings are for the other shape of hostile archive:
    /// one small enough to sign that expands until the device is full.
    private static func unpack(archive: URL, into destination: URL) throws {
        let zip = try Archive(url: archive, accessMode: .read)
        let root = destination.standardizedFileURL
        var entries = 0
        // UInt64 to match the zip header's own type: converting to Int would
        // trap on a corrupt entry rather than fail the ceiling below.
        var written: UInt64 = 0

        for entry in zip {
            entries += 1
            guard entries <= maxEntries else { throw Failure("the bundle has more than \(maxEntries) entries") }

            // Symlinks are neither needed by a web bundle nor safe here: one
            // pointing outside the tree turns every later write through it
            // into an escape the path check above would not see.
            guard entry.type != .symlink else {
                throw Failure("the bundle contains a symlink: \(entry.path)")
            }

            let target = root.appendingPathComponent(entry.path).standardizedFileURL
            guard target.path == root.path || target.path.hasPrefix(root.path + "/") else {
                throw Failure("the bundle contains an entry outside itself: \(entry.path)")
            }

            if entry.type == .directory {
                try FileManager.default.createDirectory(at: target, withIntermediateDirectories: true)
                continue
            }

            written += entry.uncompressedSize
            guard written <= UInt64(maxUnpackedBytes) else {
                throw Failure("the bundle expands to more than \(maxUnpackedBytes) bytes")
            }

            try FileManager.default.createDirectory(
                at: target.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            _ = try zip.extract(entry, to: target)
        }
    }

    /// Which release track this build follows.
    ///
    /// A constant until there is something to switch: the server decides
    /// rollout within a channel, so a second channel only earns its place when
    /// someone needs to be on one deliberately.
    private static let channel = "stable"

    /// `CFBundleShortVersionString` — MARKETING_VERSION in project.yml.
    private static var appVersion: String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
    }

    private struct Failure: LocalizedError {
        let message: String
        init(_ message: String) { self.message = message }
        var errorDescription: String? { message }
    }
}

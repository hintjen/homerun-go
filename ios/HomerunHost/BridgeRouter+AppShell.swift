import AuthenticationServices
import Foundation
import UIKit

/// The non-server half of the bridge: config, identity, storage, clipboard,
/// deep links, first-run setup. Mostly thin passthroughs to platform APIs.
///
/// The table that registers these lives in `BridgeRouter.swift`, between the
/// conformance markers.
extension BridgeRouter {

    // MARK: - App / config

    /// `InitialConfig`. Every field is optional; the API URL is whatever the
    /// UI last told us via `set-api-url`, which is also why the host emits
    /// `get-api-url` at startup rather than inventing a default.
    func getInitialConfig(_ params: Any?) async throws -> Any? {
        var config: [String: Any] = [:]
        if let apiURL = HostStore.apiURL { config["apiUrl"] = apiURL }
        return config
    }

    func getSystemLanguage(_ params: Any?) async throws -> Any? {
        Locale.preferredLanguages.first ?? "en"
    }

    /// Which colour scheme the page settled on, `light` or `dark`.
    ///
    /// **The web layer cannot set the status bar itself.** In a WKWebView the
    /// clock and battery are drawn by the view controller, so the page has to
    /// say and the host has to act.
    ///
    /// Not derivable from the device: the UI's theme setting defaults to
    /// `system` but a player can pin it, and then the page and the phone
    /// disagree.
    func setAppearance(_ params: Any?) async throws -> Any? {
        guard let value = params as? String, let theme = PageTheme(rawValue: value) else {
            // A send has nobody to answer, so an unreadable one is only worth a
            // line — the status bar keeps whatever it last knew.
            HostLog.bridge.error("set-appearance sent something other than light or dark")
            return nil
        }
        events?.appearanceChanged(theme)
        return nil
    }

    /// The web splash has painted.
    ///
    /// Nothing to do on this platform, and that is worth stating rather than
    /// leaving as a silent stub: iOS tears its own launch screen down as soon
    /// as the app draws its first frame, so by the time this arrives the thing
    /// it asks to hide is already gone. Answering it is still not optional —
    /// an unimplemented channel is a channel the next contract sync reports as
    /// missing, and the host that ignores it is the one that hangs when the
    /// kind changes from a send to an invoke.
    func splashShown(_ params: Any?) async throws -> Any? {
        nil
    }

    /// What the user just did, for the Taptic Engine.
    ///
    /// The payload is a meaning rather than an instruction — `selection`,
    /// `commit` — and ``HapticsPlayer`` owns the translation into generators.
    ///
    /// An unrecognised value is dropped rather than raised, and that is the
    /// contract rather than laziness: `bridge/v1` is additive, so a pattern
    /// added later has to reach an older host as silence instead of an error.
    /// Throwing would be invisible anyway — a send has no `id` to answer.
    func haptic(_ params: Any?) async throws -> Any? {
        guard let value = params as? String, let pattern = HapticPattern(rawValue: value) else {
            HostLog.bridge.error("haptic sent an unknown pattern; ignoring")
            return nil
        }
        HapticsPlayer.play(pattern)
        return nil
    }

    func setPosthogDistinctID(_ params: Any?) async throws -> Any? {
        HostStore.posthogDistinctID = params as? String
        return nil
    }

    func cacheClientNonce(_ params: Any?) async throws -> Any? {
        HostStore.clientNonce = params as? String
        return nil
    }

    func clipboardWriteText(_ params: Any?) async throws -> Any? {
        guard let text = params as? String else {
            throw BridgeError("There was nothing to copy.")
        }
        UIPasteboard.general.string = text
        return nil
    }

    /// Present the OS share sheet.
    ///
    /// `UIActivityViewController`, which is what a share glyph promises on a
    /// phone: the system ranks targets by who this person actually shares with,
    /// and it does that better than a menu of our own could.
    ///
    /// The UI sends the sentence and the link apart — `text` carries the invite
    /// without the address — because `UIActivityViewController` composes the
    /// items itself. Handing it a URL as a `URL` rather than as more text is
    /// what lets Messages show a link preview and Safari offer a bookmark;
    /// flattened into one string it is just characters.
    ///
    /// Dismissal is an ordinary outcome. `completed: false` is what the UI
    /// reads to stay quiet — no toast, and no success haptic for something that
    /// did not happen.
    @MainActor
    func shareContent(_ params: Any?) async throws -> Any? {
        let payload = params as? [String: Any]
        let title = (payload?["title"] as? String)?.nonEmpty
        let text = (payload?["text"] as? String)?.nonEmpty
        let urlString = (payload?["url"] as? String)?.nonEmpty

        var items: [Any] = []
        // The sentence carries the subject with it. `setValue(_:forKey: "subject")`
        // is the shorter-looking way and it throws `NSUnknownKeyException` at
        // runtime — `UIActivityViewController` has no such property, and the
        // subject is only ever offered through an item source.
        if let text { items.append(ShareTextItem(text: text, subject: title)) }
        // Only a real web URL becomes a URL item; anything else stays a string
        // rather than handing the share extensions something they will try to
        // open. A URL that fails to parse is still worth sharing as text.
        if let urlString {
            if let url = URL(string: urlString), url.scheme?.hasPrefix("http") == true {
                items.append(url)
            } else {
                items.append(urlString)
            }
        }
        if items.isEmpty, let title { items.append(ShareTextItem(text: title, subject: title)) }
        guard !items.isEmpty else { return ["completed": false] }

        guard let presenter = Self.topViewController() else {
            // Nothing on screen to hang a sheet from. Answering rather than
            // throwing keeps this on the UI's ordinary path, and answering at
            // all is the part that matters: an unresolved invoke hangs the
            // page's promise for ever (PROTOCOL.md §5).
            return ["completed": false]
        }

        let activity = UIActivityViewController(activityItems: items, applicationActivities: nil)

        // iPad presents this as a popover and *crashes* without an anchor. The
        // page's own button is not reachable from here, so it hangs off the
        // middle of the presenter — the same place a sheet would appear.
        if let popover = activity.popoverPresentationController {
            popover.sourceView = presenter.view
            popover.sourceRect = CGRect(
                x: presenter.view.bounds.midX,
                y: presenter.view.bounds.midY,
                width: 0,
                height: 0)
            popover.permittedArrowDirections = []
        }

        // The continuation's type is spelled out rather than inferred from the
        // resume below, which is inside an escaping handler the compiler cannot
        // reach back through.
        return await withCheckedContinuation { (continuation: CheckedContinuation<[String: Bool], Never>) in
            // Called for both outcomes, and exactly once. `completed` is false
            // for a dismissal and also for an extension that failed, which the
            // UI treats the same way and should.
            activity.completionWithItemsHandler = { _, completed, _, _ in
                continuation.resume(returning: ["completed": completed])
            }
            presenter.present(activity, animated: true)
        }
    }

    /// The view controller a sheet should be presented from.
    ///
    /// Walks past anything already presented: the share can be raised from a
    /// screen that is itself inside a modal, and presenting from underneath one
    /// is silently ignored by UIKit.
    @MainActor
    static func topViewController() -> UIViewController? {
        let key = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap(\.windows)
            .first { $0.isKeyWindow }
        var top = key?.rootViewController
        while let presented = top?.presentedViewController {
            top = presented
        }
        return top
    }

    /// Returns false rather than throwing when the URL cannot be opened — the
    /// contract's result type is a plain boolean and the UI reads it.
    func openExternalURL(_ params: Any?) async throws -> Any? {
        guard let raw = params as? String, let url = URL(string: raw) else { return false }
        // Only hand the system a web URL. An arbitrary scheme from page
        // content could address another app on the device.
        guard let scheme = url.scheme?.lowercased(), scheme == "http" || scheme == "https" else {
            return false
        }
        guard UIApplication.shared.canOpenURL(url) else { return false }
        return await UIApplication.shared.open(url)
    }

    // MARK: - Identity

    /// The id the UI sends as a server's `device`, so it has to be the one the
    /// backend issued. Registers on first ask; null until sign-in makes that
    /// possible. See `DeviceRegistrar`.
    func getDeviceID(_ params: Any?) async throws -> Any? {
        await deviceRegistrar.deviceId()
    }

    /// The plaintext port the app's own UI dials for *this* device.
    ///
    /// Null until the link is up, and again once the app leaves the foreground
    /// — the socket lives exactly as long as the foreground does. Null is a
    /// valid answer meaning "no port", and the UI copes; it is asked again on
    /// the next page load. See `DeviceWebsocket`.
    func getDeviceWSPort(_ params: Any?) async throws -> Any? {
        DeviceWebsocket.shared.port
    }

    // MARK: - Journeys

    func journeyModalsGet(_ params: Any?) async throws -> Any? {
        HostStore.journeyModals
    }

    func journeyModalsSet(_ params: Any?) async throws -> Any? {
        guard let modals = params as? [String: Any] else { return false }
        HostStore.journeyModals = modals
        return true
    }

    // MARK: - Device facts

    /// Phone clocks are network-synced, so the desktop's clock-skew check has
    /// nothing to find here. The UI blocks login on this, so it must be true.
    func checkSystemTime(_ params: Any?) async throws -> Any? {
        true
    }

    /// "Installed" has no wizard on mobile — the server ships inside the app,
    /// so this is just "did the data directory get created".
    ///
    /// Setup runs at launch, so this is normally true by the time the UI asks.
    /// It retries rather than reporting false, because a false answer here
    /// sends the UI down the desktop installation path and strands it — see
    /// `HostStore.ensureFirstRunSetup`.
    func isInstalled(_ params: Any?) async throws -> Any? {
        if FileManager.default.fileExists(atPath: HostStore.serversDirectory.path) {
            return true
        }
        return HostStore.ensureFirstRunSetup()
    }

    /// The UI treats "native" as a locally-hosted server, which is exactly what
    /// this device is. There is no WSL on a phone.
    func getInstallType(_ params: Any?) async throws -> Any? {
        "native"
    }

    func getSystemMemory(_ params: Any?) async throws -> Any? {
        Self.memoryReport()
    }

    func getNativeSystemMemory(_ params: Any?) async throws -> Any? {
        Self.memoryReport()
    }

    /// Both memory channels share a shape: `memory` is a string because the
    /// desktop host reports whatever the platform gave it.
    private static func memoryReport() -> [String: Any] {
        let bytes = ProcessInfo.processInfo.physicalMemory
        let gb = Double(bytes) / 1_073_741_824.0
        return ["success": true, "memory": String(format: "%.0f", gb.rounded())]
    }

    /// `StorageInfo`. Only the device-wide figures are meaningful here; there
    /// is no install drive to choose on iOS.
    func getStorageInfo(_ params: Any?) async throws -> Any? {
        let url = HostStore.documentsDirectory
        var info: [String: Any] = ["installType": "native"]

        if let values = try? url.resourceValues(forKeys: [
            .volumeTotalCapacityKey, .volumeAvailableCapacityForImportantUsageKey,
        ]) {
            let toGB = { (bytes: Int64) in Double(bytes) / 1_073_741_824.0 }
            if let total = values.volumeTotalCapacity {
                info["totalStorageGB"] = toGB(Int64(total))
            }
            if let free = values.volumeAvailableCapacityForImportantUsage {
                info["totalStorageFreeGB"] = toGB(free)
                if let total = values.volumeTotalCapacity {
                    info["totalStorageUsedGB"] = toGB(Int64(total)) - toGB(free)
                }
            }
        }

        info["homerunStorage"] = Self.directorySizeGB(HostStore.serversDirectory)
        return info
    }

    private static func directorySizeGB(_ url: URL) -> Double {
        guard
            let enumerator = FileManager.default.enumerator(
                at: url, includingPropertiesForKeys: [.totalFileAllocatedSizeKey])
        else { return 0 }

        var total: Int64 = 0
        for case let file as URL in enumerator {
            let size = try? file.resourceValues(forKeys: [.totalFileAllocatedSizeKey])
                .totalFileAllocatedSize
            total += Int64(size ?? 0)
        }
        return Double(total) / 1_073_741_824.0
    }

    // MARK: - Region latency

    /// The contract's "unreachable", as a latency rather than an error.
    ///
    /// A number, not a throw, because the UI ranks regions by this and one bad
    /// host must not cost the whole list. It stays on this side because it is
    /// a *bridge* value rather than a measurement: the core answers nil for
    /// "could not measure", and what the UI receives instead is this
    /// protocol's business.
    ///
    /// Note the UI's own "nothing answered" test is `=== Infinity`, which this
    /// never trips — `JSON.stringify(Infinity)` is `null`, so no host can send
    /// it. See `docs/region-latency.md`.
    private static let unreachableMs = 9999

    /// Round-trip time to a region's gateway, in milliseconds.
    ///
    /// The whole measurement — splitting the address, resolving it, timing the
    /// handshake — is `net.regionLatency` in the native host. None of it is
    /// here, and that is the point: it *was* here, and in Kotlin, and both
    /// copies were wrong. Each treated the argument as a URL when it is a bare
    /// hostname, so every region came back ``unreachableMs`` and the picker
    /// ranked a list of ties. `homerun-core::region` has the post-mortem.
    func measureRegionLatency(_ params: Any?) async throws -> Any? {
        guard
            let domain = (params as? String)?
                .trimmingCharacters(in: .whitespacesAndNewlines),
            !domain.isEmpty
        else { return Self.unreachableMs }

        // The core call blocks for up to five seconds. Off the actor, or the
        // whole bridge stalls behind one slow region.
        let measured = await Task.detached { Core.regionLatency(domain: domain) }.value

        guard let ms = measured, ms.isFinite, ms >= 0 else { return Self.unreachableMs }
        return Int(ms.rounded())
    }

    // MARK: - Notifications

    /// Local notification. Permission is requested on first use rather than at
    /// launch: asking before the user has any idea what the app does is the
    /// reliable way to get denied.
    func pushNotification(_ params: Any?) async throws -> Any? {
        guard let payload = params as? [String: Any],
            let message = payload["message"] as? String
        else { return nil }
        await NotificationPresenter.show(
            title: payload["title"] as? String ?? "Homerun", body: message)
        return nil
    }

    // MARK: - Remote push (`remotePush` capability, revision 9)

    /// The host's half is the OS permission and the FCM token; registering
    /// the token with the API is the shared UI's job over the user's JWT —
    /// the same split as social sign-in, so no identity passes through here.
    /// Unlike Android there is no activity to borrow a launcher from: the
    /// permission sheet is a global system call.

    func pushPermission(_ params: Any?) async throws -> Any? {
        ["status": await PushMessaging.shared.permissionStatus()]
    }

    /// Prompts — because the UI asked, at a moment the user understands. A
    /// permission already decided resolves immediately with the truth: iOS
    /// shows the sheet exactly once per install and cannot re-prompt.
    func pushRequestPermission(_ params: Any?) async throws -> Any? {
        ["status": await PushMessaging.shared.requestPermission()]
    }

    /// Null is a state, not an error: the simulator has no APNs and a build
    /// without GoogleService-Info.plist has no Firebase, and both stay null
    /// for ever without breaking anything. `push:token-changed` announces a
    /// token arriving later.
    func pushGetToken(_ params: Any?) async throws -> Any? {
        // Spelled through an `Any` binding rather than inline: `String? ??
        // NSNull()` has no common type for Swift to infer, so the operands
        // have to meet as `Any` before `??` sees them.
        let token: Any = await PushMessaging.shared.currentToken() ?? NSNull()
        return ["token": token]
    }

    // MARK: - Deep links

    func deepLinkConsume(_ params: Any?) async throws -> Any? {
        deepLinks.consume()
    }

    // MARK: - Browser-based sign-in

    /// Run an OAuth redirect in a real browser and hand back where it landed.
    ///
    /// `ASWebAuthenticationSession` rather than our own WebView, because
    /// Google answers `disallowed_useragent` to `WKWebView` — which is the
    /// entire reason this channel exists. It is also the right tool
    /// independently: it shares Safari's cookies, so a user already signed in
    /// to Google is one tap from done, and it captures its own callback
    /// without going through the OS URL router. That last part matters —
    /// nothing here touches `DeepLinkManager`, so an auth callback can never
    /// be mistaken for a `homerun://` deep link and dropped as an unknown
    /// intent.
    ///
    /// The user dismissing the sheet is an ordinary outcome, not an error;
    /// Apple reports it as `.canceledLogin` and it is passed through as
    /// `canceled` so the page can stay quiet about it.
    @MainActor
    func authWebSession(_ params: Any?) async throws -> Any? {
        guard
            let dict = params as? [String: Any],
            let urlString = dict["url"] as? String,
            let url = URL(string: urlString)
        else {
            return ["success": false, "error": "No sign-in address was provided."]
        }
        let scheme = (dict["callbackScheme"] as? String) ?? "homerun"

        return await withCheckedContinuation { continuation in
            var session: ASWebAuthenticationSession?
            session = ASWebAuthenticationSession(
                url: url,
                callbackURLScheme: scheme
            ) { callbackURL, error in
                // Held until here purely so ARC does not release the session
                // mid-flight, which silently closes the sheet.
                _ = session
                if let callbackURL {
                    continuation.resume(returning: [
                        "success": true,
                        "callbackUrl": callbackURL.absoluteString,
                    ])
                    return
                }
                if let error = error as? ASWebAuthenticationSessionError,
                   error.code == .canceledLogin {
                    continuation.resume(returning: [
                        "success": false,
                        "error": "Sign-in was cancelled.",
                        "canceled": true,
                    ])
                    return
                }
                continuation.resume(returning: [
                    "success": false,
                    "error": "Could not open the sign-in page.",
                ])
            }
            session?.presentationContextProvider = authPresentationAnchor
            // Use the shared cookie jar. Without this the user is asked to
            // sign in to Google again even when Safari already knows them,
            // which is most of the value of not using a WebView.
            session?.prefersEphemeralWebBrowserSession = false
            if session?.start() != true {
                continuation.resume(returning: [
                    "success": false,
                    "error": "Could not open the sign-in page.",
                ])
            }
        }
    }
}

/// Tells `ASWebAuthenticationSession` which window to hang its sheet on.
///
/// File-scope rather than a property because Swift extensions cannot add
/// stored properties, and it holds no state worth per-router isolation — every
/// sign-in presents from the same key window.
private final class AuthPresentationAnchor: NSObject,
    ASWebAuthenticationPresentationContextProviding
{
    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap(\.windows)
            .first { $0.isKeyWindow } ?? ASPresentationAnchor()
    }
}

private let authPresentationAnchor = AuthPresentationAnchor()

/// One line of shared text, with the subject a mail target should start with.
///
/// `UIActivityViewController` only asks for a subject through this protocol.
/// Handing it a plain `String` item — which is all the share payload's `text`
/// is — means Mail composes with an empty subject line, and there is no
/// property on the controller to set instead.
private final class ShareTextItem: NSObject, UIActivityItemSource {
    private let text: String
    private let subject: String?

    init(text: String, subject: String?) {
        self.text = text
        self.subject = subject
    }

    /// Shown while the sheet works out what can accept the item. It must be the
    /// same *type* as the real item; the value is never used.
    func activityViewControllerPlaceholderItem(_ controller: UIActivityViewController) -> Any {
        text
    }

    func activityViewController(
        _ controller: UIActivityViewController,
        itemForActivityType activityType: UIActivity.ActivityType?
    ) -> Any? {
        text
    }

    func activityViewController(
        _ controller: UIActivityViewController,
        subjectForActivityType activityType: UIActivity.ActivityType?
    ) -> String {
        subject ?? ""
    }
}

extension String {
    /// The string, or nil when it is blank.
    ///
    /// The share payload's three fields are all optional and the UI omits what
    /// it has nothing to say for — but an empty string arrives as a present
    /// value, and passing one to `UIActivityViewController` puts an empty item
    /// in the sheet.
    fileprivate var nonEmpty: String? {
        trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : self
    }
}

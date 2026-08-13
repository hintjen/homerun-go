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

    /// The device WebSocket is parity work that has not landed on iOS yet.
    /// Null is a valid result and means "no port"; the UI copes.
    func getDeviceWSPort(_ params: Any?) async throws -> Any? {
        nil
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

    /// Round-trip time in milliseconds to a region endpoint, used to rank
    /// regions in the UI. A failure returns a large number rather than
    /// throwing, so one unreachable region cannot break the whole picker.
    func measureRegionLatency(_ params: Any?) async throws -> Any? {
        guard let raw = params as? String, let url = URL(string: raw) else { return 9999 }

        var request = URLRequest(url: url)
        request.httpMethod = "HEAD"
        request.timeoutInterval = 5
        request.cachePolicy = .reloadIgnoringLocalCacheData

        let started = Date()
        do {
            _ = try await URLSession.shared.data(for: request)
        } catch {
            return 9999
        }
        return Int(Date().timeIntervalSince(started) * 1000)
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

    // MARK: - Deep links

    func deepLinkConsume(_ params: Any?) async throws -> Any? {
        deepLinks.consume()
    }
}

import Foundation

/// An error whose message is shown to a player, verbatim, in the app.
///
/// Write it for someone who wants to play Minecraft: "Another server is
/// already running" — not "EADDRINUSE".
struct BridgeError: LocalizedError {
    let message: String
    let code: String?

    init(_ message: String, code: String? = nil) {
        self.message = message
        self.code = code
    }

    var errorDescription: String? { message }
}

/// How a handler emits an event. The controller supplies this; the router does
/// not hold the WebView.
@MainActor
protocol BridgeEventSink: AnyObject {
    func emit(_ event: String, _ args: [Any])
}

/// Maps a `bridge/v1` channel to the code that answers it.
///
/// `shared/conformance/check-coverage.js` reads the block between the
/// BRIDGE-CHANNELS markers below and treats every quoted string inside it as a
/// channel this host implements. Two consequences worth knowing before editing
/// it:
///
///  - The block must contain channel names and nothing else quoted. An inline
///    closure with a string literal in it would register as a channel that
///    does not exist, and the check would pass while the UI hangs. So the
///    table registers method references only.
///  - Do not park unimplemented channels there pointing at a stub that throws.
///    That turns the gate green while leaving the work undone; the failing
///    list from `npm run conformance:ios` is the to-do list for M3.
@MainActor
final class BridgeRouter {
    typealias Handler = (_ params: Any?) async throws -> Any?

    private(set) var handlers: [String: Handler] = [:]

    /// Set by `BridgeController` after construction — handlers emit through it.
    weak var events: BridgeEventSink?

    let deepLinks: DeepLinkManager
    let backend: PumpkinBackend

    init(deepLinks: DeepLinkManager, backend: PumpkinBackend) {
        self.deepLinks = deepLinks
        self.backend = backend

        func on(_ channel: String, _ handler: @escaping Handler) {
            handlers[channel] = handler
        }

        // BRIDGE-CHANNELS-BEGIN
        on("get-initial-config", getInitialConfig)
        on("get-app-version", getAppVersion)
        on("get-system-language", getSystemLanguage)
        on("set-posthog-distinct-id", setPosthogDistinctID)
        on("cache-client-nonce", cacheClientNonce)
        on("clipboard-write-text", clipboardWriteText)
        on("open-external-url", openExternalURL)
        on("push-notification", pushNotification)
        on("deep-link:consume", deepLinkConsume)
        on("check-system-time", checkSystemTime)
        on("get-device-id", getDeviceID)
        on("get-device-ws-port", getDeviceWSPort)
        on("measure-region-latency", measureRegionLatency)
        on("journey-modals-get", journeyModalsGet)
        on("journey-modals-set", journeyModalsSet)
        on("get-storage-info", getStorageInfo)
        on("get-system-memory", getSystemMemory)
        on("get-native-system-memory", getNativeSystemMemory)
        on("is-installed", isInstalled)
        on("get-install-type", getInstallType)
        on("set-api-url", setAPIURL)
        on("credentials-received", credentialsReceived)
        on("logout", logout)
        on("start-installation-or-check", startInstallationOrCheck)
        on("check-homerun-storage-limit", checkHomerunStorageLimit)
        on("open-storage-settings", openStorageSettings)
        on("native-server-start", nativeServerStart)
        on("native-server-stop", nativeServerStop)
        on("native-server-delete", nativeServerDelete)
        on("native-server-rcon", nativeServerRcon)
        on("native-server-active-ids", nativeServerActiveIds)
        on("native-server-get-uptime", nativeServerGetUptime)
        on("native-server-get-ops", nativeServerGetOps)
        on("native-server-get-mem-usage", nativeServerGetMemUsage)
        on("native-server-get-cpu-usage", nativeServerGetCpuUsage)
        on("native-server-get-players", nativeServerGetPlayers)
        on("native-server-get-perf-history", nativeServerGetPerfHistory)
        on("get-native-server-logs", getNativeServerLogs)
        on("get-native-server-port", getNativeServerPort)
        on("get-native-local-network", getNativeLocalNetwork)
        on("set-native-local-network", setNativeLocalNetwork)
        on("server-files-exist", serverFilesExist)
        on("open-server-files", openServerFiles)
        on("import-minecraft-world", importMinecraftWorld)
        // Not in the iOS profile — see BridgeRouter+DesktopStubs.swift.
        on("update-firewall-rules", updateFirewallRules)
        on("discord-get-status", discordGetStatus)
        on("discord-get-user-id", discordGetUserID)
        on("discord-open-app", discordOpenApp)
        on("discord-connect", ignoredSend)
        on("discord-page-update", ignoredSend)
        on("discord-wsl-server-update", ignoredSend)
        on("set-distro-tag", ignoredSend)
        // BRIDGE-CHANNELS-END
    }

    func handler(for channel: String) -> Handler? { handlers[channel] }

    /// `AppVersionInfo` — `version` and `commit` are required by the contract,
    /// the rest are optional. `commit` is null until the build stamps one in.
    func getAppVersion(_ params: Any?) async throws -> Any? {
        let info = Bundle.main.infoDictionary
        return [
            "version": info?["CFBundleShortVersionString"] as? String ?? "0.0.0",
            "commit": NSNull(),
            "platform": "ios",
            "arch": "arm64",
        ]
    }
}

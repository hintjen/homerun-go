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
/// BRIDGE-CHANNELS markers below, and counts a channel only where the string
/// is in *declaration position* — `"channel": handler` here, `"channel" to
/// handler` in Kotlin. Two consequences worth knowing before editing it:
///
///  - Keep the table a dictionary literal of method references. A router that
///    registers channels some other way reads as having no handlers at all,
///    which is the safe direction to fail but will stop CI dead.
///  - Do not park unimplemented channels here pointing at a stub that throws.
///    That turns the gate green while leaving the work undone; the failing
///    list from `npm run conformance:ios` is the to-do list.
@MainActor
final class BridgeRouter {
    typealias Handler = (_ params: Any?) async throws -> Any?

    private(set) var handlers: [String: Handler] = [:]

    /// Set by `BridgeController` after construction — handlers emit through it.
    weak var events: BridgeEventSink?

    let deepLinks: DeepLinkManager
    let backend: PumpkinBackend
    let deviceRegistrar = DeviceRegistrar()
    /// Only the lease gate is reached from here; the restore and the on-stop
    /// backup belong to the backend, which is where a run's start and end are.
    let backups = BackupManager()

    init(deepLinks: DeepLinkManager, backend: PumpkinBackend) {
        self.deepLinks = deepLinks
        self.backend = backend

        // BRIDGE-CHANNELS-BEGIN
        handlers = [
            "get-initial-config": getInitialConfig,
            "get-app-version": getAppVersion,
            "get-system-language": getSystemLanguage,
            "set-posthog-distinct-id": setPosthogDistinctID,
            "cache-client-nonce": cacheClientNonce,
            "clipboard-write-text": clipboardWriteText,
            "open-external-url": openExternalURL,
            "push-notification": pushNotification,
            "deep-link:consume": deepLinkConsume,
            "check-system-time": checkSystemTime,
            "get-device-id": getDeviceID,
            "get-device-ws-port": getDeviceWSPort,
            "measure-region-latency": measureRegionLatency,
            "journey-modals-get": journeyModalsGet,
            "journey-modals-set": journeyModalsSet,
            "get-storage-info": getStorageInfo,
            "get-system-memory": getSystemMemory,
            "get-native-system-memory": getNativeSystemMemory,
            "is-installed": isInstalled,
            "get-install-type": getInstallType,
            "set-api-url": setAPIURL,
            "credentials-received": credentialsReceived,
            "logout": logout,
            "start-installation-or-check": startInstallationOrCheck,
            "check-homerun-storage-limit": checkHomerunStorageLimit,
            "open-storage-settings": openStorageSettings,
            "native-server-start": nativeServerStart,
            "native-server-stop": nativeServerStop,
            "native-server-delete": nativeServerDelete,
            "native-server-rcon": nativeServerRcon,
            "native-server-active-ids": nativeServerActiveIds,
            "native-server-get-uptime": nativeServerGetUptime,
            "native-server-get-ops": nativeServerGetOps,
            "native-server-get-mem-usage": nativeServerGetMemUsage,
            "native-server-get-cpu-usage": nativeServerGetCpuUsage,
            "native-server-get-players": nativeServerGetPlayers,
            "native-server-get-perf-history": nativeServerGetPerfHistory,
            "get-native-server-logs": getNativeServerLogs,
            "get-native-server-port": getNativeServerPort,
            "get-native-local-network": getNativeLocalNetwork,
            "set-native-local-network": setNativeLocalNetwork,
            "server-files-exist": serverFilesExist,
            "open-server-files": openServerFiles,
            "import-minecraft-world": importMinecraftWorld,
        // Not in the iOS profile — see BridgeRouter+DesktopStubs.swift.
            "update-firewall-rules": updateFirewallRules,
            "discord-get-status": discordGetStatus,
            "discord-get-user-id": discordGetUserID,
            "discord-open-app": discordOpenApp,
            "discord-connect": ignoredSend,
            "discord-page-update": ignoredSend,
            "discord-wsl-server-update": ignoredSend,
            "set-distro-tag": ignoredSend,
        ]
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

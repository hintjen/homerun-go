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

    /// How far along `shared/conformance/host-revisions.json` this host is.
    ///
    /// `PROTOCOL.md` §7 versions the protocol and says changes are additive,
    /// which is why every host answers `v: 1` for ever and the UI cannot tell a
    /// January host from a July one. That is harmless while the bundle and the
    /// host ship in one binary, and stops being harmless the moment a bundle
    /// arrives over the air: a call to a channel this host has never heard of
    /// leaves a promise pending for ever, and the user sees a frozen screen
    /// with no error.
    ///
    /// So: bump this whenever the table below gains a channel, and add the
    /// matching ledger entry. `scripts/check-host-revision.js` compares the two
    /// and fails the build if you do one without the other.
    static let hostRevision = 1

    private(set) var handlers: [String: Handler] = [:]

    /// Set by `BridgeController` after construction — handlers emit through it.
    weak var events: BridgeEventSink?

    let deepLinks: DeepLinkManager
    let backend: PumpkinBackend
    let deviceRegistrar = DeviceRegistrar()
    /// Only the lease gate is reached from here; the restore and the on-stop
    /// backup belong to the backend, which is where a run's start and end are.
    let backups = BackupManager()

    // The router used to keep its own in-flight count, and the backend its own
    // claimed/active ids, so that `native-server-active-ids` could answer "is
    // this server this device's right now" rather than "is it running". Both
    // are gone: `homerun-core::lifecycle` holds it, reached through
    // `backend.lifecycle`, and a launch and a stop must see the same instance.
    //
    // Why it matters that this is one place and not three: the UI's reconcile
    // loop compares that list against the API's `target_state`, and an id
    // missing from it while the API still says `running` reads as a start
    // issued from another device — the loop asks the API to `force_link_up`,
    // which regenerates the gateway's keys underneath a launch that has
    // already resolved its tunnel config. Both ends of a server's life open
    // that window, and this host had bugs at both.

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

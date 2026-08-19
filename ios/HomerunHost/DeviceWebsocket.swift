import Foundation
import OSLog
// `os` for `Logger`, `OSLog` for `OSLogStore` — the reader lives in the second
// and the writer in the first, and importing only one of them compiles until
// the line that needs the other.
import os

/// The socket the dashboard talks to this device on.
///
/// The console, RCON and remote log-fetch in the web dashboard do not go
/// through the API — they connect to `wss://<device-fqdn>`, which the device
/// serves itself. `plans/device-websocket.md` has the whole design, the desktop
/// implementation it is ported from, and Android's `DeviceWebsocket.kt` is the
/// sibling this mirrors deliberately closely.
///
/// This class owns the link and the lifecycle. The socket, the TLS and the
/// ACME client are the supervisor's, because that is where the console buffer
/// and the command path already are — nothing crosses the FFI while a console
/// is streaming.
///
/// Two ports, both forwarded by the tunnel. The gateway's `:443` reaches the
/// TLS listener; its `:80` reaches the ACME challenge listener, and that
/// forward is omitted when there is no hostname to prove, because a forward at
/// a listener that never starts looks like the device answered.
///
/// # This lives exactly as long as the foreground does
///
/// The one real difference from Android, and it is the platform's rather than
/// this code's: iOS suspends the process, and a suspended process serves
/// nothing. There is no foreground service to hold it — see
/// `plans/ios-background-execution.md`, which sweeps every persistent-process
/// option and finds none that can hold a socket, let alone a server.
///
/// So the link comes up when the app becomes active and goes down when it
/// resigns. Leaving it "up" across a suspension would be worse than honest
/// teardown twice over: the gateway would hold a peer slot for a device that
/// cannot answer, and the dashboard would show a console that never fills. The
/// cost of the honest version is a link renegotiation per foreground, which is
/// one API call and a handshake.
@MainActor
final class DeviceWebsocket {

    static let shared = DeviceWebsocket()

    private var tunnel: WireProxy?
    private var task: Task<Void, Never>?

    /// The plaintext port, and what `get-device-ws-port` answers.
    ///
    /// The shared UI dials `ws://localhost:<port>` for the device it is running
    /// on and `wss://<fqdn>` only for other people's, so this is deliberately
    /// *not* the TLS port: a loopback client has no reason to present the
    /// public hostname as SNI, and the certificate is for that hostname.
    /// Reaching it still needs a Keycloak token and an API membership check.
    private(set) var port: Int?

    /// The device's public hostname, once the API has named it.
    private(set) var fqdn: String?

    private init() {}

    // MARK: - Lifecycle

    /// Bring the link up, if it is not already.
    ///
    /// Idempotent and safe to call from anywhere credentials might have just
    /// arrived — login calls it, and so does every foreground. Never awaited:
    /// provisioning polls for up to a minute and nothing in the UI should wait
    /// on it, because nothing in the UI depends on it. Health rides the
    /// instances heartbeat, so a device that serves no websocket still appears
    /// in the browse list.
    func ensure() {
        // `port` rather than `tunnel`: a socket that came up while its tunnel
        // did not is still a socket, and starting a second one answers "the
        // device websocket is already running" after a minute of polling for a
        // link nothing would use.
        guard task == nil, port == nil else { return }

        // Every way of declining says which one it was. Found by running it:
        // a first launch logged the registrar's "no credentials" and then
        // nothing at all from here, which reads identically to a link that
        // was attempted and failed. Silence is the failure mode this whole
        // subsystem is worst at — see `homerun-pumpkin-ffi`'s `host_log`.
        guard let apiURL = HostStore.apiURL, !apiURL.isEmpty else {
            HostLog.device.info("no API URL yet — not linking")
            return
        }
        guard let token = TokenStore.accessToken, !token.isEmpty else {
            HostLog.device.info("not signed in — this device will serve no websocket")
            return
        }
        // Registration is not started from here. It needs the token that
        // arrives at login and it is what `DeviceRegistrar` exists for; a
        // second registrar racing it would create a second device.
        guard let deviceId = HostStore.registeredDeviceId, !deviceId.isEmpty else {
            HostLog.device.info("no device id yet — nothing to link")
            return
        }
        HostLog.device.info("bringing the device link up")

        task = Task { [weak self] in
            await self?.bringUp(apiURL: apiURL, deviceId: deviceId, token: token)
            self?.task = nil
        }
    }

    /// Take the link, the socket and the tunnel down.
    ///
    /// The tunnel goes first, deliberately. While wireproxy is up the gateway
    /// can still hand it a connection, and a forward pointing at a port that
    /// has just been released is how a dashboard gets a refusal instead of a
    /// clean close.
    func stop() {
        // Only when there was something to take down. `stop` runs on every
        // backgrounding, and a line each time an app with no link goes into the
        // background is a log nobody reads twice.
        if port != nil {
            HostLog.device.info("the app left the foreground — taking the device link down")
        }

        task?.cancel()
        task = nil

        tunnel?.stop()
        tunnel = nil

        // Synchronous, on the main thread, and that is the right trade here.
        // The supervisor gives in-flight connections up to a second to end,
        // which is a second the app is not drawing anything anyway — and the
        // alternative, a detached task, is not guaranteed to run at all before
        // the system suspends the process. A socket left half-up across a
        // suspension is what this whole teardown exists to avoid.
        let reply = HomerunFFI.stopDeviceWebsocket()
        if !reply.ok, let error = reply.error {
            HostLog.device.error("the socket did not stop cleanly: \(error, privacy: .public)")
        }

        port = nil
        fqdn = nil
    }

    // MARK: - Bring-up

    private func bringUp(apiURL: String, deviceId: String, token: String) async {
        guard
            let link = await HomerunAPI.awaitDeviceLink(
                apiURL: apiURL, deviceId: deviceId, token: token)
        else { return }

        // The app can have been sent to the background while that polled. A
        // link brought up now would be one nothing can serve, and the teardown
        // that would have caught it has already run.
        guard !Task.isCancelled else {
            HostLog.device.info("the app left the foreground before the link was ready")
            return
        }

        // The ACME challenge listener's port has to be decided *before* the
        // socket starts, because the supervisor binds it during the order and
        // the tunnel has to be forwarding at it by then. This one is chosen
        // here rather than by the OS for that reason — the order and the
        // forward have to agree, and the order happens first.
        guard let challengePort = Self.freePort() else {
            HostLog.device.error("no free port for the ACME challenge listener")
            return
        }

        guard let bound = startSocket(apiURL: apiURL, deviceId: deviceId, link: link,
                                      challengePort: challengePort)
        else { return }

        do {
            let config = try Core.deviceWsTunnelConfig(
                link: link.link,
                // The **TLS** port, not the plaintext one. The gateway sends a
                // ClientHello; forwarding it at the loopback socket the app's
                // own UI uses would fail every handshake.
                httpsTarget: bound.tls,
                // Only when there is a hostname to prove. Without one no order
                // can run, and a forward at a listener that never starts looks
                // like the device answered.
                httpTarget: link.fqdn == nil ? nil : Int(challengePort))

            let proxy = WireProxy()
            proxy.onHandshakeFailed = {
                // Nothing user-facing: there is no console for a device link
                // the way there is for a server, and the dashboard's own
                // connection failing is the symptom the user would see anyway.
                HostLog.device.error("the gateway stopped answering this device's link")
            }
            try proxy.startRendered(
                config,
                describedAs: "device: https=127.0.0.1:\(bound.tls) http=127.0.0.1:\(challengePort)")
            tunnel = proxy
        } catch {
            HostLog.device.error(
                "the device tunnel did not start: \(error.localizedDescription, privacy: .public)")
            // The socket without the tunnel is reachable over loopback and
            // nowhere else, which is exactly what the app's own UI uses. Left
            // running on purpose: taking it down would cost the local console
            // to punish a failure that only affects remote access.
            port = bound.plaintext
            fqdn = link.fqdn
            return
        }

        port = bound.plaintext
        fqdn = link.fqdn
        HostLog.device.info(
            "device link up: fqdn=\(link.fqdn ?? "(unnamed)", privacy: .public) ws=:\(bound.plaintext, privacy: .public) tls=:\(bound.tls, privacy: .public) proxyProtocol=\(link.expectsProxyProtocol, privacy: .public)"
        )
    }

    /// The two ports the supervisor bound.
    private struct Bound {
        let plaintext: Int
        let tls: Int
    }

    /// Bring the socket up, and answer the ports it bound.
    ///
    /// Port 0 asks the OS to choose. Choosing here instead would leave a window
    /// between picking a number and binding it in which something else could
    /// take it, and the failure would land on the tunnel rather than here.
    private func startSocket(
        apiURL: String, deviceId: String, link: Core.DeviceLink, challengePort: UInt16
    ) -> Bound? {
        var config: [String: Any] = [
            "port": 0,
            "apiUrl": apiURL,
            "jwksUrl": Self.jwksURL,
            "deviceId": deviceId,
            "storageDir": Self.certificateDirectory().path,
            "challengePort": Int(challengePort),
            // Whether the plane in front of us writes a PROXY header. The core
            // answered this off the link; getting it wrong fails every TLS
            // handshake with a message about neither.
            "expectProxyProtocol": link.expectsProxyProtocol,
            // Staging in debug builds. Production allows five certificates per
            // hostname per week, and a developer reinstalling all afternoon
            // would spend that before lunch.
            "acmeStaging": Self.acmeStaging,
        ]
        if let fqdn = link.fqdn { config["fqdn"] = fqdn }

        guard let json = try? JSONSerialization.data(withJSONObject: config),
            let text = String(data: json, encoding: .utf8)
        else {
            HostLog.device.error("the socket config could not be encoded")
            return nil
        }

        let reply = HomerunFFI.startDeviceWebsocket(text)
        guard reply.ok, let object = reply.object,
            let plaintext = object["port"] as? Int, let tls = object["tlsPort"] as? Int
        else {
            HostLog.device.error(
                "the socket refused to start: \(reply.error ?? "no reason given", privacy: .public)")
            return nil
        }
        return Bound(plaintext: plaintext, tls: tls)
    }

    // MARK: - Storage

    /// Where the ACME account and the certificate live.
    ///
    /// **Application Support, not Documents, and excluded from backup.** It
    /// holds a private key: iCloud is the wrong place for one, and the Files
    /// app — which reaches everything under `Documents/` — is a worse one. The
    /// same rule the world directory follows, for a stronger reason.
    ///
    /// Losing it costs one ACME order, so a failure to create it is logged and
    /// not fatal; the supervisor degrades to serving no certificate.
    private static func certificateDirectory() -> URL {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        var directory = support.appendingPathComponent("device-ws/tls", isDirectory: true)
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            try directory.setResourceValues(values)
        } catch {
            HostLog.device.error(
                "the certificate store could not be prepared: \(error.localizedDescription, privacy: .public)"
            )
        }
        return directory
    }

    /// A port nothing else currently holds.
    ///
    /// Only the ACME challenge listener needs this — the websocket asks the OS
    /// for its own and reports back. Bound and released rather than hardcoded:
    /// a fixed number collides with whatever else on the device wanted it, and
    /// fails with less to say about why.
    private static func freePort() -> UInt16? {
        let descriptor = socket(AF_INET, SOCK_STREAM, 0)
        guard descriptor >= 0 else { return nil }
        defer { close(descriptor) }

        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = 0
        // Loopback, because that is where the listener will be. Binding the
        // wildcard would prove a port free on interfaces nothing will use.
        address.sin_addr.s_addr = inet_addr("127.0.0.1")

        let length = socklen_t(MemoryLayout<sockaddr_in>.size)
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(descriptor, $0, length) }
        }
        guard bound == 0 else { return nil }

        var assigned = sockaddr_in()
        var assignedLength = length
        let read = withUnsafeMutablePointer(to: &assigned) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                getsockname(descriptor, $0, &assignedLength)
            }
        }
        guard read == 0 else { return nil }

        let port = UInt16(bigEndian: assigned.sin_port)
        return port == 0 ? nil : port
    }

    // MARK: -

    #if DEBUG
        private static let acmeStaging = true
    #else
        private static let acmeStaging = false
    #endif

    /// Keycloak's signing keys.
    ///
    /// One URL, whatever the API host is — which is what the desktop does
    /// (`deviceWebsocket/auth.ts`) and is the safer shape anyway: a device that
    /// could be told where to find "the" signing keys is a device that can be
    /// told to trust somebody else's.
    private static let jwksURL =
        "https://auth.gethomerun.app/realms/FractalKeycloak/protocol/openid-connect/certs"
}

// MARK: - This app's own logs

/// Hand the supervisor a way to read this app's logs, for `get-app-logs`.
///
/// Android needs none: logcat holds its own process's entries and the crate
/// reads them directly. iOS logs to the unified logging system, which only
/// `OSLogStore` can read and only Swift can call — so the crate calls back.
///
/// Called once at launch, deliberately: the provider is registered long before
/// anyone knows whether a socket will ever come up, so a support request never
/// races the link.
func registerAppLogsProvider() {
    HomerunFFI.setAppLogsProvider(collectAppLogsIntoBuffer)
}

/// Give the crate's own diagnostics somewhere to land.
///
/// Nothing the supervisor logs is visible on iOS otherwise. `os_log` is
/// unreachable from Rust — its entry points are C macros, not functions — and
/// printing is not an alternative, because after a launch stdout is the pipe
/// feeding the player-visible console. A certificate that is ordered, issued,
/// stored and never served would look exactly like one that was never ordered.
///
/// Registered at launch, before anything can fail: a failure that happens
/// before the sink exists is a failure nobody can explain afterwards.
func registerNativeLogSink() {
    HomerunFFI.setLogSink(writeNativeLogLine)
}

/// One line from the crate, written under the `device` category.
///
/// A C function pointer, so it captures nothing. `os.Logger` takes an
/// interpolation rather than a string, and every field here is marked public
/// deliberately: these lines are the crate's own diagnostics, they contain no
/// player data, and a redacted `<private>` in the middle of one is how a
/// support flow ends up reading "the order failed: <private>".
private let writeNativeLogLine: @convention(c) (UInt8, UnsafePointer<CChar>?) -> Void = {
    level, message in
    guard let message else { return }
    let text = String(cString: message)
    switch level {
    case 1: HostLog.device.error("native: \(text, privacy: .public)")
    case 2: HostLog.device.warning("native: \(text, privacy: .public)")
    default: HostLog.device.info("native: \(text, privacy: .public)")
    }
}

/// Fill `buffer` with this process's recent log, and answer how many bytes.
///
/// A C function pointer, so it captures nothing — the crate calls it from a
/// worker thread at a moment of somebody else's choosing, and a captured
/// reference would be one Rust keeps alive with no way to say when it is done.
///
/// `OSLogStore(scope: .currentProcessIdentifier)` can see this process and no
/// other, which is the same line logd draws for `logcat --pid` on Android: this
/// cannot be widened into somebody else's device, which is what makes it safe
/// to expose to a support flow.
///
/// Errors answer -1 rather than an empty buffer. Empty reads as "nothing went
/// wrong", and the difference matters most in the one situation this is used
/// in — somebody looking at a device they cannot hold.
private let collectAppLogsIntoBuffer: @convention(c) (UnsafeMutablePointer<CChar>?, Int) -> Int = {
    buffer, capacity in
    guard let buffer, capacity > 0 else { return -1 }

    guard let text = recentAppLog() else { return -1 }

    // The *end* of a log is the part that explains a problem, so an
    // over-long one is cut at the front. A cut may land inside a character;
    // the crate decodes lossily for exactly this reason, and then trims to a
    // line boundary itself.
    var bytes = Array(text.utf8)
    if bytes.count > capacity { bytes = Array(bytes.suffix(capacity)) }
    bytes.withUnsafeBufferPointer { source in
        if let base = source.baseAddress {
            buffer.withMemoryRebound(to: UInt8.self, capacity: bytes.count) { destination in
                destination.update(from: base, count: bytes.count)
            }
        }
    }
    return bytes.count
}

/// This process's entries, oldest first, formatted one per line.
///
/// The window is bounded in *time* as well as in bytes. `OSLogStore` reads from
/// a store the whole system writes to, and asking it for everything since the
/// process began is a scan that gets slower the longer the app has been open —
/// on the thread of whoever asked for the logs.
private func recentAppLog() -> String? {
    do {
        let store = try OSLogStore(scope: .currentProcessIdentifier)
        let since = store.position(date: Date().addingTimeInterval(-appLogWindow))

        // No predicate. Everything this process logged is wanted, including
        // what the frameworks under it said — the same breadth `logcat --pid`
        // gives Android, and the framework lines are often the ones that
        // explain a networking failure the app only saw the result of.
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm:ss.SSS"

        var text = ""
        for entry in try store.getEntries(at: since) {
            guard let log = entry as? OSLogEntryLog else { continue }
            // The category, spaced and colon-terminated, is what the crate
            // splits the renderer's half out on. Nothing on iOS writes under
            // the WebView's category today — WKWebView exposes no console
            // callback the way Android's WebChromeClient does — so that half
            // arrives empty rather than wrong.
            text += "\(formatter.string(from: log.date)) \(log.category): \(log.composedMessage)\n"
        }
        return text
    } catch {
        HostLog.host.error(
            "the app log could not be read: \(error.localizedDescription, privacy: .public)")
        return nil
    }
}

/// How far back a support request reaches. Long enough to hold a launch and a
/// failed server start, short enough that reading it is not itself the problem.
private let appLogWindow: TimeInterval = 30 * 60

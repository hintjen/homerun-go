import Foundation
import WireproxyIOS

/// The tunnel that makes a phone-hosted server reachable from anywhere.
///
/// A phone on cellular sits behind CGNAT, so there is no port-forwarding
/// fallback the way there is on desktop: without this, a server runs and
/// nobody outside the local Wi-Fi can join.
///
/// Traffic is *ingress* — the Homerun gateway is the client, the phone is the
/// server:
///
///     player → gateway → WireGuard → <phone wg IP>:25565
///                      → [TCPServerTunnel] → 127.0.0.1:<minecraft port>
///
/// > **No VPN permission is involved, and that is the most important property
/// > of this design.** wireproxy terminates WireGuard in its own userspace
/// > netstack; the interface address is virtual inside the process and is
/// > never registered with the OS. No `NEPacketTunnelProvider`, no VPN
/// > profile, no system prompt. Android's `WireProxy.kt` says the same thing
/// > about the same design.
///
/// The config text is generated to match `wireproxyConfig.ts` (desktop) and
/// `WireProxy.kt` (Android) byte for byte. The same gateway is on the other
/// end of all three, so divergence is a bug by definition.
@MainActor
final class WireProxy {

    /// Tunnel credentials, as the API reports them in `native_config`.
    struct Link: Equatable {
        let clientPrivateKey: String
        let gatewayPublicKey: String
        /// `host:port` of the gateway's WireGuard endpoint.
        let endpoint: String
        /// Gateway-v2 only. v2 runs one shared multi-peer interface and
        /// allocates a unique /32 per peer; absent means legacy, where the
        /// fixed pair below applies.
        let address: String?
        let allowedIps: String?
    }

    /// Fired once when the tunnel has been down long enough to call it dead.
    var onHandshakeFailed: (() -> Void)?

    private var tunnel: WireproxyiosTunnel?
    private var watchdog: Timer?
    private var openedAt: Date?

    var isRunning: Bool { tunnel != nil }

    // MARK: - Config

    /// Render the wireproxy configuration.
    ///
    /// > **`ListenPort` is a fixed constant, not the local port.** The gateway
    /// > unconditionally DNATs player traffic to port 25565 on the WireGuard
    /// > interface, whatever the server happens to have bound locally. Only
    /// > `Target` follows the real port. Changing the listen port silently
    /// > makes the server unreachable.
    ///
    /// `MTU = 1280` and `PersistentKeepalive = 30` are load-bearing for NAT
    /// traversal on an inbound tunnel.
    ///
    /// TCP only. Desktop and Android also render UDP sections for Bedrock
    /// (19132) and Simple Voice Chat (24454); neither can exist on iOS, which
    /// is Pumpkin-only and therefore vanilla-only — it runs no plugin, and
    /// Bedrock is rejected before it reaches here.
    static func render(link: Link, minecraftPort: Int) -> String {
        [
            "[Interface]",
            "PrivateKey = \(link.clientPrivateKey)",
            "Address = \(link.address ?? "10.0.0.2/24")",
            "MTU = 1280",
            "",
            "[Peer]",
            "PublicKey = \(link.gatewayPublicKey)",
            "Endpoint = \(link.endpoint)",
            "AllowedIPs = \(link.allowedIps ?? "10.0.0.1/32")",
            "PersistentKeepalive = 30",
            "",
            "[TCPServerTunnel]",
            "ListenPort = \(Self.gatewayJavaPort)",
            "Target = 127.0.0.1:\(minecraftPort)",
            "",
        ].joined(separator: "\n")
    }

    // MARK: - Lifecycle

    /// Bring the tunnel up. Throws with a player-facing message.
    ///
    /// The config is handed over as a string and never written to disk — it
    /// holds a private key. Android has to write a file and then chmod it;
    /// running in-process removes the need.
    func start(link: Link, minecraftPort: Int) throws {
        stop()

        var error: NSError?
        let config = Self.render(link: link, minecraftPort: minecraftPort)
        guard let started = WireproxyiosStart(config, &error) else {
            throw ServerBackendError.engine(
                "The connection to the Homerun gateway could not be opened"
                    + (error.map { ": \($0.localizedDescription)" } ?? "."))
        }

        // Endpoint and address only — never the key material.
        HostLog.tunnel.info(
            "up: peer=\(link.endpoint, privacy: .public) address=\(link.address ?? "10.0.0.2/24", privacy: .public) allowed=\(link.allowedIps ?? "10.0.0.1/32", privacy: .public) target=127.0.0.1:\(minecraftPort, privacy: .public)"
        )

        tunnel = started
        openedAt = Date()
        startWatchdog()
    }

    func stop() {
        watchdog?.invalidate()
        watchdog = nil
        openedAt = nil
        tunnel?.stop()
        tunnel = nil
    }

    // MARK: - Health

    /// Watch for a tunnel that never comes up, or stops coming up.
    ///
    /// Desktop and Android detect this by counting the string "Handshake did
    /// not complete after 5 seconds" ten times in wireproxy's stdout — roughly
    /// 50 seconds. In-process there is no child stdout to read, and
    /// wireguard-go's logger writes to fd 1, which the Rust layer redirects
    /// into the player-visible console. The device knows the answer exactly,
    /// so ask it instead: same threshold, a real signal.
    private func startWatchdog() {
        watchdog = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.checkHandshake() }
        }
    }

    private func checkHandshake() {
        guard let tunnel, let openedAt else { return }

        let last = tunnel.lastHandshakeUnix()

        // Two different questions, and conflating them kills healthy tunnels.
        //
        // *Never* handshaken means the gateway is not answering, measured from
        // when the interface came up.
        //
        // Handshaken once and then quiet is **normal**. WireGuard renews a
        // session roughly every two minutes, and only when there is traffic —
        // keepalives do not produce a new handshake. So the timestamp
        // legitimately sits still on an idle tunnel, and it only means
        // anything once it passes the protocol's own reject window, after
        // which the session is dead by definition rather than by our guess.
        let age: TimeInterval
        let deadline: TimeInterval
        if last == 0 {
            age = Date().timeIntervalSince(openedAt)
            deadline = Self.firstHandshakeDeadline
        } else {
            age = Date().timeIntervalSince1970 - Double(last)
            deadline = Self.sessionDeadline
        }

        guard age > deadline else { return }
        HostLog.tunnel.error(
            "no handshake for \(Int(age), privacy: .public)s (limit \(Int(deadline), privacy: .public)s) — treating the tunnel as dead"
        )

        // Once only — the backend's response is to stop the server, and it
        // must not be asked twice.
        watchdog?.invalidate()
        watchdog = nil
        onHandshakeFailed?()
    }

    // MARK: -

    /// The port the gateway DNATs Java traffic to on the WireGuard interface.
    /// A contract with the gateway, not a local choice.
    private static let gatewayJavaPort = 25565

    /// How long to wait for the *first* handshake before giving up. Matches
    /// desktop and Android, which notice at ten missed attempts, five seconds
    /// apart.
    private static let firstHandshakeDeadline: TimeInterval = 50

    /// How stale an established session may get before it is certainly dead.
    ///
    /// WireGuard's own `REJECT_AFTER_TIME` is 180 s: past that it refuses to
    /// use the session at all. A healthy tunnel rekeys well inside that, so
    /// anything beyond it is genuinely gone rather than merely idle. Do not
    /// lower this to the 50 s above — that number counts *failed attempts* on
    /// the other platforms, and a healthy idle tunnel makes none.
    private static let sessionDeadline: TimeInterval = 180
}

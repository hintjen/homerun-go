import Foundation

/// Being found on the local network: the player's toggle, persisted.
///
/// "Expose Minecraft server to your local network" is a checkbox the shared
/// UI shows for every native Java server, backed by `get-` and
/// `set-native-local-network`. Until this file existed iOS answered `true`
/// and did nothing on set, while the linked engine listened on every
/// interface regardless — Pumpkin's default is `0.0.0.0` and nothing set its
/// address. `docs/ios-server-backend.md` had it filed under "still open".
///
/// Now it means what it says: **off**, the default, binds loopback and the
/// server is reachable through the gateway tunnel and nowhere else; **on**
/// binds every interface *and* turns on Pumpkin's own LAN broadcast, so a
/// Java client on the same Wi-Fi lists the server without knowing the phone's
/// address. Both are the core's rule (`minecraft::lan::bind`), applied in
/// Rust from the start request; this file only remembers the answer.
///
/// The broadcast needs Apple's multicast entitlement
/// (`com.apple.developer.networking.multicast`) before a datagram leaves the
/// phone. Without it Pumpkin binds its socket and every send fails quietly,
/// so the toggle is harmless but incomplete until that is granted — see
/// `HomerunHost.entitlements`.
enum LocalNetwork {
    /// Beside the world, so it is deleted with the server.
    private static let file = "homerun-local-network.json"

    static func isEnabled(serverId: String) -> Bool {
        let url = HostStore.serverDirectory(id: serverId).appendingPathComponent(file)
        guard let data = try? Data(contentsOf: url),
            let record = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else { return false }
        return record["enabled"] as? Bool ?? false
    }

    /// Takes effect on the next start; the UI only offers it while stopped.
    static func set(serverId: String, enabled: Bool) throws {
        let dir = HostStore.serverDirectory(id: serverId)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let data = try JSONSerialization.data(withJSONObject: ["enabled": enabled])
        try data.write(to: dir.appendingPathComponent(file), options: .atomic)
    }
}

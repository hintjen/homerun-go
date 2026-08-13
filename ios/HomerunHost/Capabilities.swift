import Foundation
import WebKit

/// The `HostCapabilities` object the UI reads to decide which surfaces exist.
///
/// It is loaded out of the vendored `bridge-v1.json` rather than transcribed
/// into Swift. The contract is generated from the UI repo's `channels.ts` and
/// re-vendored by `scripts/sync-contract.js`; anything hand-copied here would
/// silently fall behind it, and a *missing* field is a host bug rather than a
/// default the UI fills in (PROTOCOL.md §4.1).
enum Capabilities {
    /// Injected at document start — the UI resolves capabilities synchronously
    /// as its first act and cannot await the host.
    ///
    /// `__homerunHostRevision` rides along for the same reason it has to be
    /// synchronous, and it is a sibling global rather than a capability field
    /// because capabilities are generated from the contract while this is a
    /// property of the binary. A bundle delivered over the air can be newer
    /// than the host under it, and a feature gated on a channel this host does
    /// not answer must render as absent rather than as a button that hangs.
    ///
    /// Android has published this since the OTA work landed; iOS declared a
    /// revision and told nobody, which made the number decorative here.
    static func userScript() -> WKUserScript {
        let json = BridgeEnvelope.jsLiteral(object: profile())
        return WKUserScript(
            source: """
                window.__homerunCapabilities = \(json);
                window.__homerunHostRevision = \(BridgeRouter.hostRevision);
                """,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true)
    }

    /// `profiles.ios.capabilities` from the bundled contract.
    ///
    /// Fatal on failure, and deliberately so: a host that boots without
    /// capabilities renders a UI making wrong assumptions about the device —
    /// a crash at launch is cheaper to diagnose than that.
    static func profile() -> [String: Any] {
        guard let url = Bundle.main.url(forResource: "bridge-v1", withExtension: "json"),
            let data = try? Data(contentsOf: url),
            let manifest = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let profiles = manifest["profiles"] as? [String: Any],
            let ios = profiles["ios"] as? [String: Any],
            let capabilities = ios["capabilities"] as? [String: Any]
        else {
            fatalError(
                "bridge-v1.json is missing or has no profiles.ios.capabilities. "
                    + "It ships as a bundle resource — check ios/project.yml.")
        }
        return capabilities
    }
}

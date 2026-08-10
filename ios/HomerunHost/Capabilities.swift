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
    static func userScript() -> WKUserScript {
        let json = BridgeEnvelope.jsLiteral(object: profile())
        return WKUserScript(
            source: "window.__homerunCapabilities = \(json);",
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

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

    /// The Ed25519 public key over-the-air bundle manifests are signed with.
    ///
    /// Must be byte-identical to Android's `bundlePublicKey` default in
    /// `android/app/build.gradle.kts` — one key signs for both platforms, and
    /// the publish workflow signs once.
    ///
    /// Public by nature, which is the whole point of signing asymmetrically,
    /// so it is checked in rather than injected at build time. **Empty
    /// disables over-the-air updates entirely**: `BundleUpdater` refuses to
    /// fetch what it cannot verify, and a build that quietly stopped updating
    /// is far harder to notice than one that never started.
    ///
    /// Changing it needs an App Store release. A device only accepts manifests
    /// signed by the key compiled into *it*, so every installed copy holds the
    /// old key until it updates through the store.
    static let bundlePublicKey = "8d44ecfa010fe0136b450baee986a352cd027d3555403f0662dce5eb2ff16f4e"

    /// Whether this build has anything to do with over-the-air bundles at all.
    ///
    /// `HOMERUN_OTA_UPDATES=0` (see `ios/project.yml`) is for a development
    /// build whose whole point is the UI just staged into the app's `web/`.
    /// Off means **ignore them entirely**, not merely "do not fetch":
    /// `BundleStore` promotes nothing, serves the shipped copy, and reports no
    /// pending bundle. Nothing on disk is touched, so a build with it back on
    /// carries on where it left off. Android's `-PotaUpdates=off`.
    ///
    /// **Defaults to on, and every ambiguous reading resolves to on.** A
    /// missing key means a project generated before this existed; an
    /// unsubstituted `$(…)` means the setting reached Info.plist undefined.
    /// Both are accidents, and an app that silently stopped updating is far
    /// harder to notice than one that never started — the same argument as the
    /// empty signing key above, in the other direction.
    static let otaUpdates: Bool = {
        guard
            let raw = Bundle.main.object(forInfoDictionaryKey: "HomerunOTAUpdates") as? String,
            !raw.isEmpty,
            !raw.hasPrefix("$(")
        else { return true }
        return !["0", "no", "off", "false"].contains(raw.lowercased())
    }()

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

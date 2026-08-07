import Foundation
import UIKit

/// Small persistent values the app shell owns: the API URL the UI hands us,
/// analytics identity, and journey-modal state.
///
/// `UserDefaults` rather than a file: these are a handful of scalars read on
/// the boot path, and losing them on reinstall is correct behaviour.
enum HostStore {
    private static let defaults = UserDefaults.standard

    private enum Key {
        static let apiURL = "homerun.apiURL"
        static let posthogDistinctID = "homerun.posthogDistinctId"
        static let clientNonce = "homerun.clientNonce"
        static let journeyModals = "homerun.journeyModals"
        static let installed = "homerun.firstRunComplete"
    }

    static var apiURL: String? {
        get { defaults.string(forKey: Key.apiURL) }
        set { defaults.set(newValue, forKey: Key.apiURL) }
    }

    static var posthogDistinctID: String? {
        get { defaults.string(forKey: Key.posthogDistinctID) }
        set { defaults.set(newValue, forKey: Key.posthogDistinctID) }
    }

    static var clientNonce: String? {
        get { defaults.string(forKey: Key.clientNonce) }
        set { defaults.set(newValue, forKey: Key.clientNonce) }
    }

    static var journeyModals: [String: Any] {
        get { defaults.dictionary(forKey: Key.journeyModals) ?? [:] }
        set { defaults.set(newValue, forKey: Key.journeyModals) }
    }

    /// Mobile has no install wizard, so "installed" means first-run setup
    /// created the data directory.
    static var firstRunComplete: Bool {
        get { defaults.bool(forKey: Key.installed) }
        set { defaults.set(newValue, forKey: Key.installed) }
    }

    // MARK: - Locations

    /// Everything the host owns lives under `Documents/`, so a user can reach
    /// their worlds through the Files app.
    static var documentsDirectory: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    }

    static var serversDirectory: URL {
        documentsDirectory.appendingPathComponent("servers", isDirectory: true)
    }

    static func serverDirectory(id: String) -> URL {
        serversDirectory.appendingPathComponent(id, isDirectory: true)
    }

    /// A stable per-install identifier. `identifierForVendor` resets when the
    /// last app from this vendor is removed, which is the correct lifetime —
    /// but it can be nil early in boot, so the value is persisted once.
    static func deviceID() -> String? {
        if let existing = defaults.string(forKey: "homerun.deviceId") { return existing }
        guard let vendor = UIDevice.current.identifierForVendor?.uuidString else { return nil }
        defaults.set(vendor, forKey: "homerun.deviceId")
        return vendor
    }
}

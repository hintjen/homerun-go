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
        static let registeredDeviceId = "homerun.registeredDeviceId"
        static let deviceGroupId = "homerun.deviceGroupId"
        static let registeredDeviceAccount = "homerun.registeredDeviceAccount"
        static let currentAccount = "homerun.currentAccount"
        static let pendingBackupReports = "homerun.pendingBackupReports"
    }

    // MARK: - The backup-state outbox

    /// Backup reports that have not reached the API yet, keyed by server id.
    ///
    /// # Why this is on disk and not in a Task
    ///
    /// Reporting `backup-state` is what **closes the backup lease**, and the
    /// lease has no timeout. A report that never lands leaves every other
    /// device locked out of that world until this one launches again and acks
    /// `running`. The escape hatch is a force-launch, which shows the user a
    /// data-loss warning for what is usually just a phone that got closed.
    ///
    /// A fire-and-forget request racing app suspension is exactly the case
    /// that loses one. So the body is written here *before* the POST is tried
    /// and cleared when it succeeds; anything left is retried at launch.
    ///
    /// Only ever one entry per server, and a later report supersedes an
    /// earlier one — the lease does not care which outcome closes it.
    static var pendingBackupReports: [String: [String: Any]] {
        get { defaults.dictionary(forKey: Key.pendingBackupReports) as? [String: [String: Any]] ?? [:] }
        set { defaults.set(newValue, forKey: Key.pendingBackupReports) }
    }

    static func rememberBackupReport(serverId: String, body: [String: Any]) {
        var pending = pendingBackupReports
        pending[serverId] = body
        pendingBackupReports = pending
    }

    static func forgetBackupReport(serverId: String) {
        var pending = pendingBackupReports
        pending.removeValue(forKey: serverId)
        pendingBackupReports = pending
    }

    /// The device id the *backend* issued, from `/api/init/native/`.
    ///
    /// Distinct from `deviceID()` below, which is a local identifier and is
    /// not something the API will accept — sending it as a server's `device`
    /// is rejected with "does not exist".
    static var registeredDeviceId: String? {
        get { defaults.string(forKey: Key.registeredDeviceId) }
        set { defaults.set(newValue, forKey: Key.registeredDeviceId) }
    }

    static var deviceGroupId: String? {
        get { defaults.string(forKey: Key.deviceGroupId) }
        set { defaults.set(newValue, forKey: Key.deviceGroupId) }
    }

    /// The account `registeredDeviceId` was registered to, as a matrix id.
    ///
    /// A device row belongs to exactly one account, so a registration made as
    /// one account is not usable as another: the backend refuses it as
    /// somebody else's hardware. See `DeviceRegistrar.deviceId()`.
    static var registeredDeviceAccount: String? {
        get { defaults.string(forKey: Key.registeredDeviceAccount) }
        set { defaults.set(newValue, forKey: Key.registeredDeviceAccount) }
    }

    /// The account signed in right now, as the matrix id the UI handed over
    /// with the credentials.
    ///
    /// The matrix id rather than the address: claiming a guest account rotates
    /// the address while staying the same account on the same device, so
    /// keying on email would re-register for no reason every time somebody
    /// signed up.
    static var currentAccount: String? {
        get { defaults.string(forKey: Key.currentAccount) }
        set { defaults.set(newValue, forKey: Key.currentAccount) }
    }

    /// The backend this app talks to.
    ///
    /// Whatever the page last told us, and otherwise the value this build was
    /// compiled with — which is the parity Android has had all along through
    /// `BuildConfig.API_URL`.
    ///
    /// Without the fallback there was no way to point an iOS build anywhere.
    /// This is written only by the page (`set-api-url`, and the credentials
    /// handler), so on a fresh install it was nil, `get-initial-config` omitted
    /// `apiUrl` entirely, and the page fell back to its own compiled-in
    /// production default. Reaching staging meant editing `localStorage` by hand
    /// in the Web Inspector.
    ///
    /// The page still wins once it has an opinion, deliberately: someone who
    /// pointed this app at a laptop expects it to stay there across a relaunch.
    /// See `docs/building.md` § *Which backend a build talks to*.
    static var apiURL: String? {
        get { defaults.string(forKey: Key.apiURL) ?? buildTimeAPIURL }
        set { defaults.set(newValue, forKey: Key.apiURL) }
    }

    /// `HOMERUN_API_URL` as it reached `Info.plist` at build time.
    ///
    /// Empty counts as absent, because that is what Xcode substitutes for a
    /// setting nobody defined; the raw `$(…)` is checked for the case where
    /// substitution did not happen at all. Either would otherwise become a URL
    /// every request fails against, which is a worse outcome than having none.
    private static var buildTimeAPIURL: String? {
        guard
            let raw = Bundle.main.object(forInfoDictionaryKey: "HomerunAPIURL") as? String,
            !raw.isEmpty,
            !raw.hasPrefix("$(")
        else { return nil }
        return raw
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

    /// Create the data directory, at launch.
    ///
    /// There is nothing to install on a phone — the server ships inside the
    /// app — so this is the whole of "setup", and it must be done *before* the
    /// UI asks `is-installed`. Answering false sends the UI down the desktop
    /// installation path, where it invokes `start-native-installation`; that
    /// channel belongs to the `installation` capability, which iOS declares
    /// false, so the invoke is rejected, the awaiting boot code throws, and
    /// the app sits on its splash screen forever.
    @discardableResult
    static func ensureFirstRunSetup() -> Bool {
        do {
            try FileManager.default.createDirectory(
                at: serversDirectory, withIntermediateDirectories: true)
            firstRunComplete = true
            return true
        } catch {
            HostLog.host.error("first-run setup failed: \(error.localizedDescription, privacy: .public)")
            return false
        }
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

    /// A stable per-install identifier, local to this app.
    ///
    /// Not the backend's device id — see `registeredDeviceId`. Kept because it
    /// is a useful stable string for naming and diagnostics.
    static func localInstallID() -> String? {
        if let existing = defaults.string(forKey: "homerun.deviceId") { return existing }
        guard let vendor = UIDevice.current.identifierForVendor?.uuidString else { return nil }
        defaults.set(vendor, forKey: "homerun.deviceId")
        return vendor
    }
}

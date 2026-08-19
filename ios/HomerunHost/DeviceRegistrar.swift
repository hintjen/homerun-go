import Foundation
import UIKit

/// Makes this phone a device the Homerun backend knows about.
///
/// The UI asks the host for a device id and sends it as the `device` field
/// when creating a server. If the backend has never heard of that id, creation
/// fails with `{"device":["Device with id … does not exist"]}` — which the UI
/// renders as a flat "An error occurred while creating the server", because
/// its error handler only reads `name` and `message`. So the id we return has
/// to be one the backend issued, not one we invented.
///
/// Desktop registers during its install step. There is no install on mobile,
/// so registration happens lazily on the first request for the id: that is
/// order-independent, and by then the user has signed in, which is what
/// supplies the token.
@MainActor
final class DeviceRegistrar {
    private var inFlight: Task<String?, Never>?

    /// The backend's device id, registering if this is the first time.
    ///
    /// Returns nil when it genuinely cannot be had — not signed in yet, or the
    /// backend refused. The UI treats a null id as "no device selected" and
    /// blocks server creation with a message of its own, which is a better
    /// outcome than handing it an id that will be rejected later.
    func deviceId() async -> String? {
        if let known = HostStore.registeredDeviceId {
            // A device row belongs to exactly one account, so returning any
            // existing registration regardless of who is signed in is not a
            // shortcut — it is how a phone that started as a guest stays
            // registered to the guest it has already left. Every later request
            // naming that device is then refused as somebody else's hardware:
            // the push token upsert with "Not one of your devices", and the
            // guest-server migration with "That device does not belong to this
            // account".
            let account = HostStore.currentAccount
            let registeredTo = HostStore.registeredDeviceAccount

            if account == nil { return known }

            if registeredTo == nil {
                // Registered before this marker existed. Adopt the current
                // account rather than re-registering: on upgrade that would
                // mint a second device row for every install at once. A wrong
                // guess corrects itself at the next real change of account.
                HostStore.registeredDeviceAccount = account
                return known
            }

            if registeredTo == account { return known }

            HostLog.device.info("signed in as a different account; re-registering this device")
            HostStore.registeredDeviceId = nil
            HostStore.deviceGroupId = nil
            TokenStore.deviceToken = nil
        }

        // The UI asks from more than one screen, and two registrations would
        // create two devices.
        if let inFlight { return await inFlight.value }

        let task = Task<String?, Never> { [weak self] in
            defer { self?.inFlight = nil }
            return await self?.register() ?? nil
        }
        inFlight = task
        return await task.value
    }

    private func register() async -> String? {
        guard let token = TokenStore.accessToken, !token.isEmpty else {
            HostLog.device.info("not registering yet — no credentials")
            return nil
        }
        guard let apiURL = HostStore.apiURL, !apiURL.isEmpty else {
            HostLog.device.info("not registering yet — no API URL")
            return nil
        }

        do {
            let device = try await HomerunAPI.registerDevice(
                apiURL: apiURL,
                token: token,
                deviceName: Self.deviceName(),
                // Pins re-registration to the device we already have, so a
                // reinstall reuses it instead of leaving an orphan behind.
                existingDeviceId: HostStore.registeredDeviceId)

            HostStore.registeredDeviceId = device.id
            HostStore.registeredDeviceAccount = HostStore.currentAccount
            HostStore.deviceGroupId = device.groupId
            TokenStore.deviceToken = device.token

            HostLog.device.info("registered as \(device.id, privacy: .public)")
            return device.id
        } catch {
            HostLog.device.error("registration failed: \(error.localizedDescription, privacy: .public)")
            return nil
        }
    }

    /// A stable, reasonably unique name for this install.
    ///
    /// Desktop uses the machine's hostname. iOS has no equivalent —
    /// `UIDevice.name` returns a generic model string unless the app is
    /// specially entitled — so this is the model plus a slice of the vendor
    /// identifier. Stability matters more than prettiness: the backend dedupes
    /// on the name, and a bare "iphone" would collide with every other install
    /// and get a hex suffix appended instead.
    private static func deviceName() -> String {
        let model = UIDevice.current.model.lowercased().replacingOccurrences(of: " ", with: "-")
        let vendor = UIDevice.current.identifierForVendor?.uuidString.prefix(8).lowercased() ?? "unknown"
        return "\(model)-\(vendor)"
    }
}

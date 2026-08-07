import Foundation

/// Fire-and-forget channels: session, boot, and storage signals.
///
/// A send has no reply, so a failure here is invisible to the UI by design.
/// Anything the user must know about has to come back as an *event*.
extension BridgeRouter {

    /// The UI answers the host's `get-api-url` event with this.
    func setAPIURL(_ params: Any?) async throws -> Any? {
        HostStore.apiURL = params as? String
        return nil
    }

    /// Credentials arrive after a successful login or a magic-link return.
    ///
    /// The host keeps the access token because it has to call the API with no
    /// page in front of it — registering this device is the first such call,
    /// and the UI cannot do it on our behalf.
    ///
    /// Emitting `credentials-set` is the load-bearing part: the boot state
    /// machine waits on that event before routing to the dashboard, so a
    /// handler that stores and stays quiet hangs login on a spinner.
    func credentialsReceived(_ params: Any?) async throws -> Any? {
        guard let credentials = params as? [String: Any],
            let token = credentials["access_token"] as? String, !token.isEmpty
        else {
            events?.emit("credentials-error", ["Sign-in did not complete. Please try again."])
            return nil
        }

        TokenStore.accessToken = token
        if let apiURL = credentials["apiUrl"] as? String, !apiURL.isEmpty {
            HostStore.apiURL = apiURL
        }
        events?.emit("credentials-set", [])

        // Registration needs the token that just arrived, and the UI will ask
        // for the device id as soon as the dashboard mounts. Starting now
        // means it is usually already done by then; `DeviceRegistrar` handles
        // the case where it is not.
        Task { _ = await deviceRegistrar.deviceId() }
        return nil
    }

    func logout(_ params: Any?) async throws -> Any? {
        HostStore.clientNonce = nil
        // The device registration deliberately survives: it belongs to the
        // phone, not the session, and re-registering on the next login would
        // orphan the servers already attached to it.
        TokenStore.accessToken = nil
        return nil
    }

    /// **Boot-critical.** The UI's boot state machine blocks until
    /// `system-check-complete` or `system-check-failed` arrives. Emitting
    /// neither leaves the app on a splash screen forever, so every path out of
    /// here emits exactly one of them.
    func startInstallationOrCheck(_ params: Any?) async throws -> Any? {
        if HostStore.ensureFirstRunSetup() {
            events?.emit("system-check-complete", [])
        } else {
            events?.emit(
                "system-check-failed",
                ["Homerun could not set up storage on this device. Free up some space and reopen the app."]
            )
        }
        return nil
    }

    /// The desktop host enforces a storage cap. On iOS the device's own free
    /// space is the real limit, so this reports against that.
    func checkHomerunStorageLimit(_ params: Any?) async throws -> Any? {
        let free =
            (try? HostStore.documentsDirectory.resourceValues(
                forKeys: [.volumeAvailableCapacityForImportantUsageKey]
            ).volumeAvailableCapacityForImportantUsage) ?? nil

        // Under a gigabyte free, a world save can fail partway through — which
        // is how worlds get corrupted. Warn before that happens.
        let exhausted = (free ?? Int64.max) < 1_073_741_824
        events?.emit(exhausted ? "storage-limit-exceeded" : "storage-limit-ok", [])
        return nil
    }

    /// A loopback: the host receives the send and echoes it back as an event.
    /// iOS has no per-app storage settings pane to open, so the UI handles it.
    func openStorageSettings(_ params: Any?) async throws -> Any? {
        events?.emit("open-storage-settings", [])
        return nil
    }
}

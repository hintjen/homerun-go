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

    /// Credentials arrive after a successful login or a magic-link return. The
    /// UI owns the session; the host acknowledges so anything listening for
    /// `credentials-set` can react.
    func credentialsReceived(_ params: Any?) async throws -> Any? {
        guard params is [String: Any] else {
            events?.emit("credentials-error", ["Sign-in did not complete. Please try again."])
            return nil
        }
        events?.emit("credentials-set", [])
        return nil
    }

    func logout(_ params: Any?) async throws -> Any? {
        HostStore.clientNonce = nil
        return nil
    }

    /// **Boot-critical.** The UI's boot state machine blocks until
    /// `system-check-complete` or `system-check-failed` arrives. Emitting
    /// neither leaves the app on a splash screen forever, so every path out of
    /// here emits exactly one of them.
    func startInstallationOrCheck(_ params: Any?) async throws -> Any? {
        do {
            try FileManager.default.createDirectory(
                at: HostStore.serversDirectory, withIntermediateDirectories: true)
            HostStore.firstRunComplete = true
            events?.emit("system-check-complete", [])
        } catch {
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

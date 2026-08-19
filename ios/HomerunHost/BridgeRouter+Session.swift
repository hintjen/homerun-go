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
        // Which account this is, so `DeviceRegistrar` can tell a change of
        // token from a change of *account* — a device row belongs to one
        // account, and re-using another's is what the backend refuses.
        HostStore.currentAccount = credentials["matrix_id"] as? String
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
        // orphan the servers already attached to it. Signing back in as a
        // *different* account is the one case where it must not survive, and
        // `DeviceRegistrar` handles that by comparing the account it was
        // registered to — here there is nothing yet to compare against.
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

/// The Minecraft account: who the player is, so their minigame stats can be
/// looked up.
///
/// Three invokes and two events, all `minecraftAccount`-tier and all already in
/// `bridge/v1` — Android has answered them since revision 8 and this is iOS
/// catching up, not a protocol change.
///
/// The two events are not optional and are the detail most likely to be missed:
/// several `useMinecraftAccount` consumers mount at once — the profile banner
/// and the leaderboard's "You" row are both on `/games` — and each keeps its own
/// state from them. A login that only answered its caller would leave the other
/// consumers signed out until reload.
extension BridgeRouter {

    /// Refreshes silently when the token has aged out. Null covers "not signed
    /// in" and "the session could not be recovered" alike, which is what the UI
    /// does with both anyway.
    func minecraftAuthGetProfile(_ params: Any?) async throws -> Any? {
        guard let session = await MinecraftAuth.shared.profile() else { return NSNull() }
        return (try? Core.accountRedacted(session)) ?? NSNull()
    }

    func minecraftAuthLogin(_ params: Any?) async throws -> Any? {
        do {
            let session = try await MinecraftAuth.shared.signIn()
            let credentials = try Core.accountRedacted(session)
            events?.emit("minecraft:auth:ready", [credentials])
            return ["success": true, "credentials": credentials]
        } catch is CancellationError {
            throw CancellationError()
        } catch {
            // Written for a player: `MinecraftAuth.AuthError` messages already
            // are, and `Core.CoreError` carries the core's own wording. Anything
            // else would be a Foundation description, so it does not go through.
            HostLog.bridge.warning(
                "Minecraft sign-in failed: \(error.localizedDescription, privacy: .public)")
            let message: String
            switch error {
            case let authError as MinecraftAuth.AuthError:
                message = authError.message
            case let coreError as Core.CoreError:
                message = coreError.message
            default:
                message = "Could not sign in to Microsoft."
            }
            return ["success": false, "error": message]
        }
    }

    func minecraftAuthLogout(_ params: Any?) async throws -> Any? {
        await MinecraftAuth.shared.signOut()
        events?.emit("minecraft:auth:signed-out", [])
        return ["success": true]
    }
}

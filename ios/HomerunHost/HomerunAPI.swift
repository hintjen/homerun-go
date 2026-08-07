import Foundation

/// The bits of the Homerun backend this host has to talk to itself.
///
/// The UI owns almost all backend traffic. This exists for the one thing it
/// cannot do: registering *this device*. The UI only ever reads `/api/device/`
/// — creating one is the host's job on every platform, because the id has to
/// outlive any page and be there before the first server is created.
///
/// Deliberately small, and mirrors `HomerunApi.kt` on Android.
enum HomerunAPI {

    /// What registration hands back. All three are persisted.
    struct Device {
        let id: String
        let groupId: String?
        /// Bearer token for device-scoped reporting endpoints — *not* the
        /// user's JWT, and not interchangeable with it.
        let token: String?
    }

    enum APIError: LocalizedError {
        case notAuthenticated
        case http(Int, String)
        case malformed

        var errorDescription: String? {
            switch self {
            case .notAuthenticated:
                return "Homerun is not signed in on this device."
            case .http(let status, let body):
                return "The Homerun service returned \(status). \(body)"
            case .malformed:
                return "The Homerun service sent a reply Homerun could not read."
            }
        }
    }

    /// Register this device, or re-register an existing one.
    ///
    /// `POST /api/init/native/` — and it must be this endpoint rather than the
    /// more obvious `POST /api/device/`. Creating a bare Device row is not
    /// enough: the server-creation serializer additionally requires that the
    /// device be a member of the user's root database *and* of their Homerun
    /// group, and `Minecraft.create` requires that group to have a Gateway
    /// service. This one call does all of it. A device made the other way is
    /// rejected later with "is not owned by the current user", which is a
    /// confusing way to learn this.
    ///
    /// Idempotent: the same `deviceName` returns the same device. Passing
    /// `existingDeviceId` pins it, which is what stops a reinstall from
    /// silently accumulating devices — the backend appends a hex suffix on
    /// name collision rather than failing.
    static func registerDevice(
        apiURL: String,
        token: String,
        deviceName: String,
        existingDeviceId: String? = nil
    ) async throws -> Device {
        guard !token.isEmpty else { throw APIError.notAuthenticated }

        var body: [String: Any] = ["device_name": deviceName]
        if let existingDeviceId { body["existing_device_id"] = existingDeviceId }

        let object = try await post(apiURL: apiURL, path: "/api/init/native/", body: body, token: token)

        guard let id = object["device_id"] as? String else { throw APIError.malformed }
        return Device(
            id: id,
            groupId: object["group_id"] as? String,
            token: object["device_token"] as? String)
    }

    // MARK: - Tunnel

    /// The tunnel credentials as they stood *before* launch, if any.
    ///
    /// This is the baseline the post-launch poll compares against. The legacy
    /// provisioner mints a fresh keypair per session and nulls the config on
    /// stop, so a config still equal to this one is the dead previous set.
    static func tunnelBaseline(apiURL: String, serverId: String, token: String) async -> WireProxy.Link? {
        guard let body = try? await get(apiURL: apiURL, path: "/api/server/\(serverId)/", token: token)
        else { return nil }
        return linkOf(body)?.link
    }

    /// Wait for the gateway to hand this server a tunnel.
    ///
    /// The API provisions the WireGuard peer asynchronously once the server is
    /// marked running, so at launch the credentials usually do not exist yet.
    /// Same cadence as desktop and Android: 20 attempts, 3 s apart, waiting
    /// *before* each poll.
    ///
    /// Null means no tunnel — no token, no signal, or the gateway never
    /// provisioned. On mobile that is fatal, because there is no
    /// port-forwarding fallback; the caller decides.
    static func awaitTunnel(
        apiURL: String,
        serverId: String,
        token: String,
        stale: WireProxy.Link?,
        attempts: Int = 20,
        interval: TimeInterval = 3
    ) async -> WireProxy.Link? {
        guard !token.isEmpty else { return nil }

        for attempt in 1...attempts {
            try? await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))

            guard
                let body = try? await get(
                    apiURL: apiURL, path: "/api/server/\(serverId)/", token: token),
                let polled = linkOf(body)
            else { continue }

            // Gateway v2 reuses credentials deliberately, so for those this
            // check is skipped — without the exception a v2 link would poll
            // until timeout on every single start.
            if !polled.isGateway2, let stale, polled.link == stale {
                NSLog("[tunnel] poll %d: still the pre-launch config, waiting", attempt)
                continue
            }

            NSLog("[tunnel] ready after %d attempt(s)", attempt)
            return polled.link
        }

        NSLog("[tunnel] no tunnel after %d attempts", attempts)
        return nil
    }

    /// A link plus the one field that changes how staleness is judged.
    private struct PolledLink {
        let link: WireProxy.Link
        let isGateway2: Bool
    }

    /// Pull the tunnel out of a `/api/server/<id>/` body.
    ///
    /// The API surfaces gateway-v2's `gateway_tunnel_config` under the same
    /// `native_config` key, so this one path covers both provisioners.
    private static func linkOf(_ body: [String: Any]) -> PolledLink? {
        guard let config = body["config"] as? [String: Any],
            let links = config["links"] as? [[String: Any]],
            let first = links.first,
            let native = first["native_config"] as? [String: Any]
        else { return nil }

        func string(_ key: String) -> String? {
            (native[key] as? String).flatMap { $0.isEmpty ? nil : $0 }
        }

        // Without all three there is no tunnel to build, and a half-written
        // config would surface as an unexplained handshake timeout instead.
        guard let privateKey = string("client_privkey"),
            let publicKey = string("gateway_pubkey"),
            let endpoint = string("link_address")
        else { return nil }

        return PolledLink(
            link: WireProxy.Link(
                clientPrivateKey: privateKey,
                gatewayPublicKey: publicKey,
                endpoint: endpoint,
                address: string("address"),
                allowedIps: string("allowed_ips")),
            isGateway2: first["provisioner"] as? String == "gateway2")
    }

    // MARK: - Transport

    private static func get(apiURL: String, path: String, token: String) async throws -> [String: Any] {
        guard let url = URL(string: apiURL.trimmedTrailingSlash + path) else {
            throw APIError.malformed
        }
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.timeoutInterval = 20

        let (data, response) = try await URLSession.shared.data(for: request)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard (200..<300).contains(status) else {
            throw APIError.http(status, String(data: data, encoding: .utf8) ?? "")
        }
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw APIError.malformed
        }
        return object
    }

    private static func post(
        apiURL: String, path: String, body: [String: Any], token: String
    ) async throws -> [String: Any] {
        guard let url = URL(string: apiURL.trimmedTrailingSlash + path) else {
            throw APIError.malformed
        }

        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setRequestValue()
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        // No blanket short timeout: this runs once, on a phone that may be on
        // a slow connection, and failing it strands the user at the first
        // thing they try to do.
        request.timeoutInterval = 30

        let (data, response) = try await URLSession.shared.data(for: request)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0

        guard (200..<300).contains(status) else {
            // The backend's 400s are field-keyed JSON with no `message`, so
            // pass the body through rather than inventing a summary — it is
            // the only thing that says which field it objected to.
            throw APIError.http(status, String(data: data, encoding: .utf8) ?? "")
        }

        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw APIError.malformed
        }
        return object
    }
}

extension URLRequest {
    fileprivate mutating func setRequestValue() {
        setValue("application/json", forHTTPHeaderField: "Content-Type")
        setValue("application/json", forHTTPHeaderField: "Accept")
    }
}

extension String {
    fileprivate var trimmedTrailingSlash: String {
        hasSuffix("/") ? String(dropLast()) : self
    }
}

import Foundation

/// Turning a player's name into the UUID a server keys them by.
///
/// Its own file because it is the only outbound request this host makes that
/// is not to the app's own API: it goes to Mojang, on the player's behalf, and
/// nothing about it shares the API layer's token handling or error semantics.
///
/// An **offline** server needs none of this — its UUIDs are a function of the
/// name and `homerun-core` derives them internally, which is why the host asks
/// `Core.requiredLookups` what to fetch rather than fetching every name it
/// sees. On a phone that difference is real: a launch with no signal costs
/// nothing instead of eight timeouts.
enum MojangDirectory {

    private static let profile = "https://api.mojang.com/users/profiles/minecraft/"
    private static let timeout: TimeInterval = 10

    /// This player's UUID, dashed, or nil.
    ///
    /// **Nil is a normal outcome**, not an error: an unknown name, a rate
    /// limit, a phone with no signal. The caller leaves that entry out rather
    /// than writing an id that can never match a real player — and never fails
    /// a launch over it. A server missing one operator beats no server.
    static func identity(for name: String) async -> String? {
        guard
            let encoded = name.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed),
            let url = URL(string: profile + encoded)
        else { return nil }

        var request = URLRequest(url: url, timeoutInterval: timeout)
        request.setValue("application/json", forHTTPHeaderField: "Accept")

        guard
            let (data, response) = try? await URLSession.shared.data(for: request),
            // 204 and 404 both mean "no such player". Neither is worth a log
            // line, and neither is distinguishable from the caller's side.
            (response as? HTTPURLResponse)?.statusCode == 200,
            let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let undashed = body["id"] as? String
        else { return nil }

        // Mojang answers with 32 hex characters and no dashes; a server keys
        // players by the dashed form. The core owns that conversion so the
        // three hosts cannot disagree about it.
        return try? Core.dashUuid(undashed)
    }

    /// Resolve every name a launch needs, concurrently.
    ///
    /// `names` comes from `Core.requiredLookups`, which returns nothing at all
    /// for an offline server. Names that could not be resolved are simply
    /// absent from the result.
    static func identities(for names: [String]) async -> [String: String] {
        guard !names.isEmpty else { return [:] }

        return await withTaskGroup(of: (String, String?).self) { group in
            for name in names {
                group.addTask { (name, await identity(for: name)) }
            }

            var resolved: [String: String] = [:]
            for await (name, id) in group {
                if let id { resolved[name] = id }
            }
            return resolved
        }
    }
}

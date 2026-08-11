import Foundation

/// The wire form of `homerun_server_start`.
///
/// A file of its own, with no dependency on anything but Foundation, so
/// `ios/coretest` can compile it and check the encoding against the real Rust
/// parser. That is not incidental: these key names are strings resolved at run
/// time, and `game_type` where the wire says `gameType` compiles, links, and
/// starts a server on the engine's defaults with nothing anywhere saying so.
///
/// Nothing here interprets a setting. Rust decides what each key means, which
/// is what keeps iOS and Android from drifting on it.
enum StartRequest {

    /// The settings a launch carries.
    struct Settings {
        /// The API's `environment_variables`, verbatim.
        ///
        /// `[String: Any]` rather than `[String: String]` because the API is
        /// not consistent about it — `MAX_PLAYERS` arrives as a number from
        /// some panels and a string from others, and the core reads both.
        /// Narrowing here would silently drop the numeric ones.
        let env: [String: Any]

        /// The API's `game_type`, **verbatim** — `native-crossplay` is what
        /// forces offline mode, and a value reduced to java/bedrock cannot
        /// say so.
        let gameType: String

        /// Name → UUID for the players named in the env. A name missing here
        /// is one the lookup could not answer, which Rust then derives offline
        /// or drops; it is never a reason to fail a launch.
        let resolved: [String: String]
    }

    /// Build the request. `settings` may be nil, which starts the server on
    /// the engine's own configuration.
    static func encode(
        serverId: String, dataDir: String, port: UInt16, settings: Settings?
    ) -> [String: Any] {
        var request: [String: Any] = [
            "serverId": serverId,
            "dataDir": dataDir,
            "port": Int(port),
        ]
        if let settings {
            request["settings"] = [
                "env": settings.env,
                "gameType": settings.gameType,
                "resolved": settings.resolved.map { ["name": $0.key, "id": $0.value] },
            ]
        }
        return request
    }
}

import Foundation

/// Envelope encoding and decoding for `bridge/v1` (PROTOCOL.md §2).
///
/// Everything here uses `JSONSerialization` rather than `Codable`. `params`
/// and `result` are arbitrary JSON by design — modelling them with `Codable`
/// means an enum wrapper and a boxing round-trip at every hop, to describe
/// values the host mostly passes through untouched.
enum BridgeEnvelope {
    static let version = 1
    /// Protocol-level, and deliberately absent from `channels.ts` — so it is
    /// handled by the transport and never appears in the router's table.
    static let readyMethod = "__bridge:ready"

    /// A decoded UI -> host message. `id` present means invoke (a reply is
    /// mandatory); absent means send (fire-and-forget).
    struct Incoming {
        let v: Int
        let id: String?
        let method: String
        let params: Any?
    }

    /// WKWebView structured-clones the posted object, so the body normally
    /// arrives as a dictionary. A string body is accepted too — Android posts
    /// JSON text, and matching it here keeps one shape in mind across hosts.
    static func decode(body: Any) -> Incoming? {
        let object: [String: Any]?
        if let dict = body as? [String: Any] {
            object = dict
        } else if let text = body as? String, let data = text.data(using: .utf8) {
            object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        } else {
            object = nil
        }

        guard let object, let method = object["method"] as? String else { return nil }

        // A JS number arrives as NSNumber; an id that is not a string is the
        // UI's business, so echo whatever it sent back verbatim.
        let id: String?
        switch object["id"] {
        case let value as String: id = value
        case let value as NSNumber: id = value.stringValue
        default: id = nil
        }

        return Incoming(
            v: (object["v"] as? NSNumber)?.intValue ?? 0,
            id: id,
            method: method,
            params: unwrapNull(object["params"]))
    }

    static func success(id: String, result: Any?) -> [String: Any] {
        ["v": version, "id": id, "result": result ?? NSNull()]
    }

    static func failure(id: String, message: String, code: String?) -> [String: Any] {
        var error: [String: Any] = ["message": message]
        if let code { error["code"] = code }
        return ["v": version, "id": id, "error": error]
    }

    /// `args` is always an array, its length and order matching the event's
    /// tuple — even for a single payload (PROTOCOL.md §2).
    static func event(name: String, args: [Any]) -> [String: Any] {
        ["v": version, "event": name, "args": args.map { unwrapNull($0) ?? NSNull() }]
    }

    // MARK: - Serialisation

    /// Renders an object as a single JSON literal safe to paste into a JS
    /// expression.
    ///
    /// U+2028 and U+2029 are the trap: both are legal inside a JSON string and
    /// neither is legal inside a JavaScript source line, so a chat message or
    /// a world name containing one turns the injected call into a syntax error
    /// — and the reply simply never arrives.
    static func jsLiteral(object: Any) -> String {
        guard JSONSerialization.isValidJSONObject(object),
            let data = try? JSONSerialization.data(withJSONObject: object),
            let json = String(data: data, encoding: .utf8)
        else {
            return "{\"v\":\(version),\"error\":{\"message\":\"The app could not encode a reply.\"}}"
        }
        return
            json
            .replacingOccurrences(of: "\u{2028}", with: "\\u2028")
            .replacingOccurrences(of: "\u{2029}", with: "\\u2029")
    }

    /// JSON null crosses into Swift as NSNull; callers want nil.
    private static func unwrapNull(_ value: Any?) -> Any? {
        (value is NSNull) ? nil : value
    }
}

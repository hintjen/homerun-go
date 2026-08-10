import Foundation

/// Holds auth callbacks that arrive before the UI can receive them.
///
/// Email OTP and magic links return through `homerun://`. If the app was not
/// running, iOS delivers that URL during launch — long before the WebView has
/// loaded, let alone subscribed to anything. Dropping it strands the user on a
/// login screen having already clicked the link in their mail app, which looks
/// like the link is broken.
///
/// So a cold-start URL is *stored* and the UI collects it with
/// `deep-link:consume` when it is ready. A URL arriving while the app is
/// running goes out as a `deep-link` event instead, which the bridge's own
/// queue covers if the page is still loading.
@MainActor
final class DeepLinkManager {
    /// Set by the app delegate once the bridge exists.
    var emit: ((String) -> Void)?

    private var pending: String?

    func handle(url: URL) {
        let value = url.absoluteString
        if let emit {
            emit(value)
        } else {
            pending = value
        }
    }

    /// Returns the stored URL exactly once — a second call gets nil, so a
    /// reload does not replay a login the user already completed.
    func consume() -> String? {
        defer { pending = nil }
        return pending
    }
}

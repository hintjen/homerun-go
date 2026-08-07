import WebKit

/// Breaks the retain cycle every WKWebView message handler otherwise creates.
///
/// `WKUserContentController` retains its handlers strongly, the controller is
/// owned by the configuration, the configuration by the web view — and the
/// handler owns the web view. Registering `self` directly leaks the entire
/// WebView and its content process for the life of the app, which on a device
/// already running a Minecraft server is memory nobody can spare.
final class WeakScriptMessageHandler: NSObject, WKScriptMessageHandler {
    private weak var delegate: WKScriptMessageHandler?

    init(_ delegate: WKScriptMessageHandler) {
        self.delegate = delegate
    }

    func userContentController(
        _ controller: WKUserContentController, didReceive message: WKScriptMessage
    ) {
        delegate?.userContentController(controller, didReceive: message)
    }
}

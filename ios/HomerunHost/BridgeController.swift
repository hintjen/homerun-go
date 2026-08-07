import UIKit
import WebKit

/// The `bridge/v1` transport: owns the WebView, carries invokes to the router,
/// and delivers replies and events back to the page (PROTOCOL.md §3).
///
/// Everything here runs on the main thread. `WKScriptMessageHandler` callbacks
/// already arrive there, and every WebKit API this touches requires it, so the
/// class is `@MainActor` and handlers hop off it themselves if they need to.
@MainActor
final class BridgeController: NSObject, BridgeEventSink {
    let webView: WKWebView

    private let router: BridgeRouter

    /// Events emitted before the page is listening are lost, so they queue
    /// until it announces itself and then flush in order (PROTOCOL.md §4.2).
    private enum Delivery {
        case queuing([[String: Any]])
        case live
    }
    private var delivery: Delivery = .queuing([])

    /// In-flight invokes for the *current* page, keyed by the id the UI chose.
    private var pending: [String: Task<Void, Never>] = [:]

    /// Bumped whenever the page goes away. A reply carrying a stale generation
    /// is dropped rather than delivered: the new page never made that call,
    /// and its ids start again from 1, so delivering it would resolve an
    /// unrelated promise with someone else's data.
    private var generation = 0

    init(deepLinks: DeepLinkManager, backend: PumpkinBackend) {
        router = BridgeRouter(deepLinks: deepLinks, backend: backend)

        let config = WKWebViewConfiguration()
        // Both must be set before the WebView exists. The configuration retains
        // the scheme handler, so the host does not hold one itself — and it
        // could not anyway: `self` is off limits until after super.init().
        config.setURLSchemeHandler(AppSchemeHandler(), forURLScheme: AppSchemeHandler.scheme)
        config.userContentController.addUserScript(Capabilities.userScript())
        config.userContentController.addUserScript(Self.errorHookScript())

        webView = WKWebView(frame: .zero, configuration: config)
        super.init()

        config.userContentController.add(WeakScriptMessageHandler(self), name: "homerun")
        webView.navigationDelegate = self
        webView.uiDelegate = self
        router.events = self

        // A deep link that arrives while the app is running goes straight out
        // as an event; the queue covers the case where the page is still
        // loading. Cold-start URLs take the `deep-link:consume` path instead.
        deepLinks.emit = { [weak self] url in
            self?.emit("deep-link", [url])
        }

        // The contract's state event carries only these three; `starting` and
        // `stopping` are host-internal and the UI infers them from its own
        // pending call.
        backend.onStateChanged = { [weak self] serverId, state in
            guard let wire = ["running", "stopped", "crashed"].first(where: { $0 == state.rawValue })
            else { return }
            self?.emit("native-server-state-changed", [["serverId": serverId, "state": wire]])
        }

        backend.onLog = { [weak self] serverId, line in
            self?.emit("native-server-log", [["serverId": serverId, "line": line]])
        }

        backend.onPlayersChanged = { [weak self] serverId in
            self?.emit("native-server-players-changed", [["serverId": serverId]])
        }

        // This is an app, not a document: the CSS owns the safe areas and the
        // scrolling, so the native layer must not add rubber-banding or insets
        // on top of it.
        webView.scrollView.bounces = false
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        webView.isOpaque = false
        webView.backgroundColor = .white

        #if DEBUG
            if #available(iOS 16.4, *) {
                webView.isInspectable = true  // Safari -> Develop -> Simulator
            }
        #endif

        load()
    }

    // No deinit unregistering the message handler: `WeakScriptMessageHandler`
    // already keeps the controller from being retained, so a dead controller
    // leaves a proxy that forwards to nil rather than a leak. Touching
    // main-actor WebKit state from `deinit` — which is nonisolated — is also
    // a hard error under Swift 6.

    func load() {
        webView.load(URLRequest(url: AppSchemeHandler.indexURL))
    }

    // MARK: - Host -> UI

    /// Emit an event. `args` is positional and must match the event's tuple.
    func emit(_ event: String, _ args: [Any]) {
        let envelope = BridgeEnvelope.event(name: event, args: args)
        switch delivery {
        case .queuing(var queued):
            queued.append(envelope)
            delivery = .queuing(queued)
        case .live:
            deliver(envelope)
        }
    }

    private func deliver(_ envelope: [String: Any]) {
        let literal = BridgeEnvelope.jsLiteral(object: envelope)
        // Guarded on the JS side: a reply can land between a reload starting
        // and the page's own transport being installed.
        webView.evaluateJavaScript(
            "window.__homerunHost && window.__homerunHost.receive(\(literal));",
            completionHandler: nil)
    }

    // MARK: - Page lifecycle

    /// Drops every trace of the page that just went away. The host keeps no
    /// other per-page state, which is what lets a reload fully resynchronise:
    /// the fresh page re-invokes for everything it needs.
    private func resetForNewPage() {
        generation += 1
        pending.values.forEach { $0.cancel() }
        pending.removeAll()
        delivery = .queuing([])
    }
}

// MARK: - UI -> host

extension BridgeController: WKScriptMessageHandler {
    func userContentController(
        _ controller: WKUserContentController, didReceive message: WKScriptMessage
    ) {
        guard message.name == "homerun" else { return }

        guard let incoming = BridgeEnvelope.decode(body: message.body) else {
            NSLog("[bridge] discarded a message with no method: %@", String(describing: message.body))
            return
        }

        // Never guess at an unknown version — the shapes may differ.
        guard incoming.v == BridgeEnvelope.version else {
            reply(
                to: incoming.id, message: "This version of Homerun cannot talk to the app screen.",
                code: "UNSUPPORTED_VERSION")
            return
        }

        if incoming.method == BridgeEnvelope.readyMethod {
            flushQueue()
            return
        }

        if incoming.method == "__host:jsError" {
            logJSError(incoming.params)
            return
        }

        guard let handler = router.handler(for: incoming.method) else {
            // An unanswered invoke hangs a UI promise forever — the worst
            // failure mode in this protocol, and it looks like a frozen screen
            // with no error. Answer even when there is nothing to say.
            reply(
                to: incoming.id,
                message: "Homerun for iOS cannot do that yet (\(incoming.method)).",
                code: "UNKNOWN_METHOD")
            return
        }

        dispatch(incoming, to: handler)
    }

    private func dispatch(_ incoming: BridgeEnvelope.Incoming, to handler: @escaping BridgeRouter.Handler) {
        let generationAtCall = generation
        let id = incoming.id

        let task = Task { @MainActor [weak self] in
            // No timeout, deliberately: native-server-start and
            // import-minecraft-world legitimately run for minutes. Pending
            // calls are cleared when the page dies, not on a clock.
            do {
                let result = try await handler(incoming.params)
                self?.respond(id: id, generation: generationAtCall, envelope: { id in
                    BridgeEnvelope.success(id: id, result: result)
                })
            } catch {
                let bridgeError = error as? BridgeError
                let message = bridgeError?.message ?? error.localizedDescription
                let code = bridgeError?.code
                self?.respond(id: id, generation: generationAtCall, envelope: { id in
                    BridgeEnvelope.failure(id: id, message: message, code: code)
                })
            }
            if let id { self?.pending.removeValue(forKey: id) }
        }

        // A send has no id and nothing to correlate; only invokes are tracked.
        if let id { pending[id] = task }
    }

    private func respond(
        id: String?, generation: Int, envelope: (String) -> [String: Any]
    ) {
        guard let id else { return }  // a send: nothing to answer
        guard generation == self.generation else { return }  // the page is gone
        deliver(envelope(id))
    }

    private func reply(to id: String?, message: String, code: String) {
        guard let id else {
            NSLog("[bridge] %@", message)
            return
        }
        deliver(BridgeEnvelope.failure(id: id, message: message, code: code))
    }

    private func flushQueue() {
        guard case .queuing(let queued) = delivery else { return }
        delivery = .live
        queued.forEach(deliver)

        // The protocol has no host-to-UI request, so the host asks for the API
        // URL by emitting an event and the UI answers with a `set-api-url`
        // send (PROTOCOL.md §1).
        emit("get-api-url", [])
    }

    private func logJSError(_ params: Any?) {
        let details = params as? [String: Any] ?? [:]
        NSLog(
            "[bridge] uncaught JS error: %@ (%@:%@)",
            details["message"] as? String ?? "?",
            details["source"] as? String ?? "?",
            String(describing: details["line"] ?? "?"))
    }

    /// Uncaught JS errors are otherwise invisible from the native side, and a
    /// bundle that fails to boot looks identical to a bridge that is broken.
    private static func errorHookScript() -> WKUserScript {
        let source = """
            window.addEventListener('error', function (e) {
              window.webkit.messageHandlers.homerun.postMessage({
                v: 1, method: '__host:jsError',
                params: { message: String(e.message), source: String(e.filename), line: e.lineno }
              });
            });
            window.addEventListener('unhandledrejection', function (e) {
              window.webkit.messageHandlers.homerun.postMessage({
                v: 1, method: '__host:jsError',
                params: { message: 'Unhandled rejection: ' + String(e.reason), source: '', line: 0 }
              });
            });
            """
        return WKUserScript(source: source, injectionTime: .atDocumentStart, forMainFrameOnly: true)
    }
}

extension BridgeController: WKNavigationDelegate {
    /// The content process is killed under memory pressure, and on this device
    /// the memory-hungry thing is the server the user is running. Recovery is
    /// part of normal operation, not a defensive extra (PROTOCOL.md §4.3).
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        NSLog("[bridge] WebView content process died; reloading")
        resetForNewPage()
        load()
    }

    func webView(
        _ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!
    ) {
        // A reload from any source — ours, the inspector's, the page's — puts
        // a new page behind the same host, so it needs the same reset.
        resetForNewPage()
    }

    func webView(
        _ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error
    ) {
        NSLog("[bridge] navigation failed: %@", error.localizedDescription)
    }

    func webView(
        _ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: Error
    ) {
        NSLog("[bridge] navigation failed before loading: %@", error.localizedDescription)
    }
}

/// WKWebView drops `alert`, `confirm` and `prompt` on the floor unless the UI
/// delegate implements them — the call just never returns to the page.
extension BridgeController: WKUIDelegate {
    private var presenter: UIViewController? {
        webView.window?.rootViewController
    }

    func webView(
        _ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
        initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void
    ) {
        guard let presenter else { return completionHandler() }
        let alert = UIAlertController(title: nil, message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in completionHandler() })
        presenter.present(alert, animated: true)
    }

    func webView(
        _ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
        initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void
    ) {
        guard let presenter else { return completionHandler(false) }
        let alert = UIAlertController(title: nil, message: message, preferredStyle: .alert)
        alert.addAction(
            UIAlertAction(title: "Cancel", style: .cancel) { _ in completionHandler(false) })
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in completionHandler(true) })
        presenter.present(alert, animated: true)
    }

    func webView(
        _ webView: WKWebView, runJavaScriptTextInputPanelWithPrompt prompt: String,
        defaultText: String?, initiatedByFrame frame: WKFrameInfo,
        completionHandler: @escaping (String?) -> Void
    ) {
        guard let presenter else { return completionHandler(nil) }
        let alert = UIAlertController(title: prompt, message: nil, preferredStyle: .alert)
        alert.addTextField { $0.text = defaultText }
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel) { _ in completionHandler(nil) })
        alert.addAction(
            UIAlertAction(title: "OK", style: .default) { [weak alert] _ in
                completionHandler(alert?.textFields?.first?.text)
            })
        presenter.present(alert, animated: true)
    }
}

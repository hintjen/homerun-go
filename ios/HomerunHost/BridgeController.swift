import UIKit
import WebKit

/// Which of the shared UI's two colour schemes is on screen right now.
///
/// Not necessarily the device's: the UI's theme setting is `system` by default
/// but a player can pin it to light or dark, and then the page and the phone
/// disagree. Only the page knows, which is why the host is told rather than
/// asking `traitCollection`.
enum PageTheme: String {
    case light
    case dark
}

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

    /// The scheme the page has resolved, or nil until it says. Nil means the
    /// splash is still up, so the status bar is being read against brand blue
    /// rather than against either theme.
    private(set) var pageTheme: PageTheme?

    /// Set by the view controller, which owns the status bar. Called on the
    /// main thread whenever `pageTheme` changes — including back to nil when
    /// the page goes away and the blue shows through again.
    var onThemeChanged: ((PageTheme?) -> Void)?

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
        #if DEBUG
            config.userContentController.addUserScript(Self.networkErrorHookScript())
        #endif

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

        // Remote push rides the same queue, and depends on it harder: a
        // cold-start notification tap is delivered by UNUserNotificationCenter
        // during launch, long before the page's handshake, and `push:opened`
        // must survive that gap — the same shape as the cold-start deep link.
        // Token rotations are merely convenient to queue.
        PushMessaging.shared.emit = { [weak self] event, args in
            self?.emit(event, args)
        }

        // The contract's state event carries only these three; `starting` and
        // `stopping` are host-internal and the UI infers them from its own
        // pending call.
        // `backupInProgress` is deliberately dropped: it is an API concern, and
        // the contract's payload is `{serverId, state}`. Inventing a field here
        // would be a channel the UI repo never agreed to.
        //
        // These two also feed `Reporting`, which is why they forward rather
        // than only emitting. The backend offers one closure per event, not a
        // listener list the way Android's `ServerHost` does, and this is the
        // one place that owns them — so a second subscriber joins here. Both
        // sides are main-actor, so the call costs nothing, and reporting is
        // told *after* the page is, because the page is what the player is
        // looking at.
        backend.onStateChanged = { [weak self] serverId, state, _ in
            defer { Reporting.onStateChanged(serverId: serverId, state: state) }
            guard let wire = ["running", "stopped", "crashed"].first(where: { $0 == state.rawValue })
            else { return }
            self?.emit("native-server-state-changed", [["serverId": serverId, "state": wire]])
        }

        backend.onLog = { [weak self] serverId, line in
            self?.emit("native-server-log", [["serverId": serverId, "line": line]])
            Reporting.onLog(serverId: serverId, line: line)
        }

        backend.onPlayersChanged = { [weak self] serverId in
            self?.emit("native-server-players-changed", [["serverId": serverId]])
        }

        // Arrives before the stop it explains, so the UI can say why the
        // server went away rather than showing it as an ordinary shutdown.
        backend.onNetworkError = { [weak self] serverId, kind in
            self?.emit(
                "native-server-network-error", [["serverId": serverId, "kind": kind.rawValue]])
        }

        // This is an app, not a document: the CSS owns the safe areas and the
        // scrolling, so the native layer must not add rubber-banding or insets
        // on top of it.
        webView.scrollView.bounces = false
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        webView.isOpaque = false
        // What shows until the page paints — a cold start, a reload, a content
        // process coming back. The bundle's splash is on this blue, so there is
        // no flash between the two; white here was visible as one, and on a
        // dark-mode device the system background underneath it was black.
        webView.backgroundColor = Brand.launchBackground

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

        // `quit-and-install` — which on this platform installs without
        // quitting. iOS has no relaunch API, `exit(0)` reads to a user as a
        // crash, and quitting would take a running Pumpkin server with it.
        // The router has already promoted the bundle and invalidated the
        // resolved root, so a reload is the whole of applying it.
        router.onApplyUpdate = { [weak self] in
            guard let self else { return }
            HostLog.bundle.info("applying a staged bundle at the page's request")
            self.webView.reload()
        }

        // Offer an update as soon as it is staged rather than waiting for the
        // user to relaunch of their own accord.
        BundleUpdater.onBundleStaged = { [weak self] bundle in
            Task { @MainActor in
                self?.emit("update-available", [[
                    "status": "available",
                    "version": bundle,
                    "bundle": bundle,
                ]])
            }
        }

        // After the page has been asked to load, never before: this is seconds
        // of network and disk for something that takes effect later, so there
        // is nothing to gain by making the user wait on it.
        BundleUpdater.check()
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
        // `pageTheme` is deliberately *not* cleared here. A navigation leaves
        // the old page on screen until the new one paints, so forgetting the
        // theme would flash a white clock over a light page for as long as the
        // load takes. The old page's answer is the best guess for the new one;
        // the watcher corrects it at document start either way.
    }
}

// MARK: - UI -> host

extension BridgeController: WKScriptMessageHandler {
    func userContentController(
        _ controller: WKUserContentController, didReceive message: WKScriptMessage
    ) {
        guard message.name == "homerun" else { return }

        guard let incoming = BridgeEnvelope.decode(body: message.body) else {
            HostLog.bridge.error("discarded a message with no method")
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
            // The handshake is also the health signal for an over-the-air
            // bundle: one that throws on its first chunk never gets here, and
            // one that does has proved it can run. Nothing else in the
            // protocol says that.
            BundleStore.confirm()
            flushQueue()
            return
        }

        if incoming.method == "__host:jsError" {
            logJSError(incoming.params)
            return
        }

        if incoming.method == "__host:netError" {
            let details = incoming.params as? [String: Any] ?? [:]
            let method = details["method"] as? String ?? "?"
            let url = details["url"] as? String ?? "?"
            let status = String(describing: details["status"] ?? "?")
            let body = details["body"] as? String ?? ""
            HostLog.bridge.error(
                "API \(method, privacy: .public) \(url, privacy: .public) -> \(status, privacy: .public) \(body, privacy: .public)"
            )
            return
        }

        #if DEBUG
            // What the UI actually asks for, in order. Boot-path problems are
            // usually a call that never happened rather than one that failed,
            // and that is invisible without this.
            HostLog.bridge.debug("<- \(incoming.method, privacy: .public)\(incoming.id == nil ? " (send)" : "", privacy: .public)")
        #endif

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
            HostLog.bridge.error("\(message, privacy: .public)")
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

    /// `set-appearance`, from the router.
    ///
    /// The page is the only thing that knows: its theme setting defaults to
    /// `system` but a player can pin it, and then the page and the phone
    /// disagree. This host used to infer it from a document-start script that
    /// watched the class next-themes puts on `<html>` — the channel replaced
    /// that, and the script went with it. One answer, from the contract.
    func appearanceChanged(_ theme: PageTheme) {
        setPageTheme(theme)
    }

    private func setPageTheme(_ theme: PageTheme?) {
        guard theme != pageTheme else { return }
        pageTheme = theme
        // The WebView is deliberately not opaque, so this colour is what
        // composites through anywhere the page is not painting: before first
        // paint, during a keyboard pan that reveals past the body box, and in
        // the gap between screens on a view transition. Left on the launch
        // blue it is not a flash on the way into the app — it is a blue edge
        // behind every screen for the life of the process.
        webView.backgroundColor = Brand.backdrop(for: theme)
        onThemeChanged?(theme)
    }

    private func logJSError(_ params: Any?) {
        let details = params as? [String: Any] ?? [:]
        let message = details["message"] as? String ?? "?"
        let source = details["source"] as? String ?? "?"
        let line = String(describing: details["line"] ?? "?")
        HostLog.bridge.error(
            "uncaught JS error: \(message, privacy: .public) (\(source, privacy: .public):\(line, privacy: .public))"
        )
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

    /// Debug only: report failed API calls, with the server's response body.
    ///
    /// The UI turns a rejected request into a short human sentence, which is
    /// right for a player and useless for debugging — "an error occurred while
    /// creating the server" is the same message whatever the API actually
    /// objected to. This surfaces the status and body in the device log.
    ///
    /// Deliberately logs the *response* only. Request headers carry the bearer
    /// token, and this goes to a log that is not private.
    private static func networkErrorHookScript() -> WKUserScript {
        let source = """
            (function () {
              function report(method, url, status, body) {
                try {
                  window.webkit.messageHandlers.homerun.postMessage({
                    v: 1, method: '__host:netError',
                    params: {
                      method: String(method || 'GET'), url: String(url),
                      status: status, body: String(body || '').slice(0, 600)
                    }
                  });
                } catch (e) {}
              }

              var fetch_ = window.fetch;
              window.fetch = function (input, init) {
                var url = (typeof input === 'string') ? input : (input && input.url);
                var method = (init && init.method) || 'GET';
                return fetch_.apply(this, arguments).then(function (res) {
                  if (!res.ok) {
                    res.clone().text().then(function (t) {
                      report(method, url, res.status, t);
                    }).catch(function () {});
                  }
                  return res;
                }, function (err) {
                  // A request that never completes is the failure this hook
                  // used to be blind to: no status, no body, and a UI that
                  // spins for ever with nothing logged anywhere. Report and
                  // rethrow — swallowing it would change what the page does.
                  report(method, url, 0, 'fetch failed: ' + (err && err.message));
                  throw err;
                });
              };

              // axios uses XHR by default, so hooking fetch alone misses it.
              var open_ = XMLHttpRequest.prototype.open;
              var send_ = XMLHttpRequest.prototype.send;
              XMLHttpRequest.prototype.open = function (m, u) {
                this.__m = m; this.__u = u;
                return open_.apply(this, arguments);
              };
              XMLHttpRequest.prototype.send = function () {
                var self = this;
                this.addEventListener('load', function () {
                  if (self.status >= 400) {
                    report(self.__m, self.__u, self.status, self.responseText);
                  }
                });
                // Same blindness as fetch had: an XHR that errors, times out or
                // is aborted never reaches 'load', so nothing was reported.
                ['error', 'timeout', 'abort'].forEach(function (kind) {
                  self.addEventListener(kind, function () {
                    report(self.__m, self.__u, 0, 'xhr ' + kind);
                  });
                });
                return send_.apply(this, arguments);
              };
            })();
            """
        return WKUserScript(source: source, injectionTime: .atDocumentStart, forMainFrameOnly: true)
    }
}

extension BridgeController: WKNavigationDelegate {
    /// The content process is killed under memory pressure, and on this device
    /// the memory-hungry thing is the server the user is running. Recovery is
    /// part of normal operation, not a defensive extra (PROTOCOL.md §4.3).
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        HostLog.bridge.error("WebView content process died; reloading")
        resetForNewPage()
        // This one really does blank the view: what is on screen until the
        // reload paints is the launch blue, which wants a white clock.
        setPageTheme(nil)
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
        HostLog.bridge.error("navigation failed: \(error.localizedDescription, privacy: .public)")
    }

    func webView(
        _ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: Error
    ) {
        HostLog.bridge.error("navigation failed before loading: \(error.localizedDescription, privacy: .public)")
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

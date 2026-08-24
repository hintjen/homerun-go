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

    /// The engine, for one question this class asks it: is this device
    /// hosting? A staged bundle waits until it is not — see
    /// ``applyStagedBundle(_:)``.
    private let backend: PumpkinBackend

    /// The scheme the page has resolved, or nil until it says. Nil means the
    /// splash is still up, so the status bar is being read against brand blue
    /// rather than against either theme.
    private(set) var pageTheme: PageTheme?

    /// Set by the view controller, which owns the status bar. Called on the
    /// main thread whenever `pageTheme` changes — including back to nil when
    /// the page goes away and the blue shows through again.
    var onThemeChanged: ((PageTheme?) -> Void)?

    /// When the process started, in epoch milliseconds, for `host:page_ready`.
    ///
    /// Set by `AppDelegate` at the top of `didFinishLaunching`. The default is
    /// a fallback for any path that reaches a controller without going through
    /// there, and would read as a suspiciously fast launch rather than as a
    /// crash, which is the right way round for a diagnostic.
    static var launchedAtMs: Double = Date().timeIntervalSince1970 * 1000

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

    /// Held only to answer "was this device hosting" when the content process
    /// dies. The `on*` closures below are set on it at init and need no
    /// reference afterwards; this one is read long after, from the navigation
    /// delegate. No cycle: every closure it holds captures `self` weakly, and
    /// `AppDelegate` owns the backend for the life of the process either way.
    private let backend: PumpkinBackend

    init(deepLinks: DeepLinkManager, backend: PumpkinBackend) {
        self.backend = backend
        router = BridgeRouter(deepLinks: deepLinks, backend: backend)
        self.backend = backend

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
            defer {
                Reporting.onStateChanged(serverId: serverId, state: state)
                // A run ending is the moment this device stops being busy,
                // which is what can hold a staged bundle back. Last, so the
                // page that may be about to be replaced still gets its final
                // state and the report still goes out.
                if state == .stopped || state == .crashed {
                    self?.applyStagedBundle("the server reached \(state.rawValue)")
                }
            }
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

        // Apply an update as soon as it is staged rather than offering it.
        // There is no prompt on this host: `update-available` is what the
        // shared UI's update card subscribes to, and never emitting it is what
        // keeps the card off the screen.
        BundleUpdater.onBundleStaged = { [weak self] bundle in
            Task { @MainActor in
                HostLog.bundle.info("bundle \(bundle, privacy: .public) is staged")
                self?.applyStagedBundle("it was just staged")
            }
        }

        // After the page has been asked to load, never before: this is seconds
        // of network and disk for something that takes effect later, so there
        // is nothing to gain by making the user wait on it.
        BundleUpdater.check()
    }

    // MARK: - Over-the-air updates

    /// Put a downloaded bundle on screen, if this is a safe moment to.
    ///
    /// There is no update prompt on this host. A bundle that has been fetched,
    /// verified and unpacked goes live as soon as the app can take it, which
    /// is usually within a second of it arriving — the page reloads and comes
    /// back on the new UI. The alternative was a card asking permission for
    /// something that costs a second and that nobody can evaluate, and
    /// "later" meant the next launch anyway.
    ///
    /// **Two things make now the wrong moment**, and both defer rather than
    /// cancel:
    ///
    ///  - **A bridge call is in flight.** A reload clears `pending`, so the
    ///    call's promise never resolves. `wait-for-update-check` is itself one
    ///    of them — it is awaited on the mandatory post-login path, so
    ///    applying underneath it would hang login at a spinner, which is the
    ///    exact failure this protocol is most careful about.
    ///  - **This device is hosting.** A running server survives the swap — it
    ///    lives in the backend, not the page — but the console scrollback does
    ///    not, and interrupting someone mid-session to reload the UI is a poor
    ///    trade for a fix that can wait for the stop.
    ///
    /// The on-stop backup deliberately does *not* hold it back, where Android's
    /// `busy` does: it runs in a detached task on the backend, owns no page
    /// state, and a reload cannot touch it.
    ///
    /// Every path back to idle calls this again — the last reply of a page,
    /// the handshake of a fresh one, a server reaching a final state — and if
    /// none of them ever does, `BundleStore.activate()` in `AppDelegate` still
    /// takes it at the next launch. That path is untouched and remains the
    /// floor.
    func applyStagedBundle(_ trigger: String) {
        guard let staged = BundleStore.pending() else { return }

        guard case .live = delivery else {
            HostLog.bundle.info(
                "holding \(staged, privacy: .public) back (\(trigger, privacy: .public)): no page is listening yet")
            return
        }
        guard pending.isEmpty else {
            HostLog.bundle.info(
                "holding \(staged, privacy: .public) back (\(trigger, privacy: .public)): the page is mid-call")
            return
        }
        guard backend.lifecycle.activeIds().isEmpty else {
            HostLog.bundle.info(
                "holding \(staged, privacy: .public) back (\(trigger, privacy: .public)): this device is hosting")
            return
        }

        HostLog.bundle.info(
            "applying \(staged, privacy: .public) now (\(trigger, privacy: .public))")
        BundleStore.activate()
        // Forget the resolved root before reloading, or the page comes back on
        // the bundle it was already showing and nothing appears to happen.
        AppSchemeHandler.invalidateRoot()
        webView.reload()
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

    /// Send one analytics event, through the page.
    ///
    /// This host carries no PostHog SDK and deliberately should not: the shared
    /// UI already holds the user's identity, and `pages/_app.tsx` forwards this
    /// channel straight to `posthog.capture`. That is the desktop's pattern
    /// exactly — `captureRendererEvent` in homerun/homerun-ui
    /// `src/electron/main.ts` sends the same channel name over IPC — and it
    /// costs no ledger entry and no revision bump, because
    /// `posthog-capture-event` is already a `core` event in `bridge-v1.json`
    /// that no host has ever emitted.
    ///
    /// Only for what the page cannot observe about itself. Anything the UI
    /// already subscribes to is named in the UI, where it merges with identity
    /// naturally, which is why the list of call sites here stays short.
    ///
    /// Queued before the handshake like any other event. The exception is
    /// anything emitted while a page is *dying*: `resetForNewPage` throws that
    /// queue away, which is why incidents go to `UserDefaults` instead. See
    /// `Incidents`.
    func capture(_ event: String, _ properties: [String: Any] = [:]) {
        // Spelled out rather than a ternary: the two branches are `[String]`
        // and `[Any]`, and letting the compiler reconcile them is the kind of
        // inference that breaks on a compiler upgrade.
        var args: [Any] = [event]
        if !properties.isEmpty { args.append(properties) }
        emit("posthog-capture-event", args)
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
            // A bundle held back while the last page was busy takes the first
            // chance it gets. A no-op unless one is waiting.
            applyStagedBundle("the page announced itself")
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
            // The other half of applying immediately: a bundle that arrived
            // while this call was in flight goes live now that it is not.
            if self?.pending.isEmpty == true {
                self?.applyStagedBundle("the page went idle")
            }
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

        /*
          How long the app took to become usable.

          There is deliberately no `host:launched` to pair with it: every event
          emitted before the handshake is only *delivered* at the handshake, so
          a launch that never gets this far could never report itself, and a
          launch event would be this one with worse timing. A boot that fails
          that badly is `BundleStore`'s probation to catch, not analytics'.
        */
        capture(
            "host:page_ready",
            ["since_launch_ms": Int(Date().timeIntervalSince1970 * 1000 - Self.launchedAtMs)])

        // After the flush, so it lands in the order a reader expects and never
        // ahead of the page-ready event that dates it.
        Incidents.drain { event, properties in capture(event, properties) }
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

    /// An uncaught error from the page, to the device log *and* to the API.
    ///
    /// This hook is injected at document start, which is what makes it worth
    /// keeping now that the shared UI reports its own errors: it is live
    /// before the bundle boots, so it catches the one failure a React error
    /// boundary can never see — the bundle that throws on its way up and
    /// leaves a blank screen with no page left to report from.
    ///
    /// Location rather than a stack: `window.onerror` gives a file and a line
    /// and nothing else here. The core groups on what it is given.
    private func logJSError(_ params: Any?) {
        let details = params as? [String: Any] ?? [:]
        let message = details["message"] as? String ?? "?"
        let source = details["source"] as? String ?? "?"
        let line = String(describing: details["line"] ?? "?")
        HostLog.bridge.error(
            "uncaught JS error: \(message, privacy: .public) (\(source, privacy: .public):\(line, privacy: .public))"
        )

        AppErrors.report(
            source: AppErrors.sourceUI,
            severity: AppErrors.severityFatal,
            kind: "boot",
            message: message,
            location: source.isEmpty ? nil : "\(source):\(line)")
    }

    /// Uncaught JS errors are otherwise invisible from the native side, and a
    /// bundle that fails to boot looks identical to a bridge that is broken.
    private static func errorHookScript() -> WKUserScript {
        let source = """
            function preBootError(message, source, line) {
              if (window.__homerunPageErrors) return;
              try {
                window.webkit.messageHandlers.homerun.postMessage({
                  v: 1, method: '__host:jsError',
                  params: { message: String(message), source: String(source), line: line }
                });
              } catch (e) {}
            }
            window.addEventListener('error', function (e) {
              preBootError(e.message, e.filename, e.lineno);
            });
            window.addEventListener('unhandledrejection', function (e) {
              preBootError('Unhandled rejection: ' + String(e.reason), '', 0);
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
        // To disk, not to the page: the page is what just died, and
        // `resetForNewPage` below is about to throw away the queue anything
        // emitted here would sit in. The replacement page reports it at its
        // handshake, seconds from now.
        //
        // No `did_crash` counterpart to Android's: WebKit does not say whether
        // this was a crash or a jetsam, and on this device the answer is
        // almost always the server we are running.
        Incidents.record(
            Incidents.contentProcessDeath, hosting: !backend.runningServerIds.isEmpty)
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

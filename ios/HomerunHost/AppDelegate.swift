import FirebaseMessaging
import UIKit

/// UIKit, window-based, no scenes and no SwiftUI.
///
/// The app is one WKWebView for its whole life, and that view's configuration
/// (scheme handler, user scripts, message handler) has to be fixed before the
/// view exists. A SwiftUI `UIViewRepresentable` would put a diffing lifecycle
/// in front of a view that never needs to change, for no benefit. The
/// lifecycle hooks the later milestones need — deep links, save-on-suspend,
/// the idle timer — also live here.
@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    var window: UIWindow?

    /// Owns the bridge for the life of the process. Deliberately not recreated
    /// on WebView death: recovery reloads the page, and the host keeps no
    /// per-page state to rebuild.
    private var bridge: BridgeController?

    /// Created before the bridge so a URL delivered during launch is captured
    /// rather than dropped.
    private let deepLinks = DeepLinkManager()

    /// One backend for the life of the process. It owns the server thread, so
    /// recreating it would orphan a running world.
    private let backend = PumpkinBackend()

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        #if DEBUG
            // A mismatch means the staged .a is not the one this source was
            // written against, which otherwise shows up as garbage decoded out
            // of an FFI reply much later.
            HostLog.host.info("FFI ABI version \(homerun_abi_version(), privacy: .public)")
        #endif

        // First, so it covers the rest of launch. It points the native core's
        // crash directory at storage this app owns and takes over the uncaught
        // exception handler — the window both protect starts here.
        AppErrors.start()

        // Before the WebView exists, because the UI asks `is-installed` on its
        // post-login path and a false answer strands it on the splash screen.
        HostStore.ensureFirstRunSetup()

        // Remote push. Before the bridge, so a cold-start notification tap —
        // which UNUserNotificationCenter delivers during launch — reaches a
        // delegate that already exists; the controller's pre-ready queue holds
        // the resulting `push:opened` until the page can hear it. Quietly does
        // nothing on a build without GoogleService-Info.plist.
        PushMessaging.shared.configureIfPossible()
        if PushMessaging.shared.configured {
            // Registration is separate from the *permission*: APNs hands out
            // device tokens regardless, and FCM needs one to mint its own.
            // The permission only governs whether anything is displayed, and
            // the UI asks for it over the bridge at a moment the user
            // understands.
            application.registerForRemoteNotifications()
        }

        // A backup report that never reached the API leaves the backup lease
        // open, and the lease has no timeout — every other device stays locked
        // out of that world until this one speaks again. This is that. It
        // needs only the API URL and the device token, both of which are
        // readable here, and it is a no-op when there is nothing pending.
        Task { await BackupManager.flushPendingReports() }

        // A cold-start auth callback arrives here, long before the WebView can
        // receive anything; DeepLinkManager holds it for `deep-link:consume`.
        if let url = launchOptions?[.url] as? URL {
            deepLinks.handle(url: url)
        }

        // Before the bridge, which is what will start feeding it console lines
        // and state changes. The backend outlives every page and so does this.
        Reporting.attach(backend: backend)

        // Both halves of the crate's logging, registered before anything can
        // fail. The sink is where its own diagnostics land — without it every
        // device-websocket failure on iOS is silent, since printing would
        // write into the pipe that feeds the player-visible console. The
        // provider is the other direction: what `get-app-logs` answers a
        // support request with. See `DeviceWebsocket.swift`.
        registerNativeLogSink()
        registerAppLogsProvider()

        // Before anything can load a page. A bundle downloaded on an earlier
        // launch goes live here and in `quit-and-install`, nowhere else — never
        // under a live WebView, which would cancel whatever bridge call is in
        // flight, and `native-server-start` runs for minutes.
        //
        // Without this an over-the-air bundle only ever activates if the user
        // taps Install Now, so the whole mechanism looks like it works — the
        // fetch is narrated, the bundle stages — and silently never goes live.
        BundleStore.activate()

        // Late, because it needs a credential and an API URL. What it sends
        // was written during the *previous* launch, by a process that did not
        // survive to report it itself.
        AppErrors.drain()

        // Beside the drain and for the same reason: both answer "what happened
        // to the process before this one". The difference is who noticed — the
        // drain sends what the dying process managed to write down, and this
        // subscribes to what the system recorded when it could not write
        // anything at all. MetricKit calls back on its own queue, so
        // subscribing here costs launch nothing.
        ExitDiagnostics.shared.start()

        let bridge = BridgeController(deepLinks: deepLinks, backend: backend)
        self.bridge = bridge

        let window = UIWindow(frame: UIScreen.main.bounds)
        window.rootViewController = MainViewController(bridge: bridge)
        window.makeKeyAndVisible()
        self.window = window

        return true
    }

    /// The device websocket lives exactly as long as the foreground does.
    ///
    /// iOS suspends the process, and a suspended process serves nothing, so the
    /// link follows the app rather than being left to rot across a suspension —
    /// which would have the gateway holding a peer slot for a device that
    /// cannot answer. `plans/ios-background-execution.md` is why that limit is
    /// the platform's and not a backlog item.
    ///
    /// Idempotent: `ensure` returns immediately when a link is already up, so
    /// firing on every activation costs nothing.
    ///
    /// > Verifying this needs **Simulator.app open**. A device booted headless
    /// > with `simctl boot` never activates an app, so neither this method nor
    /// > `didBecomeActiveNotification` fires and the link silently never comes
    /// > up — which reads exactly like a hook that was never wired.
    func applicationDidBecomeActive(_ application: UIApplication) {
        DeviceWebsocket.shared.ensure()
    }

    /// Not `willResignActive`, which also fires for a notification banner, the
    /// app switcher and a phone call — none of which suspend anything. Tearing
    /// the link down for those would renegotiate it several times a minute.
    func applicationDidEnterBackground(_ application: UIApplication) {
        DeviceWebsocket.shared.stop()
    }

    /// Auth returns through `homerun://` while the app is already running.
    func application(
        _ app: UIApplication, open url: URL,
        options: [UIApplication.OpenURLOptionsKey: Any] = [:]
    ) -> Bool {
        deepLinks.handle(url: url)
        return true
    }

    /// APNs granted a device token; FCM swaps it for the registration token
    /// the bridge deals in (`MessagingDelegate` on `PushMessaging` fires when
    /// that arrives).
    func application(
        _ application: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        Messaging.messaging().apnsToken = deviceToken
    }

    /// Expected on the simulator (no APNs) and in aeroplane mode. The token
    /// stays null, which the contract calls a state — the UI shows nothing
    /// and asks again next launch.
    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        HostLog.host.info("push: APNs registration failed: \(error.localizedDescription, privacy: .public)")
    }
}

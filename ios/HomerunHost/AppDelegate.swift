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

        // Before anything can load a page. A bundle downloaded on an earlier
        // launch goes live here and in `quit-and-install`, nowhere else — never
        // under a live WebView, which would cancel whatever bridge call is in
        // flight, and `native-server-start` runs for minutes.
        //
        // Without this an over-the-air bundle only ever activates if the user
        // taps Install Now, so the whole mechanism looks like it works — the
        // fetch is narrated, the bundle stages — and silently never goes live.
        BundleStore.activate()

        let bridge = BridgeController(deepLinks: deepLinks, backend: backend)
        self.bridge = bridge

        let window = UIWindow(frame: UIScreen.main.bounds)
        window.rootViewController = MainViewController(bridge: bridge)
        window.makeKeyAndVisible()
        self.window = window

        return true
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

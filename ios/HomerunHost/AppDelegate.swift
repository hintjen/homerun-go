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
            NSLog("[host] FFI ABI version %u", homerun_abi_version())
        #endif

        // A cold-start auth callback arrives here, long before the WebView can
        // receive anything; DeepLinkManager holds it for `deep-link:consume`.
        if let url = launchOptions?[.url] as? URL {
            deepLinks.handle(url: url)
        }

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
}

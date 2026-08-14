import UIKit
import WebKit

/// Full-screen host for the shared UI. There is no native chrome by design —
/// every screen in this app comes from the `homerun-app-ui` bundle.
final class MainViewController: UIViewController {
    private let bridge: BridgeController

    init(bridge: BridgeController) {
        self.bridge = bridge
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not used — this app has no storyboards")
    }

    override func viewDidLoad() {
        super.viewDidLoad()

        // Matches the WebView's backing colour and the launch screen's, so
        // launch, a reload and a content-process restart all show the same
        // brand blue instead of flashing white — or, on a dark-mode device,
        // black — on the way to the bundle's splash.
        view.backgroundColor = Brand.launchBackground

        // The page owns the colour behind the status bar, so it owns the
        // status bar's own contrast — and the backdrop this controller paints,
        // which sits under the WebView and shows wherever that is not opaque.
        // Both follow the page rather than the device: the theme setting
        // defaults to `system` but a player can pin it.
        bridge.onThemeChanged = { [weak self] theme in
            self?.view.backgroundColor = Brand.backdrop(for: theme)
            self?.setNeedsStatusBarAppearanceUpdate()
        }

        let webView = bridge.webView
        webView.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(webView)

        // Pinned to the view, not the safe area: the UI handles insets in CSS
        // with env(safe-area-inset-*), so letting UIKit inset it as well
        // double-counts the notch.
        NSLayoutConstraint.activate([
            webView.topAnchor.constraint(equalTo: view.topAnchor),
            webView.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            webView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            webView.trailingAnchor.constraint(equalTo: view.trailingAnchor),
        ])
    }

    /// Follows the page rather than the device, because the two can disagree —
    /// the UI's theme setting is `system` by default but a player can pin it.
    /// Hard-coding `.darkContent` here is what left the clock and the battery
    /// black on a dark page.
    ///
    /// Until the page reports (launch, and again across a reload) the backdrop
    /// is brand blue, which wants white.
    override var preferredStatusBarStyle: UIStatusBarStyle {
        bridge.pageTheme == .light ? .darkContent : .lightContent
    }
}

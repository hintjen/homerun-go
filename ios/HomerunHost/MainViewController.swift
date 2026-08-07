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

        // Matches the WebView's backing colour so a reload or a content-process
        // restart does not flash through to a black window on a dark-mode
        // device; the UI theme is light.
        view.backgroundColor = .white

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

    /// The UI is light-only; without this the status bar text disappears
    /// against it on a dark-mode device.
    override var preferredStatusBarStyle: UIStatusBarStyle { .darkContent }
}

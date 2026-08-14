import UIKit

/// Colours the host paints behind the WebView.
///
/// The launch colour is read out of the asset catalog rather than written as a
/// literal here: `UILaunchScreen` in Info.plist can only name a catalog colour,
/// so a literal in Swift would be a second copy of the same value that nothing
/// would notice going stale.
///
/// The two page colours below *are* second copies, and there is no way around
/// it — the host has to paint something behind a WebView that is deliberately
/// not opaque, and it cannot ask the page synchronously. They mirror
/// `--background` in `homerun-app-ui/styles/globals.css`; change them together.
/// Being slightly wrong here is a seam at the edges of the page, not a wrong
/// page, which is why a literal is tolerable where the launch colour's was not.
enum Brand {
    /// `#5677DA`. Behind the WebView from launch until the page reports a theme.
    static let launchBackground = UIColor(named: "LaunchBackground") ?? UIColor(
        red: 0x56 / 255, green: 0x77 / 255, blue: 0xDA / 255, alpha: 1)

    /// `--background` in light mode: `hsl(0 0% 100%)`.
    static let pageLight = UIColor(red: 1, green: 1, blue: 1, alpha: 1)

    /// `--background` in dark mode: `#121214`.
    static let pageDark = UIColor(
        red: 0x12 / 255, green: 0x12 / 255, blue: 0x14 / 255, alpha: 1)

    /// What belongs behind the page right now.
    ///
    /// `nil` means no page has reported yet — a cold start, a reload, or a
    /// content process that has just come back — and the answer is the launch
    /// field, which is what keeps those three moments from flashing.
    static func backdrop(for theme: PageTheme?) -> UIColor {
        guard let theme else { return launchBackground }
        switch theme {
        case .light: return pageLight
        case .dark: return pageDark
        }
    }
}

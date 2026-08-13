import UIKit

/// Colours the host paints itself, in the moments before the shared UI can.
///
/// There is exactly one, and it is deliberately read out of the asset catalog
/// rather than written as a literal here: `UILaunchScreen` in Info.plist can
/// only name a catalog colour, so a literal in Swift would be a second copy of
/// the same value that nothing would notice going stale.
enum Brand {
    /// `#5677DA`. Behind the WebView from launch until the page paints.
    static let launchBackground = UIColor(named: "LaunchBackground") ?? UIColor(
        red: 0x56 / 255, green: 0x77 / 255, blue: 0xDA / 255, alpha: 1)
}

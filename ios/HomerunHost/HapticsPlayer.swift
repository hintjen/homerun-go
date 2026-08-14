import UIKit

/// What the user just did, as the page reports it on the `haptic` channel.
///
/// The raw values are the wire contract — `HapticPattern` in
/// `homerun-app-ui/lib/bridge/channels.ts` — so they are the six words the page
/// sends, not six descriptions of how the Taptic Engine should behave. Which
/// surfaces are allowed to send which is `docs/style.md` §16 in that repo.
enum HapticPattern: String {
    /// The value under the finger changed: a picker row, a switch, a tab.
    case selection
    /// A gesture or navigation landed: the edge swipe back, a tab popped to root.
    case navigate
    /// A consequential action was authorised: delete a server, force stop.
    case commit
    /// The thing the user asked for finished.
    case success
    /// The input was refused before anything was attempted.
    case warning
    /// It was attempted and it failed.
    case error
}

/// Plays a ``HapticPattern`` on the Taptic Engine.
///
/// The mapping below is `HAPTIC_MAPPINGS` in `homerun-app-ui/lib/haptics.ts`,
/// which names itself the specification for this file. Keep the two together:
/// a test there asserts the table covers the wire union exactly, so a seventh
/// pattern will arrive here as a `nil` from `HapticPattern(rawValue:)` rather
/// than as a compile error.
///
/// # Why the generators are held rather than made per call
///
/// A `UIFeedbackGenerator` warms the Taptic Engine when it is created or
/// ``prepare()``d, and the engine idles back down after a moment. Constructing
/// one at the instant of the tap pays that spin-up as latency on the very thing
/// whose entire value is arriving *with* the touch. So they are kept, and each
/// play re-``prepare()``s for the next one — the buzz that follows a tap is
/// nearly always followed by another within a few seconds.
///
/// # Why silence is usually not a bug
///
/// These are no-ops when the owner has Settings → Sounds & Haptics → System
/// Haptics switched off, when Low Power Mode is on, and on every Simulator,
/// which has no Taptic Engine at all. All three are correct, and the host must
/// not route around any of them.
@MainActor
enum HapticsPlayer {

    private static let selection = UISelectionFeedbackGenerator()
    private static let light = UIImpactFeedbackGenerator(style: .light)
    private static let rigid = UIImpactFeedbackGenerator(style: .rigid)
    private static let notification = UINotificationFeedbackGenerator()

    static func play(_ pattern: HapticPattern) {
        switch pattern {
        case .selection:
            selection.selectionChanged()
            selection.prepare()

        // Light: a navigation landing should feel like the screen moved, not
        // like something was committed.
        case .navigate:
            light.impactOccurred()
            light.prepare()

        // Rigid rather than heavy — sharper and more definite, which is what
        // authorising something destructive should feel like.
        case .commit:
            rigid.impactOccurred()
            rigid.prepare()

        case .success:
            notification.notificationOccurred(.success)
            notification.prepare()

        case .warning:
            notification.notificationOccurred(.warning)
            notification.prepare()

        case .error:
            notification.notificationOccurred(.error)
            notification.prepare()
        }
    }
}

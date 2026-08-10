import Foundation
import UserNotifications

/// Local notifications for `push-notification`.
///
/// Authorization is requested on first use, not at launch. A permission prompt
/// shown before the user knows what the app does is the reliable way to get
/// denied, and a denial is permanent from the app's side.
enum NotificationPresenter {
    static func show(title: String, body: String) async {
        let center = UNUserNotificationCenter.current()

        let settings = await center.notificationSettings()
        switch settings.authorizationStatus {
        case .notDetermined:
            guard let granted = try? await center.requestAuthorization(options: [.alert, .sound]),
                granted
            else { return }
        case .denied:
            // Nothing to do, and nothing to report: the UI treats this channel
            // as fire-and-forget, and a player who said no meant it.
            return
        default:
            break
        }

        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default

        // nil trigger delivers immediately; while the app is foregrounded iOS
        // suppresses the banner unless a delegate opts in, which is fine —
        // the player is already looking at the screen.
        let request = UNNotificationRequest(
            identifier: UUID().uuidString, content: content, trigger: nil)
        try? await center.add(request)
    }
}

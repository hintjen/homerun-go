import FirebaseCore
import FirebaseMessaging
import Foundation
import UserNotifications

/// The host's half of remote push: the FCM token, and nothing else.
///
/// Same split as Android (`remotePush` in the bridge contract): this host owns
/// what must be native — the OS permission and the token Firebase mints. FCM
/// relays to this platform through APNs, so `AppDelegate` registers for remote
/// notifications and forwards the APNs token to Firebase; the FCM registration
/// token that comes back is what crosses the bridge. Registering it with the
/// API is the *UI's* job, over the user's own JWT, which is why no identity
/// appears anywhere in this file.
///
/// Delivery needs even less of us than on Android: an APNs notification is
/// drawn by the system whether the app is running or not, and iOS has no
/// notification-channel concept to pre-create. The two delegate callbacks
/// below exist for the two moments the system consults the app — presenting
/// while foregrounded, and a tap.
///
/// # Unconfigured is a state, not an error
///
/// Everything degrades to null when Firebase is not set up — a missing
/// `GoogleService-Info.plist`, a simulator with no APNs. `push:get-token`
/// answers null, the UI treats null as "not yet", and nothing breaks. That is
/// the contract's rule, and it is also what makes a build without the plist
/// runnable at all: `FirebaseApp.configure()` crashes without one, so
/// [configureIfPossible] checks first and quietly does nothing.
final class PushMessaging: NSObject {

    static let shared = PushMessaging()

    /// Set by `BridgeController` alongside its other event closures; nil while
    /// no page is attached. Events emitted through it before the page's
    /// `__bridge:ready` are queued by the controller, which is what makes the
    /// cold-start tap below deliverable at all.
    var emit: ((String, [Any]) -> Void)?

    /// Whether `FirebaseApp.configure()` ran. Read by the handlers so an
    /// unconfigured build answers null instead of touching Firebase APIs that
    /// would assert.
    private(set) var configured = false

    /// The token FCM most recently minted, so `push:get-token` answers without
    /// a round trip once one exists.
    private(set) var lastToken: String?

    /// Called from `AppDelegate` before the bridge exists. Safe to call on a
    /// build with no `GoogleService-Info.plist` — that is the whole point.
    func configureIfPossible() {
        guard Bundle.main.path(forResource: "GoogleService-Info", ofType: "plist") != nil else {
            HostLog.host.info("push: no GoogleService-Info.plist, remote push inert")
            return
        }
        FirebaseApp.configure()
        Messaging.messaging().delegate = self
        UNUserNotificationCenter.current().delegate = self
        configured = true
    }

    /// The current FCM token, or nil while there is none — unpermitted APNs,
    /// simulator, or Firebase still starting up. Nil is a state; the
    /// `push:token-changed` emit is what announces one arriving later.
    func currentToken() async -> String? {
        guard configured else { return nil }
        if let token = lastToken { return token }
        let token = try? await Messaging.messaging().token()
        if let token { lastToken = token }
        return token
    }

    /// The OS permission state in the contract's vocabulary.
    func permissionStatus() async -> String {
        let settings = await UNUserNotificationCenter.current().notificationSettings()
        switch settings.authorizationStatus {
        case .notDetermined: return "notDetermined"
        case .denied: return "denied"
        // Provisional and ephemeral both deliver, which is what the UI is
        // actually asking about.
        default: return "granted"
        }
    }

    /// Show the OS permission sheet and resolve with the outcome. iOS shows
    /// the sheet exactly once per install; asking again after a decision
    /// resolves immediately with that decision, which is also what the
    /// contract promises the UI.
    func requestPermission() async -> String {
        _ = try? await UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound, .badge])
        // APNs registration is idempotent and cheap; re-running it after a
        // grant is what turns the grant into a token.
        await MainActor.run { UIApplication.shared.registerForRemoteNotifications() }
        return await permissionStatus()
    }
}

extension PushMessaging: MessagingDelegate {
    /// FCM minted or rotated the registration token. The UI re-upserts it to
    /// the API on every firing — a rotation the API never hears about is a
    /// phone that silently stops receiving.
    func messaging(_ messaging: Messaging, didReceiveRegistrationToken fcmToken: String?) {
        guard let token = fcmToken else { return }
        lastToken = token
        emit?("push:token-changed", [token])
    }
}

extension PushMessaging: UNUserNotificationCenterDelegate {
    /// Foregrounded delivery: **suppressed, deliberately** — the app being
    /// open is the notification surface. The bell and the server card are
    /// already showing the event, and a system banner on top of a page that
    /// says the same thing is noise. A backgrounded app never reaches this
    /// method; the system draws its banner, which is the case push exists
    /// for. Same rule as Android's `onMessageReceived`.
    ///
    /// The log line is load-bearing: with no banner it is the only evidence
    /// a foreground delivery happened, and "delivered but suppressed" must
    /// stay distinguishable from "never delivered".
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        HostLog.host.info("push: received in foreground, banner suppressed")
        return []
    }

    /// A tap, cold start included. `push:opened` rides the controller's
    /// pre-ready queue, so a tap that *launches* the app is held until the
    /// page's handshake — the same shape as the cold-start deep link. The
    /// `href` is `UserNotification.href`; the shared UI routes it with the
    /// same vocabulary as a bell click, and the host never interprets it.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        let userInfo = response.notification.request.content.userInfo
        var payload: [String: Any] = [:]
        if let href = userInfo["href"] as? String { payload["href"] = href }
        if let id = userInfo["id"] as? String { payload["id"] = id }
        HostLog.host.info("push: notification tap, href=\(payload["href"] as? String ?? "none", privacy: .public)")
        emit?("push:opened", [payload])
    }
}

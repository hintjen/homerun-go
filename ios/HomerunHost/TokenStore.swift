import Foundation
import Security

/// Keychain storage for the two bearer tokens the host holds.
///
/// Android keeps these in private SharedPreferences, which is the platform's
/// equivalent. On iOS the Keychain is the right home: `UserDefaults` is a
/// plist inside the app container, and it lands in device backups. The device
/// token in particular never expires, so it is worth the extra twenty lines.
///
/// `kSecAttrAccessibleAfterFirstUnlock` rather than the stricter
/// `WhenUnlocked`: the host may need to reach the API while the screen is
/// locked and a server is running.
enum TokenStore {
    /// The signed-in user's JWT, handed to us by the UI on `credentials-received`.
    static var accessToken: String? {
        get { read("accessToken") }
        set { write("accessToken", newValue) }
    }

    /// Device-scoped token from registration, for the reporting endpoints.
    /// Not interchangeable with the user's JWT.
    static var deviceToken: String? {
        get { read("deviceToken") }
        set { write("deviceToken", newValue) }
    }

    // MARK: - Keychain

    private static let service = "app.gethomerun.ios.tokens"

    private static func query(_ account: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    private static func read(_ account: String) -> String? {
        var query = query(account)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
            let data = item as? Data
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    private static func write(_ account: String, _ value: String?) {
        // Delete first: SecItemAdd fails with errSecDuplicateItem otherwise,
        // and an update-or-insert dance is longer than this for no gain.
        SecItemDelete(query(account) as CFDictionary)

        guard let value, let data = value.data(using: .utf8) else { return }
        var attributes = query(account)
        attributes[kSecValueData as String] = data
        attributes[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock

        let status = SecItemAdd(attributes as CFDictionary, nil)
        guard status == errSecSuccess else {
            // Never swallow this. A failed write turns into "not signed in"
            // several steps later, at a screen that has nothing to do with
            // the Keychain. -34018 (errSecMissingEntitlement) in particular
            // means the app is unsigned — the Keychain needs an application
            // identifier, which an ad-hoc simulator build still provides.
            NSLog("[keychain] could not store %@: OSStatus %d", account, Int(status))
            return
        }
    }
}

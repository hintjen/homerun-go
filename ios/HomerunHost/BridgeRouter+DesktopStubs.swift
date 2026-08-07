import Foundation

/// Benign answers for channels the UI calls on every host, even though they
/// only mean something on desktop.
///
/// PROTOCOL.md §6 marks these `ungated: true`: the surfaces that call them —
/// Discord presence, the firewall port toggle, the WSL distro tag — are not
/// capability-gated yet, so the calls arrive here regardless of what the iOS
/// profile declares. The contract's instruction is to reply benignly rather
/// than error, because an error would surface to a player as a broken feature
/// they never asked for.
///
/// These are *not* in the iOS required list. The conformance checker treats
/// declared-but-not-required as a note rather than a failure, precisely so a
/// host can answer them. When the UI gates these properly, delete this file.
extension BridgeRouter {

    /// Windows firewall. Nothing to open on iOS — the contract's note says to
    /// report success until the UI gates the port toggle.
    func updateFirewallRules(_ params: Any?) async throws -> Any? {
        ["success": true]
    }

    func discordGetStatus(_ params: Any?) async throws -> Any? {
        "disconnected"
    }

    func discordGetUserID(_ params: Any?) async throws -> Any? {
        nil
    }

    /// There is no Discord rich-presence integration on mobile, so the button
    /// reports that it did nothing rather than pretending it worked.
    func discordOpenApp(_ params: Any?) async throws -> Any? {
        false
    }

    /// Presence updates, and the WSL distro tag. All no-ops: fire-and-forget
    /// channels with nothing to do on this platform.
    func ignoredSend(_ params: Any?) async throws -> Any? {
        nil
    }
}

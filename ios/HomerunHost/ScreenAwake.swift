import UIKit

/// Keeps the screen from locking while something needs this app in front.
///
/// # Why this is the whole of iOS backgrounding for us
///
/// iOS cannot host a server in the background — see
/// `plans/ios-background-execution.md` for the full sweep behind that. A
/// suspended app is a stopped server, so on this platform "how long may a
/// session last" is exactly "how long does the screen stay on", and the auto-
/// lock timeout is the only thing standing between a player and their friends
/// being disconnected. That makes this small file load-bearing in a way its
/// size does not suggest.
///
/// # Why it is not just `isIdleTimerDisabled = true`
///
/// That property is a plain `Bool` with no reference counting, and this app has
/// two independent things that need it: a running server, and a backup
/// uploading a world. Two owners setting and clearing a shared flag is a race
/// with an unpleasant signature — whichever finishes first clears it for both,
/// so the screen starts locking *while a server is still running*, and it
/// presents as an intermittent disconnection rather than as a bug in either
/// caller.
///
/// So hold a named reason instead. Reasons are a set rather than a count, which
/// makes ``hold(_:)`` idempotent by construction — and it has to be, because
/// the caller that matters most re-announces `running` on a repeat and would
/// otherwise leak a count on every announcement.
@MainActor
enum ScreenAwake {

    /// A server is starting, running, or saving its world on the way down.
    static let hosting = "hosting"

    /// A world is being uploaded. Outlives the server it belongs to.
    static let backup = "backup"

    private static var reasons: Set<String> = []

    /// Keep the screen awake for [reason]. Safe to call repeatedly.
    static func hold(_ reason: String) {
        guard reasons.insert(reason).inserted else { return }
        apply()
    }

    /// Give up [reason]. Safe to call for a reason that is not held.
    static func release(_ reason: String) {
        guard reasons.remove(reason) != nil else { return }
        apply()
    }

    private static func apply() {
        let wanted = !reasons.isEmpty
        guard UIApplication.shared.isIdleTimerDisabled != wanted else { return }
        UIApplication.shared.isIdleTimerDisabled = wanted
        // Built as plain locals rather than expressions inside the
        // interpolation — `os.Logger` overload resolution is fussier than it
        // looks, and this file's whole job is to be boring.
        let verb = wanted ? "held awake" : "released"
        let holders = reasons.sorted().joined(separator: ",")
        // Worth a line each way. A device that locks mid-session presents as a
        // network fault, and this is the log that says whether the app ever
        // asked for it to stay awake.
        HostLog.host.info(
            "screen \(verb, privacy: .public) — holders: [\(holders, privacy: .public)]")
    }
}

import Foundation
import os

/// Host diagnostics.
///
/// > **Never use `NSLog` or `print` in this app.** Both write to stderr, and
/// > once a server starts the Rust layer has replaced fds 1 and 2 with a pipe
/// > feeding the *player-visible* Minecraft console. Host logging would show
/// > up as server output — and worse, it loops: logging that the UI asked for
/// > the console produces a line, which the UI then reads, which logs again.
///
/// `os.Logger` goes to the unified logging system rather than a file
/// descriptor, so it survives the redirect and stays out of the console. Read
/// it with:
///
///     xcrun simctl spawn booted log stream --predicate 'process == "Homerun"'
enum HostLog {
    private static let subsystem = "app.gethomerun.ios"

    static let host = Logger(subsystem: subsystem, category: "host")
    static let bridge = Logger(subsystem: subsystem, category: "bridge")
    static let device = Logger(subsystem: subsystem, category: "device")
    static let tunnel = Logger(subsystem: subsystem, category: "tunnel")
    static let reporting = Logger(subsystem: subsystem, category: "reporting")
    static let keychain = Logger(subsystem: subsystem, category: "keychain")
    /// Over-the-air UI bundles. Its own category because every decision it
    /// makes is narrated: a silent fallback is indistinguishable from a
    /// network problem, and this is the only console a shipped build has.
    static let bundle = Logger(subsystem: subsystem, category: "bundle")
}

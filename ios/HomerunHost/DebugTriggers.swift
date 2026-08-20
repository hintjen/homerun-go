#if DEBUG

    import Foundation

    /// Make this app fail on purpose, to prove that failures are reported.
    /// **Debug builds only** — the whole file is compiled out otherwise.
    ///
    /// Error reporting is the one feature whose own failure is invisible: a
    /// reporter that quietly sends nothing looks exactly like an app with no
    /// bugs. So there has to be a way to produce a known failure on a real
    /// device and go looking for the row at the other end.
    ///
    /// The Android host does this with a broadcast. iOS has no equivalent, so
    /// this reads an environment variable, which both Xcode schemes and
    /// `xcrun simctl launch` can set:
    ///
    ///     Product ▸ Scheme ▸ Edit Scheme ▸ Run ▸ Arguments ▸ Environment
    ///     HOMERUN_DEBUG_ERROR = trap
    ///
    ///     xcrun simctl launch --console \
    ///       --setenv HOMERUN_DEBUG_ERROR=trap <device> app.gethomerun.mobile
    ///
    /// | Mode | What it proves |
    /// |---|---|
    /// | `report` | The live path. Sends immediately, kills nothing. |
    /// | `nsexception` | `NSSetUncaughtExceptionHandler` → stash → next launch. |
    /// | `trap` | A Swift trap, which that handler does **not** catch. MetricKit. |
    /// | `hang` | The main thread stops. `MXHangDiagnostic`. |
    ///
    /// Nothing here covers the page: for a JS error, attach Safari's Web
    /// Inspector and throw one by hand. That needs no code and is the tool
    /// somebody debugging the WebView already has open.
    enum DebugTriggers {

        /// Called from `didFinishLaunchingWithOptions`. Does nothing at all
        /// unless the variable is set.
        static func armIfRequested() {
            guard let mode = ProcessInfo.processInfo.environment["HOMERUN_DEBUG_ERROR"] else {
                return
            }
            HostLog.host.info("debug: arming \(mode, privacy: .public) in \(Self.delay)s")
            // After launch finishes, not during it. A crash inside
            // `didFinishLaunchingWithOptions` dies before `AppErrors.start()`
            // has a directory to stash into, which tests the wrong thing and
            // looks like the reporter is broken.
            DispatchQueue.main.asyncAfter(deadline: .now() + Self.delay) { fire(mode) }
        }

        private static func fire(_ mode: String) {
            HostLog.host.info("debug: firing \(mode, privacy: .public)")
            switch mode {
            case "report":
                AppErrors.report(
                    kind: "DebugTrigger",
                    message: "deliberate non-fatal, for verification",
                    location: "debug-trigger")

            case "nsexception":
                NSException(
                    name: NSExceptionName("HomerunDebugException"),
                    reason: "deliberate NSException, for verification",
                    userInfo: nil
                ).raise()

            case "trap":
                // Not an NSException. This traps, which is the gap
                // `ExitDiagnostics` exists to close — if a row shows up for
                // this one, MetricKit is genuinely wired.
                fatalError("deliberate Swift trap, for verification")

            case "hang":
                // On the main thread, which is what makes it a hang rather
                // than a slow background task nobody notices.
                Thread.sleep(forTimeInterval: Self.hangSeconds)

            default:
                HostLog.host.error("debug: unknown HOMERUN_DEBUG_ERROR mode \(mode, privacy: .public)")
            }
        }

        private static let delay: TimeInterval = 3
        private static let hangSeconds: TimeInterval = 12
    }

#endif

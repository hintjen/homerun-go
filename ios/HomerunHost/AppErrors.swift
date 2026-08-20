import Foundation

/// Everything this app failed at, on its way to the API.
///
/// # What was here before
///
/// Nothing. A Swift crash left an `os.Logger` line on the device; a panic in
/// the native core wrote a file nothing opened; a throw inside the WebView
/// reached ``BridgeController``'s document-start hook and went to the device
/// log, where it stayed. The shared bundle carries Sentry, but Sentry there is
/// a *renderer* integration — it can see the page's errors and cannot see a
/// Swift stack, a Kotlin stack or a Rust panic at all, and it does not know
/// which over-the-air bundle was running.
///
/// # What this type does and does not decide
///
/// It decides nothing. Whether two failures are the same bug, whether this one
/// is worth sending again, what has to be redacted before it leaves the device
/// and what the body looks like are all answered in
/// `homerun-core::reporting::app_error`, so that this host and the Android one
/// cannot drift on any of them. What lives here is what only a platform knows:
/// the clock, the app's own version, the bundle it is running, and the
/// credential to sign with.
///
/// # Two paths, because a dying process cannot finish a request
///
/// ``report(_:)`` is the ordinary one: the core decides, and a request goes
/// out on a detached task if there is one to send.
///
/// ``stash(kind:message:stack:)`` is for the crash handler.
/// `NSSetUncaughtExceptionHandler` runs with the process already unwinding
/// towards termination; a `Task` never resumes and a `URLSession` request
/// never completes. So the crash writes a file, synchronously, and
/// ``drain()`` sends it on the next launch.
///
/// # It must never report itself
///
/// Every failure on this path is logged and dropped. A reporter that reports
/// its own failures turns one bad response into an infinite loop, and it does
/// so fastest exactly when the API is already struggling.
enum AppErrors {

    /// One per process. It is what makes "this person hit forty errors in one
    /// sitting" a question the API can answer — without it, forty rows from
    /// one bad afternoon look like forty unrelated reports.
    private static let session = UUID().uuidString

    // MARK: - Lifecycle

    /// Point the core's crash directory at storage this app owns, and take
    /// over the uncaught-exception handler.
    ///
    /// Called first in `didFinishLaunchingWithOptions`, because the window it
    /// protects starts at the first line of launch.
    static func start() {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        // Application Support rather than Documents: the host puts worlds in
        // Documents precisely so a player can reach them through the Files
        // app, and a crash file is not something to hand them.
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        Core.appErrorAttach(dataDir: dir.path)

        installExceptionHandler()
    }

    /// Send whatever the last launch left behind.
    ///
    /// Called late in launch, once a credential and an API URL exist. The
    /// reports themselves were written long before that and do not care, but
    /// the request that carries them does.
    static func drain() {
        Task.detached(priority: .utility) {
            let requests = Core.appErrorDrain(context: context())
            if !requests.isEmpty {
                HostLog.reporting.info(
                    "sending \(requests.count, privacy: .public) report(s) from the last run")
            }
            for request in requests {
                await send(request)
            }
        }
    }

    // MARK: - Reporting

    /// Report one failure.
    ///
    /// The core very often decides not to send, and that is the design working
    /// rather than something to log about.
    static func report(
        source: String = AppErrors.sourceHost,
        severity: String = AppErrors.severityError,
        kind: String,
        message: String,
        stack: String? = nil,
        location: String? = nil,
        extra: [String: Any]? = nil,
        atMs: Int? = nil
    ) {
        let occurrence = occurrenceOf(
            source: source, severity: severity, kind: kind, message: message,
            stack: stack, location: location, extra: extra, atMs: atMs)

        Task.detached(priority: .utility) {
            guard let request = Core.appErrorReport(context: context(), occurrence: occurrence)
            else { return }
            await send(request)
        }
    }

    /// Report a failure the page described.
    ///
    /// The payload is the page's, but `source` is not: a bundle is replaced
    /// over the air and is the least trusted thing in the process, so it does
    /// not get to file a report as a native crash or a host crash. Anything
    /// that is not `api` is recorded as `ui`, which is what it is.
    ///
    /// `atMs` is filled in when the page omits it. Zero would be the epoch,
    /// and a report dated 1970 sorts to the bottom of every view that matters.
    static func reportFromPage(_ occurrence: [String: Any]) {
        var safe = occurrence
        let claimed = occurrence["source"] as? String
        safe["source"] = claimed == sourceAPI ? sourceAPI : sourceUI
        if (occurrence["atMs"] as? Int ?? 0) <= 0 {
            safe["atMs"] = Int(Date().timeIntervalSince1970 * 1_000)
        }

        Task.detached(priority: .utility) {
            guard let request = Core.appErrorReport(context: context(), occurrence: safe)
            else { return }
            await send(request)
        }
    }

    /// Report a Swift error.
    static func report(_ error: Error, location: String? = nil) {
        report(
            kind: String(describing: type(of: error)),
            message: error.localizedDescription,
            location: location)
    }

    /// Write one failure to disk without sending it.
    ///
    /// Synchronous on purpose — see the type header.
    static func stash(kind: String, message: String, stack: String?, location: String? = nil) {
        Core.appErrorStash(
            context: context(),
            occurrence: occurrenceOf(
                source: sourceHost, severity: severityFatal, kind: kind, message: message,
                stack: stack, location: location, extra: nil))
    }

    // MARK: - The uncaught handler

    /// Catch what Objective-C throws on its way out.
    ///
    /// Honest about its reach: this fires for `NSException`, which covers the
    /// UIKit and Foundation failures that make up most iOS crashes — an array
    /// index, a bad selector, a KVO teardown. It does **not** fire for a Swift
    /// `fatalError`, a force-unwrapped nil or an out-of-bounds `Array`
    /// subscript, which trap rather than throw and take the process down
    /// through a signal this deliberately does not install a handler for.
    /// Installing one is possible and is a considerably more dangerous thing
    /// to do from a signal context than it looks; a Rust panic already arrives
    /// through ``Core`` instead, and Swift traps remain a gap.
    ///
    /// The previous handler is chained rather than replaced. Dropping it would
    /// suppress the crash report Apple collects, so a crash would become
    /// invisible to everyone but us.
    private static func installExceptionHandler() {
        previousHandler = NSGetUncaughtExceptionHandler()
        NSSetUncaughtExceptionHandler { exception in
            AppErrors.stash(
                kind: exception.name.rawValue,
                message: exception.reason ?? exception.name.rawValue,
                stack: exception.callStackSymbols.joined(separator: "\n"))
            AppErrors.previousHandler?(exception)
        }
    }

    /// Held so the handler above can chain to it. A C function pointer cannot
    /// capture, so this has to be reachable statically.
    private static var previousHandler: (@convention(c) (NSException) -> Void)?

    // MARK: - Context

    /// What this install is, as far as anything can tell right now.
    ///
    /// Deliberately forgiving. Every reader below can be absent — the device
    /// may not be registered, the keychain is unreadable before first unlock —
    /// and the report that arrives during exactly that window is the one worth
    /// most. A partial context beats no report.
    private static func context() -> [String: Any] {
        var context: [String: Any] = [
            "deviceId": HostStore.registeredDeviceId ?? "",
            "session": session,
            "platform": "ios",
            "appVersion": Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
                ?? "0.0.0",
            "bundle": BundleStore.active(),
            "hostRevision": BridgeRouter.hostRevision,
        ]
        // The core reads this to tell production from staging. It is never
        // sent verbatim, and deciding the deployment in one place is what
        // stops three platforms disagreeing about which one they are on.
        if let apiURL = HostStore.apiURL {
            context["apiUrl"] = apiURL
        }
        return context
    }

    private static func occurrenceOf(
        source: String,
        severity: String,
        kind: String,
        message: String,
        stack: String?,
        location: String?,
        extra: [String: Any]?,
        atMs: Int? = nil
    ) -> [String: Any] {
        var occurrence: [String: Any] = [
            "source": source,
            "severity": severity,
            "kind": kind,
            "message": message,
            // Now, unless the caller knows better. ``ExitDiagnostics`` does:
            // MetricKit hands over a crash that happened up to a day ago, and
            // stamping it with the moment it arrived would file every one of
            // them under "just now" and make the timeline useless.
            "atMs": atMs ?? Int(Date().timeIntervalSince1970 * 1_000),
        ]
        if let stack { occurrence["stack"] = stack }
        if let location { occurrence["location"] = location }
        if let extra { occurrence["extra"] = extra }
        return occurrence
    }

    // MARK: - Sending

    /// Sign it if this device has a credential, send it unsigned if it does
    /// not.
    ///
    /// Unsigned is not a fallback that lost something — it is the case this
    /// path exists for. A crash before registration, or on the login screen,
    /// has no token by definition, and those are the failures nobody can
    /// reproduce from a bug report.
    private static func send(_ request: Core.Request) async {
        guard let apiURL = HostStore.apiURL else { return }
        _ = await HomerunAPI.performAppError(
            apiURL: apiURL, request: request, token: TokenStore.deviceToken)
    }

    static let sourceHost = "host"
    static let sourceUI = "ui"
    static let sourceAPI = "api"
    /// Below the host language: a Rust panic, or a death the system reported
    /// afterwards because nothing of ours was alive to report it. See
    /// ``ExitDiagnostics``.
    static let sourceNative = "native"
    static let severityFatal = "fatal"
    static let severityError = "error"
}

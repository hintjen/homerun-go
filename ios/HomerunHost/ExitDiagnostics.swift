import Foundation
import MetricKit

/// Why the last process died, when it died in a way that could not tell us.
///
/// # The gap this closes
///
/// ``AppErrors`` covers everything the app can report about itself: a Swift
/// `NSException`, a page error, a Rust panic. All of those run code on the way
/// down. A Swift *trap* does not — `fatalError`, a force-unwrapped nil, an
/// out-of-bounds `Array` subscript and an arithmetic overflow all take the
/// process out through a signal, and `NSSetUncaughtExceptionHandler` never
/// fires for any of them. Neither does anything fire for a watchdog kill.
///
/// # Why not a signal handler
///
/// Because the platform already collects this, and a signal handler is the
/// most dangerous code we could write. It runs on a thread already in an
/// undefined state, everything it calls must be async-signal-safe — no malloc,
/// so no `JSONSerialization` and no Foundation at all — and a mistake in it
/// turns a crash Apple would have recorded cleanly into a corrupted one.
/// MetricKit is the same information, gathered by the system, handed over
/// later on an ordinary queue with the whole language available.
///
/// The Android half of this is `ExitReasons`, reading `ApplicationExitInfo`.
/// Same idea, same next-launch shape, same `native` source.
///
/// # What it cannot do, and why the fingerprint looks the way it does
///
/// **The frames are not symbolicated.** A crash arrives as a call-stack tree
/// of binary names and byte offsets — `HomerunHost+0x1a2b3c` — because the
/// dSYM that would turn that into a function name lives on the build machine,
/// not on the phone. The offsets are the whole diagnostic value, so they are
/// sent; the cost is that they move whenever the binary is recompiled, and a
/// native crash therefore regroups on each release.
///
/// That is why ``kind`` carries the signal. `crash (SIGSEGV)` is stable across
/// every build forever, so there is always a coarse group that answers "is
/// this still happening", underneath the per-build groups that answer "where".
/// The Android side reached the same arrangement from the opposite direction:
/// there the tombstone is a protobuf we cannot read at all, so the signal in
/// `kind` is the *only* discrimination there is.
///
/// # Delivery is not prompt
///
/// MetricKit hands over diagnostics at most once a day, on launch, for the
/// preceding 24 hours. A crash is not reportable within a minute of happening,
/// which is why ``AppErrors/report(source:severity:kind:message:stack:location:extra:atMs:)``
/// grew an explicit timestamp — stamping these on arrival would file a day of
/// crashes under "just now".
///
/// To see one during development without waiting a day, use Xcode's
/// **Debug ▸ Simulate MetricKit Payloads** against a device.
final class ExitDiagnostics: NSObject, MXMetricManagerSubscriber {

    /// One per process, held forever. `MXMetricManager` does not keep its
    /// subscribers alive, and a subscriber that deallocates is a subscriber
    /// that silently never hears anything.
    static let shared = ExitDiagnostics()

    private override init() { super.init() }

    /// Subscribe. Cheap, and safe to call once at launch.
    func start() {
        MXMetricManager.shared.add(self)
        HostLog.reporting.info("subscribed to MetricKit diagnostics")
    }

    // MARK: - MXMetricManagerSubscriber

    /// Required by the protocol and deliberately empty.
    ///
    /// These are the daily *metrics* — launch times, battery, disk. Useful,
    /// and not what this file is for: an error table filled with routine
    /// telemetry is an error table nobody reads.
    func didReceive(_ payloads: [MXMetricPayload]) {}

    /// The diagnostics: crashes, hangs, and resource exceptions.
    ///
    /// No high-water mark, unlike the Android side. MetricKit delivers a
    /// payload once, and the core's ledger already refuses to send the same
    /// fingerprint twice inside its cooldown — a second guard here would buy
    /// nothing and would make **Simulate MetricKit Payloads** silently do
    /// nothing on the second press, which is exactly when somebody is trying
    /// to work out whether this is wired up at all.
    func didReceive(_ payloads: [MXDiagnosticPayload]) {
        for payload in payloads {
            let at = Int(payload.timeStampEnd.timeIntervalSince1970 * 1_000)

            let crashes = payload.crashDiagnostics ?? []
            let hangs = payload.hangDiagnostics ?? []
            HostLog.reporting.info(
                "MetricKit payload: \(crashes.count, privacy: .public) crash(es), \(hangs.count, privacy: .public) hang(s)"
            )

            for crash in crashes.prefix(Self.maxPerPayload) {
                report(crash, at: at)
            }
            for hang in hangs.prefix(Self.maxPerPayload) {
                report(hang, at: at)
            }
        }
    }

    // MARK: - Crashes

    private func report(_ crash: MXCrashDiagnostic, at: Int) {
        let stack = frames(crash.callStackTree)

        var extra: [String: Any] = [:]
        if let signal = crash.signal { extra["signal"] = signal.intValue }
        if let type = crash.exceptionType { extra["exceptionType"] = type.intValue }
        if let code = crash.exceptionCode { extra["exceptionCode"] = code.intValue }
        if let reason = crash.terminationReason { extra["terminationReason"] = reason }
        extra["osVersion"] = crash.metaData.osVersion
        extra["deviceType"] = crash.metaData.deviceType

        AppErrors.report(
            source: AppErrors.sourceNative,
            severity: AppErrors.severityFatal,
            kind: "crash (\(Self.signalName(crash.signal)))",
            // Apple's own sentence, when there is one. It names the class of
            // failure — an unsatisfied Swift precondition reads differently
            // from a memory fault — and it is the half a person reads first.
            message: crash.terminationReason ?? "Crashed with \(Self.signalName(crash.signal))",
            stack: stack,
            location: leafBinary(crash.callStackTree),
            extra: extra,
            atMs: at)
    }

    // MARK: - Hangs

    /// A hang is reported as `error`, where the Android side reports an ANR as
    /// `fatal`. Not an inconsistency: Android only hears about an ANR when the
    /// system has already killed the process, while MetricKit reports any hang
    /// past its threshold — including ones the app came back from. Recording
    /// those as fatal would put recoveries in the same bucket as deaths.
    private func report(_ hang: MXHangDiagnostic, at: Int) {
        let stack = frames(hang.callStackTree)
        let seconds = hang.hangDuration.converted(to: .seconds).value

        AppErrors.report(
            source: AppErrors.sourceNative,
            severity: AppErrors.severityError,
            kind: "hang",
            // The duration is in `extra`, not here. The message is generalised
            // before it is hashed — digit runs become `#` — so putting it here
            // would neither group nor discriminate, it would just be noise.
            message: "The main thread stopped responding",
            stack: stack,
            location: leafBinary(hang.callStackTree),
            extra: ["hangSeconds": Int(seconds.rounded())],
            atMs: at)
    }

    // MARK: - Call stacks

    /// The attributed thread's frames, leaf first, in the shape the core's
    /// frame parser already reads for every other language.
    ///
    /// Leaf first is the important part. MetricKit's tree runs the other way —
    /// `callStackRootFrames` are the outermost frames, `main` and below, and
    /// `subFrames` descend towards where it actually stopped. The core keeps
    /// the *first* few frames on the grounds that the top of a stack is what
    /// identifies a bug, so handing it the tree in its own order would
    /// fingerprint every crash in the app on `main`.
    private func frames(_ tree: MXCallStackTree) -> String? {
        guard
            let root = try? JSONSerialization.jsonObject(with: tree.jsonRepresentation())
                as? [String: Any],
            let stacks = root["callStacks"] as? [[String: Any]]
        else { return nil }

        // The thread the system blamed, when it named one.
        let chosen = stacks.first { $0["threadAttributed"] as? Bool == true } ?? stacks.first
        guard let roots = chosen?["callStackRootFrames"] as? [[String: Any]] else { return nil }

        var chain: [String] = []
        var frame = roots.first
        while let current = frame, chain.count < Self.maxFrames {
            if let name = current["binaryName"] as? String {
                let offset = (current["offsetIntoBinary"] as? NSNumber)?.uint64Value ?? 0
                // `at symbol (file)` is what every other stack here looks like
                // by the time it reaches the core, so a native crash groups by
                // the same rules rather than needing a second parser that would
                // drift from the first.
                chain.append("    at \(name)+0x\(String(offset, radix: 16)) (\(name))")
            }
            // One path down, not the whole tree: a crash stack is a single
            // chain, and the branches a sampled tree carries are other threads'
            // business.
            frame = (current["subFrames"] as? [[String: Any]])?.first
        }

        return chain.isEmpty ? nil : chain.reversed().joined(separator: "\n")
    }

    /// The binary it stopped in — the closest thing here to Android's process
    /// name, and the fastest way to see "this is ours" against "this is WebKit".
    private func leafBinary(_ tree: MXCallStackTree) -> String? {
        guard
            let root = try? JSONSerialization.jsonObject(with: tree.jsonRepresentation())
                as? [String: Any],
            let stacks = root["callStacks"] as? [[String: Any]]
        else { return nil }

        let chosen = stacks.first { $0["threadAttributed"] as? Bool == true } ?? stacks.first
        guard var frame = (chosen?["callStackRootFrames"] as? [[String: Any]])?.first else {
            return nil
        }

        var depth = 0
        while let next = (frame["subFrames"] as? [[String: Any]])?.first, depth < Self.maxFrames {
            frame = next
            depth += 1
        }
        return frame["binaryName"] as? String
    }

    // MARK: - Constants

    private static func signalName(_ signal: NSNumber?) -> String {
        // Unwrapped first rather than switching over the Optional. Matching a
        // bare integer literal against an `Int?` leans on a `~=` overload that
        // is easy to get subtly wrong, and this file cannot be compiled where
        // it is being written.
        guard let signal = signal?.intValue else { return "no signal" }
        switch signal {
        // Darwin numbering, which differs from Linux's above 6: SIGBUS is 10
        // here and 7 on Android, and 7 here is SIGEMT. Getting this wrong
        // mislabels a group rather than breaking one, which is the kind of
        // error that survives a long time.
        case 4: return "SIGILL"
        case 5: return "SIGTRAP"
        case 6: return "SIGABRT"
        case 7: return "SIGEMT"
        case 8: return "SIGFPE"
        case 9: return "SIGKILL"
        case 10: return "SIGBUS"
        case 11: return "SIGSEGV"
        case 12: return "SIGSYS"
        default: return "signal \(signal)"
        }
    }

    /// Matches `errors::MAX_DRAIN` and the Android side, for the same reason.
    private static let maxPerPayload = 5

    /// The core keeps three frames and caps a stack at 8 KiB. This is enough
    /// for a person to symbolicate by hand with `atos` and nowhere near the cap.
    private static let maxFrames = 24
}

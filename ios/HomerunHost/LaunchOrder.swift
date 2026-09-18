import Foundation

/// Walks a launch through the steps `homerun-core` planned, in order.
///
/// # Why a plan rather than a written-out sequence
///
/// The order a launch runs in is a decision, not an implementation detail:
/// the tunnel is begun early and awaited late so its provisioning overlaps the
/// slow work; the restore happens after the world is known about and before
/// anything writes settings over it; the wait for a previous engine sits ahead
/// of anything that touches the server directory. Every host needs the same
/// order and three of them wrote it out by hand, which is how two of them
/// drifted.
///
/// # What this enforces
///
/// **Monotonicity, not exhaustiveness.** A host may skip a step it cannot
/// perform — this one asks for a linked plan, so the jar steps are not in it,
/// and it skips `ensureRuntime` and `acceptEula` by never arriving at them —
/// but it may not run one before a step the plan puts ahead of it. Arriving
/// out of order is a programming error and throws rather than quietly doing
/// the wrong thing.
///
/// **Checkpoints.** A stop that arrives mid-launch is honoured *before* the
/// next expensive or irreversible step rather than after it. Which steps those
/// are is the core's decision, carried on each step.
@MainActor
struct LaunchOrder {
    private let steps: [Core.Step]
    private let serverId: String
    private let lifecycle: Core.Lifecycle
    /// The furthest step reached, so arriving backwards can be detected.
    private var index = 0

    init(steps: [Core.Step], serverId: String, lifecycle: Core.Lifecycle) {
        self.steps = steps
        self.serverId = serverId
        self.lifecycle = lifecycle
    }

    /// Arrive at a step.
    ///
    /// Returns false when the plan does not include it — the host skips it and
    /// carries on. Throws when a stop is pending at a checkpoint, or when the
    /// step comes before one already run.
    @discardableResult
    mutating func at(_ name: String) throws -> Bool {
        guard let position = steps.firstIndex(where: { $0.name == name }) else {
            // Not in this launch's plan: no tunnel to open, no world to
            // restore. Legitimate, and not a reason to stop.
            return false
        }

        guard position >= index else {
            // The plan is a contract with the other hosts. A launch that runs
            // its steps in a different order is not doing the same thing they
            // are, however well it appears to work.
            throw ServerBackendError.engine(
                "Homerun Go tried to \(name) out of order while starting the server.")
        }
        index = position + 1

        if steps[position].checkpoint, lifecycle.shouldAbandon(serverId) {
            // Someone pressed Stop while this was preparing. Giving up here is
            // the whole reason the core marks these steps: the alternative is
            // finishing a launch nobody wants and then stopping it, which the
            // user watches happen.
            throw ServerBackendError.engine("The server was stopped before it finished starting.")
        }
        return true
    }
}

//! Who owns a server right now, and what its last exit meant.
//!
//! # Why this is in the core and not in each host
//!
//! Every host has to answer the same four questions while a server starts and
//! stops, and each of them had answered them separately:
//!
//!  - Is this server this device's right now? (`native-server-active-ids`)
//!  - May this start proceed, or is something already here?
//!  - A stop arrived — is there anything to stop yet?
//!  - The process is gone. Did the user ask for that, or did it fall over?
//!
//! Three of those four were wrong on Android at some point in one week, and
//! each was fixed by hand in Kotlin and then again in Swift. They are not
//! platform problems: none of them touches a process handle, a socket, or a
//! JNI call. They are bookkeeping, and bookkeeping belongs where it can be
//! written once and tested exhaustively.
//!
//! # The question that keeps being answered wrongly
//!
//! **"Active" is not "running."** A server is this device's from the moment a
//! start is asked for until its process is gone — through a jar downloading,
//! a world restoring, and the whole of a graceful shutdown while the world
//! saves.
//!
//! Answering the narrower question breaks something that looks unrelated. The
//! UI's reconcile loop compares this list against the API's `target_state`; an
//! id missing from it while the API still says `running` reads as *a start
//! issued from another device*, and the loop asks the API to `force_link_up`.
//! That regenerates the gateway's WireGuard keys. Both ends of a server's life
//! open that window, and Android fell into both:
//!
//!  - **Starting.** A launch restoring a world runs for minutes. The
//!    reprovision landed underneath a launch that had already resolved its
//!    tunnel config, so wireproxy came up against keys the gateway had thrown
//!    away — the tunnel connects, handshakes, and delivers nothing but
//!    keepalives.
//!  - **Stopping.** The dashboard invokes `native-server-stop` and PATCHes
//!    `stopped` only once that returns, so for the whole shutdown the API
//!    still reads `running`. The loop restarted the server the user had just
//!    stopped.
//!
//! The desktop never had either, because its `runningServers` set holds an id
//! until the process exits and it adds to `pendingStartup` synchronously,
//! before any await (`nativeServerManager.ts`). This module is that behaviour,
//! named and tested.
//!
//! # What this does not do
//!
//! No processes, no sockets, no async. The host still spawns the JVM, opens
//! the tunnel and watches stdout; it reports what happened here and asks what
//! it means. [`Lifecycle`] is serialisable for the same reason
//! [`crate::state::HandshakeWatch`] is: a host on the far side of a C ABI can
//! hold it as opaque bytes and hand it back, with no pointer to free.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::state::{exit_state, State};

/// How many servers this host can run at once.
///
/// Not a guess the core can make: the desktop runs several, a phone runs one
/// because a second JVM would exhaust it. It matches `multipleRunningServers`
/// in the host's declared capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Concurrency {
    One,
    Many,
}

/// What the host should do about a start it was asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "camelCase")]
pub enum StartVerdict {
    /// Go ahead. The server is already counted active — see the module docs
    /// for why that matters before the first byte is downloaded.
    Proceed,
    /// This exact server is already up or on its way. The bridge turns this
    /// into `{ success: true, alreadyRunning: true }`, which is what the
    /// reconcile loop expects to hear; it is not an error the player sees.
    AlreadyRunning,
    /// A different server holds the single slot this host has.
    AnotherServerRunning {
        #[serde(rename = "serverId")]
        server_id: String,
    },
}

/// What the host should do about a stop it was asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "camelCase")]
pub enum StopVerdict {
    /// The engine has reached its console: ask it to save and exit, and wait.
    Graceful,
    /// There is an engine, but it cannot hear a console command yet — it is
    /// still generating terrain. End it directly.
    ///
    /// A stop is carried out **now**, never deferred until the server has
    /// finished starting: waiting meant the user pressed Stop and watched the
    /// server carry on booting for a minute, which is not a stop. Nothing is
    /// lost by terminating here, because a server that never reached its
    /// console has saved no world to protect.
    Terminate,
    /// The launch has not produced anything to talk to yet — a jar is still
    /// downloading, or a world is still being restored. The intent is
    /// recorded; the launch will see it at its next checkpoint and give up.
    /// Answering "not running" here is what once let a stopped launch run to
    /// completion and start a server nobody wanted.
    AbandonLaunch,
    /// Nothing here by that name.
    NotRunning,
}

/// What an exited process meant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exit {
    pub state: State,
    /// True when someone asked for this. The host gates the on-stop backup on
    /// it — a crash must not overwrite a good snapshot with a half-saved
    /// world — so getting it wrong silently loses play.
    pub intentional: bool,
    /// True when this process belongs to a launch that has since been
    /// superseded by a newer start. Its exit says nothing about the server
    /// that is coming up now, and reporting it would flip a starting server
    /// to stopped.
    pub superseded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    state: State,
    /// Bridge calls in flight for this server, start or stop.
    ///
    /// A count, not a flag: concurrent calls for one id are normal — the
    /// reconcile loop issues its own start and is told `alreadyRunning` — and
    /// the loser must not clear a marker the winner still needs.
    calls: u32,
    /// The host has a live engine for this id.
    engine: bool,
    /// The engine has reached its console, so it can be asked to stop rather
    /// than terminated. Not the same as `state == Running`, which also waits
    /// on the tunnel.
    console: bool,
    /// A stop was asked for and has not been carried out.
    ///
    /// Deliberately not derived from [`Entry::state`], which a launch still in
    /// flight overwrites as it progresses: a stop arriving mid-startup set
    /// STOPPING, the launch reached RUNNING ten seconds later and clobbered
    /// it, and the exit was then classified as a crash.
    stop_requested: bool,
    /// Bumped by every start and every stop. A spawned process records the
    /// generation it belongs to, so an exit from a superseded launch can be
    /// told apart from the current one's.
    generation: u64,
    /// The generation that owns the live engine.
    engine_generation: u64,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            state: State::Stopped,
            calls: 0,
            engine: false,
            console: false,
            stop_requested: false,
            generation: 0,
            engine_generation: 0,
        }
    }
}

/// Every server this host is currently responsible for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lifecycle {
    concurrency: Concurrency,
    entries: BTreeMap<String, Entry>,
}

impl Lifecycle {
    pub fn new(concurrency: Concurrency) -> Self {
        Self {
            concurrency,
            entries: BTreeMap::new(),
        }
    }

    // ── Queries ─────────────────────────────────────────────────────────────

    /// Servers this device owns right now: running, coming up, or winding
    /// down. This is `native-server-active-ids`.
    ///
    /// Read the module docs before narrowing this. It is the single most
    /// misanswered question in the codebase, and the damage it does surfaces
    /// as a dead tunnel rather than as anything resembling a bookkeeping bug.
    pub fn active_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| e.is_active())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Servers actually accepting players. Narrower than [`Self::active_ids`]
    /// on purpose: this is what a "is it up yet" check wants.
    pub fn running_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| e.engine && e.state == State::Running)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn state(&self, id: &str) -> State {
        self.entries.get(id).map_or(State::Stopped, |e| e.state)
    }

    /// True when a launch must wait for a previous engine before it spawns.
    ///
    /// A start admitted during a stop is a *restart* — the core allows it, as
    /// the desktop does — and what makes that safe is not spawning until the
    /// outgoing engine is gone. Skip the wait and two servers share one
    /// directory: two jar downloads, two tunnels, two worlds writing over each
    /// other. The desktop calls this `waitForSupervisorIdle` and does it
    /// immediately before spawning; so should every host.
    ///
    /// Asked at the moment of spawning rather than answered once at admission,
    /// because the outgoing engine usually exits *during* the new launch's
    /// preparation — by the time it matters, the answer has often changed to
    /// no.
    pub fn await_previous_exit(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|e| e.engine)
    }

    /// True when starting this server must first cancel an on-stop backup of
    /// it that is still running.
    ///
    /// Always, when there is a start to speak of. It is stated here rather
    /// than left as a habit in each host because the *reasoning* is shared and
    /// non-obvious: the backup is redundant the moment this device relaunches
    /// (its local disk becomes the freshest copy, and the next stop backs it
    /// up again), and cancelling is safe because restic commits a snapshot
    /// atomically — an interrupted run leaves none, never a partial one. It
    /// may leave a stale lock, which the next backup clears.
    ///
    /// No backup state is reported for the cancelled run: the lease stays open
    /// until the next backup reports, and [`crate::backup::lease_decision`]
    /// never blocks a device on its own lease.
    pub fn supersedes_on_stop_backup(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|e| e.is_active())
    }

    /// True when a launch should give up at its next checkpoint.
    ///
    /// The host calls this before each irreversible step — before spawning,
    /// and again before opening the tunnel — so a stop that arrived during a
    /// long preparation is honoured promptly instead of after it finishes.
    pub fn should_abandon(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|e| e.stop_requested)
    }

    /// True when this state may be announced for this server.
    ///
    /// Guards one specific mistake: a launch still catching up announcing
    /// `running` for a server already on its way down. The UI would flip the
    /// card to running and the API would mark the service healthy, moments
    /// before it exits.
    ///
    /// **Do not ask this about an exit [`Self::exited`] has just returned.**
    /// That call prunes the entry once the device has nothing left in flight,
    /// and with no entry this refuses `stopped` — so whether the exit is
    /// announced comes down to a race between the host's exit callback and its
    /// stop call returning. Android lost that race only when the stop came
    /// from the notification rather than the app, and the server sat at
    /// `stopping` for ever with the foreground service pinned behind it.
    /// [`Exit::superseded`] is how an exit says it must not be announced.
    pub fn may_announce(&self, id: &str, state: State) -> bool {
        let Some(entry) = self.entries.get(id) else {
            return state != State::Stopped;
        };
        if entry.stop_requested && state == State::Running {
            return false;
        }
        // Deliberately not "and this is a change". The core reaches `Running`
        // when the console says the server is listening; the host announces it
        // only once the tunnel is up, which is later and on purpose — a server
        // on loopback is not one anyone can join. Two clocks, and the core's
        // must not veto the host's: comparing them suppressed the announcement
        // entirely, so the server ran, the tunnel came up, and the app never
        // heard about either. Repeat-suppression is the host's own business
        // and it already does it.
        true
    }

    // ── Events ──────────────────────────────────────────────────────────────

    /// A start call arrived.
    ///
    /// Call this **first**, before any network round-trip the start needs —
    /// looking up the server's settings and checking the backup lease both
    /// take long enough for a poll to land in the gap, and a server that is
    /// not yet counted active is a server the reconcile loop will try to
    /// start for itself.
    ///
    /// Call [`Self::call_finished`] when the call returns, unconditionally —
    /// a `finally` or `defer` that inspects the verdict first is a bug
    /// waiting to happen. Every verdict that claims the id also counts the
    /// call, and one that does not (there is nothing to claim) leaves nothing
    /// for `call_finished` to find.
    pub fn start_requested(&mut self, id: &str) -> StartVerdict {
        if let Concurrency::One = self.concurrency {
            if let Some(other) = self
                .entries
                .iter()
                .find(|(other, e)| other.as_str() != id && e.is_active())
                .map(|(other, _)| other.clone())
            {
                return StartVerdict::AnotherServerRunning { server_id: other };
            }
        }

        let entry = self.entries.entry(id.to_string()).or_default();
        // A start during a stop is a restart, not a duplicate: the desktop
        // lets it through and waits for the previous process to exit. Only an
        // already-live launch is a true duplicate.
        if entry.is_active() && !entry.stop_requested {
            // Counted anyway. The caller's `finally` will decrement whatever
            // it was told, and a duplicate that decremented without having
            // incremented would retire the *winner's* marker — dropping a
            // live launch out of the active set, which is the exact bug this
            // module exists to prevent.
            entry.calls += 1;
            return StartVerdict::AlreadyRunning;
        }

        entry.calls += 1;
        entry.generation += 1;
        entry.stop_requested = false;
        entry.state = State::Starting;
        StartVerdict::Proceed
    }

    /// A stop call arrived.
    ///
    /// Records the intent whatever the answer, so a launch still preparing
    /// gives up rather than finishing and starting a server nobody wants.
    /// Pair every verdict except [`StopVerdict::NotRunning`] with a
    /// [`Self::call_finished`].
    pub fn stop_requested(&mut self, id: &str) -> StopVerdict {
        let Some(entry) = self.entries.get_mut(id) else {
            return StopVerdict::NotRunning;
        };
        if !entry.is_active() {
            return StopVerdict::NotRunning;
        }

        entry.stop_requested = true;
        entry.generation += 1;
        entry.calls += 1;
        entry.state = State::Stopping;

        // How to stop is the core's call, not the host's: graceful only when
        // there is something that can hear it.
        match (entry.engine, entry.console) {
            (true, true) => StopVerdict::Graceful,
            (true, false) => StopVerdict::Terminate,
            (false, _) => StopVerdict::AbandonLaunch,
        }
    }

    /// A bridge call returned. Always in a `finally`/`defer`: an early return
    /// that skips this leaks a server into the active set for ever.
    pub fn call_finished(&mut self, id: &str) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.calls = entry.calls.saturating_sub(1);
            entry.prune();
        }
        self.entries.retain(|_, e| !e.is_idle());
    }

    /// A process now exists for this server.
    pub fn spawned(&mut self, id: &str) {
        let entry = self.entries.entry(id.to_string()).or_default();
        entry.engine = true;
        entry.console = false;
        entry.engine_generation = entry.generation;
    }

    /// The server reported that it is accepting connections.
    pub fn console_ready(&mut self, id: &str) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.console = true;
            if !entry.stop_requested {
                entry.state = State::Running;
            }
        }
    }

    /// The process is gone. Returns what that meant.
    ///
    /// The verdict comes from whether a stop was *requested*, never from a
    /// state a still-running launch can overwrite, and never from the exit
    /// code alone — a Minecraft server exits 0 on `stop` and often 0 after a
    /// fatal error too, and a stop carried out by terminating a starting
    /// server exits 143.
    pub fn exited(&mut self, id: &str, exit_code: i32) -> Exit {
        let Some(entry) = self.entries.get_mut(id) else {
            return Exit {
                state: exit_state(false, exit_code),
                intentional: false,
                superseded: false,
            };
        };

        // A stop bumps the generation and a restart bumps it again, so a
        // process that outlives both belongs to neither. `stop_requested`
        // being clear is what distinguishes "a newer launch has taken over"
        // from "this is the stop we asked for, still in progress".
        let superseded = entry.engine_generation != entry.generation && !entry.stop_requested;
        let intentional = entry.stop_requested || entry.state == State::Stopping;
        let state = exit_state(intentional, exit_code);

        // The process is gone either way, and with it its console.
        entry.engine = false;
        entry.console = false;

        if superseded {
            // Everything else belongs to the launch that replaced it. Writing
            // `stopped` here would flip a server that is coming up right now.
            return Exit {
                state,
                intentional,
                superseded,
            };
        }

        entry.stop_requested = false;
        entry.state = state;
        entry.prune();
        if entry.is_idle() {
            self.entries.remove(id);
        }

        Exit {
            state,
            intentional,
            superseded,
        }
    }

    /// A launch gave up before spawning anything.
    pub fn abandoned(&mut self, id: &str) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.engine = false;
            entry.stop_requested = false;
            entry.state = State::Stopped;
            entry.prune();
        }
    }
}

impl Entry {
    /// This server is this device's: a live engine, a call in flight, or a
    /// state that is neither stopped nor finished.
    fn is_active(&self) -> bool {
        self.engine
            || self.calls > 0
            || matches!(
                self.state,
                State::Starting | State::Running | State::Stopping
            )
    }

    /// Nothing left to remember.
    fn is_idle(&self) -> bool {
        !self.engine && self.calls == 0 && !self.stop_requested
    }

    /// A stop intent cannot outlive the thing it was aimed at.
    fn prune(&mut self) {
        if !self.engine && self.calls == 0 {
            self.stop_requested = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one() -> Lifecycle {
        Lifecycle::new(Concurrency::One)
    }

    // ── The two bugs this module exists to make impossible ──────────────────

    /// A launch long enough to restore a world is still this device's.
    ///
    /// The failure this pins: the id vanished from the active list for the
    /// minutes a restore took, the reconcile loop read that as a start from
    /// another device, and the `force_link_up` it issued regenerated the
    /// gateway's keys under a launch that had already resolved its tunnel
    /// config. The tunnel came up, handshook, and carried nothing.
    #[test]
    fn a_server_is_active_from_the_moment_a_start_arrives() {
        let mut life = one();
        assert_eq!(life.start_requested("s"), StartVerdict::Proceed);

        // Before the jar, before the restore, before anything is spawned.
        assert_eq!(life.active_ids(), vec!["s".to_string()]);
        assert!(life.running_ids().is_empty(), "not running — but active");

        life.call_finished("s");
        life.spawned("s");
        assert_eq!(life.active_ids(), vec!["s".to_string()]);
    }

    /// A stopping server is still this device's until the process is gone.
    ///
    /// The dashboard PATCHes `stopped` only after the stop call returns, so
    /// for the whole graceful shutdown the API still reads `running`. A host
    /// that reports itself idle in that window gets the server it just
    /// stopped restarted underneath it.
    #[test]
    fn a_server_stays_active_for_the_whole_of_a_graceful_stop() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s");
        life.console_ready("s");

        assert_eq!(life.stop_requested("s"), StopVerdict::Graceful);
        // The world is saving. This is the window that restarted the server.
        assert_eq!(life.active_ids(), vec!["s".to_string()]);
        assert!(life.running_ids().is_empty());

        let exit = life.exited("s", 0);
        assert_eq!(exit.state, State::Stopped);
        assert!(exit.intentional);
        life.call_finished("s");
        assert!(life.active_ids().is_empty(), "gone once the process is");
    }

    // ── Stop during a launch ────────────────────────────────────────────────

    /// Answering "not running" to a stop that arrives before the JVM exists
    /// left the launch to finish and start a server nobody wanted.
    #[test]
    fn a_stop_before_the_engine_exists_abandons_the_launch() {
        let mut life = one();
        life.start_requested("s");
        assert_eq!(life.stop_requested("s"), StopVerdict::AbandonLaunch);
        assert!(life.should_abandon("s"));

        life.abandoned("s");
        life.call_finished("s");
        life.call_finished("s");
        assert!(life.active_ids().is_empty());
    }

    /// A stop is carried out at once, however far the launch has got.
    ///
    /// The host used to wait for a starting server to reach its console before
    /// stopping it, so the world could be saved gracefully. In practice the
    /// user pressed Stop and watched the server carry on booting for up to two
    /// minutes. There is nothing to save yet at that point, so there is
    /// nothing to wait for.
    #[test]
    fn a_stop_is_never_deferred_until_a_server_has_finished_starting() {
        let mut life = one();
        life.start_requested("s");
        life.spawned("s");

        // Mid-terrain-generation: no console, so nothing can hear `stop`.
        assert_eq!(life.stop_requested("s"), StopVerdict::Terminate);

        // And once it can hear, it is asked politely instead.
        let mut life = one();
        life.start_requested("s");
        life.spawned("s");
        life.console_ready("s");
        assert_eq!(life.stop_requested("s"), StopVerdict::Graceful);
    }

    /// The wait that keeps a restart from running two servers at once.
    ///
    /// Found on a device, not by reading: the admission rule ("a start during
    /// a stop is a restart") was ported without it, and two launches ran
    /// concurrently against one server directory.
    #[test]
    fn a_restart_waits_for_the_outgoing_engine_before_spawning() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s");
        life.console_ready("s");
        life.stop_requested("s");
        life.call_finished("s");

        // The old JVM is still saving. A start is allowed — and must wait.
        assert_eq!(life.start_requested("s"), StartVerdict::Proceed);
        assert!(life.await_previous_exit("s"));

        // Once it is gone, there is nothing to wait for.
        life.exited("s", 0);
        assert!(!life.await_previous_exit("s"));
    }

    /// A first-ever launch waits for nothing.
    #[test]
    fn a_cold_start_does_not_wait() {
        let mut life = one();
        life.start_requested("s");
        assert!(!life.await_previous_exit("s"));
        assert!(
            life.supersedes_on_stop_backup("s"),
            "and it still supersedes any backup of this server"
        );
    }

    /// A relaunch's engine starts without a console, even though the previous
    /// one had reached its own — otherwise the first stop after a restart
    /// would talk to a console that does not exist yet.
    #[test]
    fn a_fresh_engine_starts_without_a_console() {
        let mut life = one();
        life.start_requested("s");
        life.spawned("s");
        life.console_ready("s");
        life.stop_requested("s");
        life.exited("s", 0);
        life.call_finished("s");
        life.call_finished("s");

        life.start_requested("s");
        life.spawned("s");
        assert_eq!(life.stop_requested("s"), StopVerdict::Terminate);
    }

    /// The exit that was reported as a crash: a stop asked for during startup
    /// is carried out by terminating the JVM, which exits 143. Reported as a
    /// crash, the host skips the on-stop backup and the session's play is
    /// lost.
    #[test]
    fn a_stop_carried_out_by_termination_is_not_a_crash() {
        let mut life = one();
        life.start_requested("s");
        life.spawned("s");
        life.stop_requested("s");

        let exit = life.exited("s", 143);
        assert_eq!(exit.state, State::Stopped);
        assert!(exit.intentional, "the user asked for this");
    }

    /// The announcement that went missing: the server was up, the tunnel was
    /// up, and the app never heard, because the core had already advanced to
    /// `Running` at console-ready and read the host's later announcement as a
    /// no-op.
    #[test]
    fn a_running_server_may_still_be_announced_after_its_tunnel_is_up() {
        let mut life = one();
        life.start_requested("s");
        life.spawned("s");
        life.console_ready("s");
        assert!(
            life.may_announce("s", State::Running),
            "the host announces running after the tunnel, not at console-ready"
        );
    }

    /// A launch still catching up must not announce `running` for a server
    /// already on its way down.
    #[test]
    fn a_stopping_server_is_never_announced_running() {
        let mut life = one();
        life.start_requested("s");
        life.spawned("s");
        life.stop_requested("s");

        assert!(!life.may_announce("s", State::Running));
        life.console_ready("s");
        assert_ne!(life.state("s"), State::Running);
    }

    // ── Admission ───────────────────────────────────────────────────────────

    #[test]
    fn a_second_start_for_the_same_server_is_not_an_error() {
        let mut life = one();
        assert_eq!(life.start_requested("s"), StartVerdict::Proceed);
        assert_eq!(life.start_requested("s"), StartVerdict::AlreadyRunning);
    }

    #[test]
    fn one_server_at_a_time_names_the_one_in_the_way() {
        let mut life = one();
        life.start_requested("first");
        assert_eq!(
            life.start_requested("second"),
            StartVerdict::AnotherServerRunning {
                server_id: "first".to_string()
            }
        );
    }

    #[test]
    fn many_hosts_run_several_at_once() {
        let mut life = Lifecycle::new(Concurrency::Many);
        assert_eq!(life.start_requested("a"), StartVerdict::Proceed);
        assert_eq!(life.start_requested("b"), StartVerdict::Proceed);
        assert_eq!(life.active_ids().len(), 2);
    }

    /// Start pressed right after Stop is a restart, which the desktop allows.
    #[test]
    fn a_start_during_a_stop_is_a_restart_not_a_duplicate() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s");
        life.console_ready("s");
        life.stop_requested("s");

        assert_eq!(life.start_requested("s"), StartVerdict::Proceed);
    }

    // ── The counter ─────────────────────────────────────────────────────────

    /// The reconcile loop issuing its own start while the user's is running is
    /// routine. The loser returns first, and its bookkeeping must not clear a
    /// marker the winner still needs.
    #[test]
    fn a_losing_concurrent_call_does_not_clear_the_winners_marker() {
        let mut life = one();
        life.start_requested("s"); // the user's, still preparing
        assert_eq!(life.start_requested("s"), StartVerdict::AlreadyRunning);

        // The duplicate's `finally` runs. Note it never incremented, so this
        // is the one place a stray decrement would be visible.
        life.call_finished("s");
        assert_eq!(
            life.active_ids(),
            vec!["s".to_string()],
            "the real launch is still in flight"
        );
    }

    #[test]
    fn nothing_is_remembered_once_a_server_is_fully_gone() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s");
        life.console_ready("s");
        life.stop_requested("s");
        life.exited("s", 0);
        life.call_finished("s");

        assert!(life.active_ids().is_empty());
        assert_eq!(life.state("s"), State::Stopped);
    }

    // ── Superseded launches ─────────────────────────────────────────────────

    /// Stop, then Start again before the old JVM has finished dying — the
    /// world save can take a minute, and players do not wait.
    ///
    /// The old process's exit must not be mistaken for the new launch's, or a
    /// server that is coming up right now flips to stopped and the UI reports
    /// a server that is starting as dead.
    #[test]
    fn an_exit_from_a_superseded_launch_is_flagged_and_changes_nothing() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s"); // this generation owns the process
        life.console_ready("s");

        life.stop_requested("s");
        life.call_finished("s");

        // Restart while the old JVM is still saving its world.
        assert_eq!(life.start_requested("s"), StartVerdict::Proceed);

        let exit = life.exited("s", 0);
        assert!(exit.superseded, "this exit belongs to the previous launch");
        assert_eq!(
            life.state("s"),
            State::Starting,
            "the new launch's state must survive the old one's exit"
        );
        assert_eq!(
            life.active_ids(),
            vec!["s".to_string()],
            "and the server is still this device's"
        );
    }

    /// What the restarting host is waiting on. The new launch parks in its
    /// `awaitPreviousExit` step until the outgoing engine is gone, and the
    /// superseded exit is the only thing that will say so — the early return
    /// that keeps the new launch's *state* must not also keep its engine.
    #[test]
    fn a_superseded_exit_still_frees_the_new_launch_to_spawn() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s");
        life.console_ready("s");

        life.stop_requested("s");
        life.call_finished("s");
        life.start_requested("s");
        assert!(
            life.await_previous_exit("s"),
            "the outgoing engine is still there, so the restart must wait"
        );

        assert!(life.exited("s", 0).superseded);
        assert!(
            !life.await_previous_exit("s"),
            "and once it is gone the restart may spawn"
        );
    }

    /// Why every host has to *force* its final announcement.
    ///
    /// A run that ends with nothing else in flight prunes its entry — that is
    /// what stops a finished server being counted active for ever — and from
    /// no entry at all the core reads "it stopped" as not news. Asking here
    /// rather than forcing is how a card sits at `stopping` until something
    /// else corrects it, and nothing else does. [`Exit::superseded`] is how an
    /// exit says it must not be announced; this is not that.
    #[test]
    fn a_pruned_exit_will_not_answer_for_its_own_stopped() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s"); // the start call has returned; nothing in flight
        life.spawned("s");
        life.console_ready("s");

        // The engine goes on its own — no stop call holding the entry open.
        let exit = life.exited("s", 0);
        assert!(!exit.superseded, "nothing replaced this run");
        assert!(
            !life.may_announce("s", State::Stopped),
            "the entry is gone, so the core cannot vouch for this — the host must force it"
        );
    }

    #[test]
    fn an_ordinary_exit_is_not_superseded() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s");
        life.console_ready("s");

        let exit = life.exited("s", 1);
        assert!(!exit.superseded);
        assert_eq!(exit.state, State::Crashed);
    }

    // ── Crashes ─────────────────────────────────────────────────────────────

    #[test]
    fn an_unasked_for_failure_is_a_crash() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s");
        life.console_ready("s");

        let exit = life.exited("s", 1);
        assert_eq!(exit.state, State::Crashed);
        assert!(!exit.intentional, "no backup for a half-saved world");
    }

    #[test]
    fn a_crash_clears_the_server_from_the_active_set() {
        let mut life = one();
        life.start_requested("s");
        life.call_finished("s");
        life.spawned("s");
        life.console_ready("s");
        life.exited("s", 1);

        assert!(life.active_ids().is_empty());
    }

    // ── Opaque state across the FFI ─────────────────────────────────────────

    /// A host on the far side of a C ABI holds this as bytes and hands it
    /// back, exactly as it does with `HandshakeWatch`.
    #[test]
    fn it_survives_a_round_trip_through_json() {
        let mut life = one();
        life.start_requested("s");
        life.spawned("s");
        life.stop_requested("s");

        let wire = serde_json::to_string(&life).unwrap();
        let back: Lifecycle = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, life);
        assert_eq!(back.active_ids(), vec!["s".to_string()]);
        assert!(back.should_abandon("s"));
    }
}

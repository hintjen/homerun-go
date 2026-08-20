//! The order a launch happens in.
//!
//! # Why an order needs owning
//!
//! [`crate::lifecycle`] settled *who owns a server* and *what an exit meant*.
//! This settles the other half: the sequence a launch runs, and which points
//! in it are safe to abandon at. Both hosts had that sequence written out
//! longhand, and the order is not arbitrary — several steps are load-bearing
//! in ways that are invisible until they are wrong:
//!
//!  - **[`Step::EnsureJar`] precedes [`Step::RestoreWorld`].** A restore
//!    decides what to do by asking whether a world is on disk, so it must not
//!    run before the directory has been populated. Getting this backwards
//!    reads as "no local world" on a device that has one.
//!  - **[`Step::RestoreWorld`] precedes [`Step::WriteSettings`].** Settings
//!    are written into files inside the server directory; a restore that
//!    landed afterwards would overwrite them with the snapshot's copies. The
//!    constraint holds for a host that configures a *linked* engine rather
//!    than writing files: a restore can bring another device's engine config
//!    into the directory, and it is loaded after this point.
//!  - **[`Step::OpenTunnel`] precedes [`Step::AnnounceRunning`].** A server
//!    accepting connections on loopback is not a server anyone can join.
//!    Announcing first makes the API mark the service healthy and the UI show
//!    a green card for a server no player can reach — the desktop learned this
//!    the same way.
//!  - **[`Step::AwaitPreviousExit`] precedes everything that writes the server
//!    directory.** A restart is admitted while the outgoing JVM is still
//!    saving its world. The desktop waits immediately before spawning, which
//!    suffices for it — but a mobile launch *restores a world* first, and
//!    writing that directory while the previous process still holds it is the
//!    corruption the wait exists to prevent. So it sits ahead of
//!    [`Step::RestoreWorld`], not merely ahead of [`Step::Spawn`].
//!
//!    Found by making a host walk this plan: the Android launch already waited
//!    in the right place, and the plan disagreed with it.
//!
//! # What this is not
//!
//! Not an executor. It returns a list; the host runs it, because every step is
//! something only a platform can do — unpack a runtime, download a jar, spawn
//! a process. What the host stops doing is *deciding what comes next*.

use serde::{Deserialize, Serialize};

/// How this host gets an engine.
///
/// The one thing about a host that changes which steps *exist*, rather than
/// which are configured. A host that links its engine has no jar to fetch and
/// no `Main-Class` to read — those steps are not merely skipped, they have no
/// meaning for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Engine {
    /// A separate process: the desktop and Android, which run a JVM.
    #[default]
    Spawned,
    /// Compiled in: iOS, which cannot spawn anything.
    Linked,
}

/// What a launch needs to know before it can be planned.
///
/// Deliberately narrow: the plan turns on what is *present*, not on what
/// anything is set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inputs {
    /// This server has a backup repository and this device is registered.
    pub backups: bool,
    /// The API returned environment variables to write into the world's files.
    pub settings: bool,
    /// This host can build a tunnel — false leaves a loopback-only server,
    /// which is what a desktop with no gateway link has.
    pub tunnel: bool,
    /// Spawned or linked. Defaults to spawned, which is what every host that
    /// does not say otherwise is.
    #[serde(default)]
    pub engine: Engine,
    /// This server runs on a JVM, so it has a jar to fetch and a `Main-Class`
    /// to read. `None` means the host did not say, and the answer is inferred
    /// from [`Inputs::engine`] — see [`plan`].
    #[serde(default)]
    pub needs_jvm: Option<bool>,
}

/// One thing a host does, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    /// Cancel an on-stop backup of this server that is still running.
    CancelOnStopBackup,
    /// Announce `starting`, so the console exists before the slow work — a
    /// download is minutes, and its progress lines are all the UI can show.
    AnnounceStarting,
    /// Begin resolving the tunnel, *without waiting*. The gateway provisions
    /// asynchronously and the poll runs up to a minute, so it overlaps the
    /// download and world generation instead of preceding them.
    BeginResolveTunnel,
    /// Unpack the bundled runtime if it is not already unpacked.
    EnsureRuntime,
    /// Download and verify the server jar.
    EnsureJar,
    /// Write `eula=true`, on every launch. There is no acceptance step
    /// anywhere in the product and the server will not boot without it.
    AcceptEula,
    /// Read the jar's `Main-Class`.
    ResolveMainClass,
    /// Restore the world if another device holds a newer snapshot.
    RestoreWorld,
    /// Apply the API's settings to the server about to start.
    ///
    /// Writing `server.properties` and friends for a host that spawns a
    /// process; configuring the engine in memory for one that links it. Same
    /// step, because the ordering that makes it correct is the same either
    /// way — after the world is in place, before the server is constructed.
    WriteSettings,
    /// Wait out a previous engine that has not finished exiting.
    AwaitPreviousExit,
    /// Start the process.
    Spawn,
    /// Wait for the console to report it is accepting connections.
    AwaitConsole,
    /// Await the tunnel begun above and bring wireproxy up. Fatal: a server
    /// nobody can reach is not a working server, so a failure here stops it.
    OpenTunnel,
    /// Only now.
    AnnounceRunning,
}

impl Step {
    /// True when a stop that arrived during the launch must be honoured
    /// *before* this step rather than after it.
    ///
    /// Every step that is expensive or irreversible is a checkpoint. The two
    /// that matter most are [`Step::Spawn`] — nothing exists yet, so
    /// abandoning is free — and [`Step::OpenTunnel`], which is the first point
    /// after a process exists, so a stop landing mid-boot is carried out
    /// against something rather than forgotten.
    pub fn is_checkpoint(self) -> bool {
        matches!(
            self,
            Step::RestoreWorld | Step::Spawn | Step::OpenTunnel | Step::AnnounceRunning
        )
    }
}

/// The steps this launch runs, in order.
pub fn plan(inputs: Inputs) -> Vec<Step> {
    let mut steps = vec![Step::CancelOnStopBackup, Step::AnnounceStarting];

    // Started early, awaited late — see the module docs.
    if inputs.tunnel {
        steps.push(Step::BeginResolveTunnel);
    }

    // Whether there is a jar at all. The host answers from the game type,
    // which is the only thing that actually knows: Pumpkin *is* the server and
    // Bedrock is its own binary, so neither has a jar or a `Main-Class`.
    //
    // When the host does not say, this falls back to the engine — which is
    // what every host did before, and is exactly the inference that breaks
    // once Pumpkin is spawned rather than linked. A spawned Pumpkin is
    // `Engine::Spawned` and still has nothing to download; without the
    // explicit answer it would be sent to fetch a Mojang jar it cannot use.
    // The fallback stays because iOS still says nothing and must keep the
    // plan it has always had.
    let needs_jvm = inputs.needs_jvm.unwrap_or(inputs.engine == Engine::Spawned);

    // `EnsureRuntime` and `AcceptEula` are in every plan, and that is not an
    // oversight. Neither is about the jar: one unpacks a bundled payload, the
    // other writes `eula=true` into the server directory, which Android does
    // as a plain file write. A host running a Mojang-derived engine would
    // still need both however it deploys it, so gating them here would be
    // encoding "no jar implies not Mojang", which is not something either
    // input says.
    //
    // The two below are jar-shaped by definition, so they are the ones that
    // come out.
    steps.push(Step::EnsureRuntime);
    if needs_jvm {
        steps.push(Step::EnsureJar);
    }
    steps.push(Step::AcceptEula);
    if needs_jvm {
        steps.push(Step::ResolveMainClass);
    }

    // Before anything writes the server directory — the outgoing JVM may
    // still be saving into it.
    steps.push(Step::AwaitPreviousExit);

    // After the jar, because the restore decision asks whether a world is on
    // disk; before the settings, because a restore would overwrite them.
    if inputs.backups {
        steps.push(Step::RestoreWorld);
    }
    if inputs.settings {
        steps.push(Step::WriteSettings);
    }

    steps.extend([Step::Spawn, Step::AwaitConsole]);

    if inputs.tunnel {
        steps.push(Step::OpenTunnel);
    }

    steps.push(Step::AnnounceRunning);
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything on, and spawned — the desktop and Android. Kept as the
    /// default subject so every assertion below means what it did before
    /// `engine` existed.
    fn full() -> Inputs {
        Inputs {
            backups: true,
            settings: true,
            tunnel: true,
            engine: Engine::Spawned,
            needs_jvm: None,
        }
    }

    fn index(steps: &[Step], step: Step) -> usize {
        steps.iter().position(|s| *s == step).expect("step missing")
    }

    /// The restore asks whether a world is on disk, so the directory has to be
    /// populated first. Reversed, a device with a world reports having none
    /// and pulls the whole thing down again.
    #[test]
    fn the_jar_lands_before_the_world_is_restored() {
        let steps = plan(full());
        assert!(index(&steps, Step::EnsureJar) < index(&steps, Step::RestoreWorld));
    }

    /// Settings are files inside the server directory; a restore afterwards
    /// would replace them with the snapshot's.
    #[test]
    fn the_world_is_restored_before_settings_are_written() {
        let steps = plan(full());
        assert!(index(&steps, Step::RestoreWorld) < index(&steps, Step::WriteSettings));
    }

    /// Both must be finished before anything reads them.
    #[test]
    fn nothing_is_spawned_until_the_directory_is_final() {
        let steps = plan(full());
        let spawn = index(&steps, Step::Spawn);
        assert!(index(&steps, Step::RestoreWorld) < spawn);
        assert!(index(&steps, Step::WriteSettings) < spawn);
    }

    /// A server accepting connections on loopback is not one anyone can join.
    /// Announcing first shows a green card for a server no player can reach.
    #[test]
    fn running_is_announced_only_after_the_tunnel_is_up() {
        let steps = plan(full());
        assert!(index(&steps, Step::OpenTunnel) < index(&steps, Step::AnnounceRunning));
        assert_eq!(*steps.last().unwrap(), Step::AnnounceRunning);
    }

    /// The tunnel resolve is started early and awaited late, so a gateway that
    /// takes a minute to provision costs nothing.
    #[test]
    fn the_tunnel_is_resolved_alongside_the_download_not_before_it() {
        let steps = plan(full());
        let begin = index(&steps, Step::BeginResolveTunnel);
        assert!(begin < index(&steps, Step::EnsureJar));
        assert!(begin < index(&steps, Step::OpenTunnel));
    }

    /// Waited out before anything writes the directory, not merely before
    /// spawning: a restore into a world the previous process still holds is
    /// the corruption this exists to prevent.
    #[test]
    fn the_previous_engine_is_awaited_before_the_directory_is_written() {
        let steps = plan(full());
        let wait = index(&steps, Step::AwaitPreviousExit);
        assert!(wait < index(&steps, Step::RestoreWorld));
        assert!(wait < index(&steps, Step::WriteSettings));
        assert!(wait < index(&steps, Step::Spawn));
    }

    /// The regression that arrives with a spawned Pumpkin.
    ///
    /// It is `Engine::Spawned` like every JVM server, and it has no jar and no
    /// `Main-Class` — so inferring the jar steps from the engine sends it to
    /// download a Mojang server it cannot run. `EnsureRuntime` and `AcceptEula`
    /// stay, because neither is about the jar.
    #[test]
    fn a_spawned_server_with_no_jar_fetches_nothing() {
        let steps = plan(Inputs {
            needs_jvm: Some(false),
            ..full()
        });
        assert!(!steps.contains(&Step::EnsureJar));
        assert!(!steps.contains(&Step::ResolveMainClass));
        assert!(steps.contains(&Step::EnsureRuntime));
        assert!(steps.contains(&Step::AcceptEula));
        assert!(steps.contains(&Step::WriteSettings));
    }

    /// A host that says nothing keeps the plan it has always had. iOS says
    /// nothing, and every server on it would start fetching a jar if this
    /// fallback were dropped.
    #[test]
    fn an_unanswered_launch_keeps_the_plan_the_engine_implied() {
        let linked = plan(Inputs {
            engine: Engine::Linked,
            needs_jvm: None,
            ..full()
        });
        assert!(!linked.contains(&Step::EnsureJar));
        assert!(!linked.contains(&Step::ResolveMainClass));

        let spawned = plan(Inputs {
            engine: Engine::Spawned,
            needs_jvm: None,
            ..full()
        });
        assert!(spawned.contains(&Step::EnsureJar));
        assert!(spawned.contains(&Step::ResolveMainClass));
    }

    /// And an explicit answer overrules the inference in both directions, so a
    /// linked engine that *did* have a jar would still be planned correctly.
    #[test]
    fn an_explicit_answer_wins_over_the_engine() {
        let steps = plan(Inputs {
            engine: Engine::Linked,
            needs_jvm: Some(true),
            ..full()
        });
        assert!(steps.contains(&Step::EnsureJar));
        assert!(steps.contains(&Step::ResolveMainClass));
    }

    /// A host without backups, settings or a tunnel still has a valid launch —
    /// it just does less. This is the desktop's loopback-only case.
    #[test]
    fn the_optional_steps_drop_out_cleanly() {
        let steps = plan(Inputs {
            backups: false,
            settings: false,
            tunnel: false,
            engine: Engine::Spawned,
            needs_jvm: None,
        });
        assert!(!steps.contains(&Step::RestoreWorld));
        assert!(!steps.contains(&Step::WriteSettings));
        assert!(!steps.contains(&Step::BeginResolveTunnel));
        assert!(!steps.contains(&Step::OpenTunnel));
        // And the spine is unchanged.
        assert!(index(&steps, Step::EnsureJar) < index(&steps, Step::Spawn));
        assert_eq!(*steps.last().unwrap(), Step::AnnounceRunning);
    }

    /// The cheapest place to notice a stop is before anything was spawned.
    #[test]
    fn a_stop_is_checked_before_anything_irreversible() {
        assert!(Step::Spawn.is_checkpoint());
        assert!(Step::OpenTunnel.is_checkpoint());
        assert!(Step::RestoreWorld.is_checkpoint());
        assert!(!Step::AcceptEula.is_checkpoint());
    }

    /// A plan with no steps, or one that could spawn twice, is a bug.
    #[test]
    fn a_plan_spawns_exactly_once() {
        for backups in [true, false] {
            for settings in [true, false] {
                for tunnel in [true, false] {
                    for engine in [Engine::Spawned, Engine::Linked] {
                        let steps = plan(Inputs {
                            backups,
                            settings,
                            tunnel,
                            engine,
                            needs_jvm: None,
                        });
                        assert_eq!(
                            steps.iter().filter(|s| **s == Step::Spawn).count(),
                            1,
                            "{backups} {settings} {tunnel} {engine:?}"
                        );
                    }
                }
            }
        }
    }

    /// A linked host loses the two steps that are about a jar, and nothing
    /// else.
    ///
    /// The pair left in deliberately are `EnsureRuntime` and `AcceptEula`:
    /// neither is about the jar, and a linked host running a Mojang-derived
    /// engine would still need both. iOS skips them by not asking, which
    /// `LaunchOrder` permits — that is a host's business, not the plan's.
    #[test]
    fn a_linked_engine_loses_the_jar_steps_and_keeps_the_rest() {
        let linked = plan(Inputs {
            engine: Engine::Linked,
            ..full()
        });

        assert!(!linked.contains(&Step::EnsureJar));
        assert!(!linked.contains(&Step::ResolveMainClass));
        assert!(linked.contains(&Step::EnsureRuntime));
        assert!(linked.contains(&Step::AcceptEula));

        // The spine is untouched: everything the order guarantees still holds.
        assert!(index(&linked, Step::AwaitPreviousExit) < index(&linked, Step::RestoreWorld));
        assert!(index(&linked, Step::RestoreWorld) < index(&linked, Step::WriteSettings));
        assert!(index(&linked, Step::WriteSettings) < index(&linked, Step::Spawn));
        assert!(index(&linked, Step::OpenTunnel) < index(&linked, Step::AnnounceRunning));
        assert_eq!(*linked.last().unwrap(), Step::AnnounceRunning);

        // And it is exactly two steps shorter than the spawned plan, so a
        // future step added to one is added to both.
        assert_eq!(linked.len() + 2, plan(full()).len());
    }

    /// The default is `Spawned`, and that is load-bearing rather than tidy:
    /// Android's launch order *throws* on a step missing from the plan, so a
    /// host that omits `engine` must get the plan it has always had.
    #[test]
    fn omitting_the_engine_means_spawned() {
        let implied: Inputs =
            serde_json::from_str(r#"{"backups":true,"settings":true,"tunnel":true}"#)
                .expect("inputs without an engine should deserialize");
        assert_eq!(implied.engine, Engine::Spawned);
        assert_eq!(plan(implied), plan(full()));
    }
}

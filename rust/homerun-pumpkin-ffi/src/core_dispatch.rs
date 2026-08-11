//! Dispatch for `homerun-core`, with no platform in it.
//!
//! # One entry point on purpose
//!
//! Every call goes through `call(method, argsJson)` and comes back as
//! `{"ok":true,"value":…}` or `{"ok":false,"error":"…"}`. A dozen exported
//! symbols would be faster by a few microseconds and would create a dozen
//! places for two languages to disagree about argument order. This way each
//! host has one parse path, adding a call is one match arm, and an unknown
//! method is a runtime error naming itself rather than a link error naming
//! nothing.
//!
//! The cost is real but small: a JSON round trip per call. The busiest caller
//! is console-line parsing at a few hundred lines a second during world
//! generation, which is nothing next to the server producing them.
//!
//! # Why this is separate from the marshalling
//!
//! This module knows nothing about JNI or the C ABI. Android reaches it
//! through [`crate::core_bridge`] and iOS through
//! [`crate::homerun_core_call`], and both wrap the *same* function — so the
//! two platforms cannot drift in what a method means, only in how a string
//! crosses the boundary.
//!
//! It also makes dispatch testable on the host. The tests at the bottom run
//! under `cargo test` on any machine, with no device and no emulator.
//!
//! # Panics must not escape
//!
//! A panic unwinding through JNI aborts the VM, and through the C ABI is
//! undefined behaviour — on a phone either is the whole app. [`call`] runs
//! inside `catch_unwind`, so a panic becomes an ordinary error envelope naming
//! the method. Seeing one means a bug in this crate, not bad input.

use serde_json::{json, Value};

use homerun_core::game::Game as _;
use homerun_core::minecraft::{self, jar, jvm, settings};
use homerun_core::{backup, game, launch, lifecycle, link, metrics, properties, state, tunnel};

/// Dispatch one call and render the reply envelope.
///
/// Never panics and never fails: every outcome, including a panic, is a JSON
/// string the caller can hand straight back to its host language.
pub fn call(method: &str, args: &str) -> String {
    let result = std::panic::catch_unwind(|| dispatch(method, args)).unwrap_or_else(|_| {
        // Reaching here means a bug in this crate, not bad input. Say so
        // plainly rather than dressing it up as a user-facing failure.
        Err(format!("the native core panicked handling \"{method}\""))
    });

    match result {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(error) => json!({ "ok": false, "error": error }),
    }
    .to_string()
}

/// `[["key","value"], …]` — the shape `settings.properties` returns, kept as a
/// list rather than an object so the merge's append order survives the round
/// trip through Kotlin.
fn pairs(value: &Value, method: &str) -> Result<Vec<(String, String)>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("\"{method}\" needs a list of [key, value] pairs"))?
        .iter()
        .map(|entry| match entry.as_array().map(Vec::as_slice) {
            Some([k, v]) => match (k.as_str(), v.as_str()) {
                (Some(k), Some(v)) => Ok((k.to_string(), v.to_string())),
                _ => Err(format!("\"{method}\": a pair must be two strings")),
            },
            _ => Err(format!("\"{method}\": each entry must be [key, value]")),
        })
        .collect()
}

/// Fold `extra`'s keys into `into`. Both are always objects here — the callers
/// build them — so a non-object is a bug in this file, not bad input.
fn merge(into: &mut Value, extra: Value) {
    if let (Some(target), Value::Object(source)) = (into.as_object_mut(), extra) {
        target.extend(source);
    }
}

/// The caller's lifecycle state, or a fresh one.
///
/// `concurrency` is only consulted when there is nothing to resume from: it
/// belongs to the host's capabilities, not to a moment in time, and reading it
/// on every call would let a host silently change the rules mid-flight.
fn load_lifecycle(args: &Value) -> Result<lifecycle::Lifecycle, String> {
    match args.get("lifecycle") {
        Some(v) if !v.is_null() => {
            serde_json::from_value(v.clone()).map_err(|e| format!("bad lifecycle: {e}"))
        }
        _ => {
            let concurrency = match args.get("concurrency").and_then(|v| v.as_str()) {
                Some("many") => lifecycle::Concurrency::Many,
                // One is the safe default: a host that runs several servers
                // and forgets to say so gets a refusal it will notice, where
                // the reverse would be a second JVM on a phone.
                _ => lifecycle::Concurrency::One,
            };
            Ok(lifecycle::Lifecycle::new(concurrency))
        }
    }
}

/// The state to carry forward, plus the questions every caller asks next.
fn lifecycle_view(life: &lifecycle::Lifecycle, id: &str) -> Result<Value, String> {
    Ok(json!({
        "lifecycle": serde_json::to_value(life).map_err(|e| e.to_string())?,
        "activeIds": life.active_ids(),
        "runningIds": life.running_ids(),
        "state": serde_json::to_value(life.state(id)).map_err(|e| e.to_string())?,
        "shouldAbandon": life.should_abandon(id),
        "awaitPreviousExit": life.await_previous_exit(id),
        "supersedesOnStopBackup": life.supersedes_on_stop_backup(id),
    }))
}

/// The caller's perf history, or a fresh one.
///
/// Like `load_lifecycle`, the policy is only consulted when there is nothing to
/// resume from — it describes the graph a host wants, not a moment in time, and
/// re-reading it every call would let the retention rule change mid-session.
///
/// One history per **run**, not per server: a graph covers a session, so the
/// host starts a new one rather than carrying one across a restart.
fn load_history(args: &Value) -> Result<metrics::History, String> {
    match args.get("history") {
        Some(v) if !v.is_null() => {
            serde_json::from_value(v.clone()).map_err(|e| format!("bad history: {e}"))
        }
        _ => {
            let policy = match args.get("policy") {
                Some(v) if !v.is_null() => {
                    serde_json::from_value(v.clone()).map_err(|e| format!("bad policy: {e}"))?
                }
                // The desktop's numbers, so a phone's graph of a server and a
                // PC's graph of the same server cover the same span.
                _ => metrics::Policy::default(),
            };
            Ok(metrics::History::new(policy))
        }
    }
}

/// The game a call is about.
///
/// Defaults to Minecraft so existing callers keep working, but an unknown id
/// is an error rather than a silent fallback — a host asking about a game this
/// build cannot host should hear so, not get Minecraft's answer.
fn resolve_game(args: &Value) -> Result<&'static dyn game::Game, String> {
    let id = args
        .get("game")
        .and_then(|v| v.as_str())
        .unwrap_or(minecraft::Minecraft.id());
    game::by_id(id).ok_or_else(|| format!("this build cannot host \"{id}\""))
}

fn players(value: &Value, method: &str) -> Result<Vec<settings::Player>, String> {
    serde_json::from_value(value.clone())
        .map_err(|e| format!("\"{method}\" needs players as {{name, uuid}}: {e}"))
}

fn dispatch(method: &str, args: &str) -> Result<Value, String> {
    let args: Value = serde_json::from_str(args).map_err(|e| format!("bad arguments: {e}"))?;

    let field = |name: &str| -> Result<&Value, String> {
        args.get(name)
            .ok_or_else(|| format!("\"{method}\" needs a {name}"))
    };
    let text = |name: &str| -> Result<String, String> {
        field(name)?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("\"{method}\" needs {name} to be a string"))
    };
    let optional_text = |name: &str| -> Option<String> {
        args.get(name).and_then(|v| v.as_str()).map(str::to_string)
    };

    match method {
        // --- jars ---------------------------------------------------------
        "minecraft.jar.resolveVersion" => {
            jar::resolve_version(field("manifest")?, optional_text("version").as_deref())
                .map(Value::from)
                .map_err(|e| e.to_string())
        }

        "minecraft.jar.metadataUrl" => {
            jar::version_metadata_url(field("manifest")?, &text("version")?)
                .map(Value::from)
                .map_err(|e| e.to_string())
        }

        "minecraft.jar.vanilla" => jar::vanilla(field("metadata")?, &text("version")?)
            .and_then(|a| {
                serde_json::to_value(a).map_err(|e| homerun_core::Error::Malformed(e.to_string()))
            })
            .map_err(|e| e.to_string()),

        "minecraft.jar.paper" => {
            let required_java = args
                .get("requiredJava")
                .and_then(|v| v.as_u64())
                .unwrap_or(21) as u16;
            jar::paper(field("builds")?, &text("version")?, required_java)
                .and_then(|a| {
                    serde_json::to_value(a)
                        .map_err(|e| homerun_core::Error::Malformed(e.to_string()))
                })
                .map_err(|e| e.to_string())
        }

        "minecraft.jar.parseLoader" => jar::Loader::parse(optional_text("type").as_deref())
            .map(|l| Value::from(l.as_str()))
            .map_err(|e| e.to_string()),

        "minecraft.jar.checkJava" => {
            let artifact: jar::Artifact = serde_json::from_value(field("artifact")?.clone())
                .map_err(|e| format!("bad artifact: {e}"))?;
            let bundled = args
                .get("bundledJava")
                .and_then(|v| v.as_u64())
                .map(|v| v as u16);
            jar::check_java(&artifact, bundled)
                .map(|_| Value::Bool(true))
                .map_err(|e| e.to_string())
        }

        "minecraft.jar.satisfies" => {
            let on_disk: jar::OnDisk = serde_json::from_value(field("onDisk")?.clone())
                .map_err(|e| format!("bad on-disk record: {e}"))?;
            let artifact: jar::Artifact = serde_json::from_value(field("artifact")?.clone())
                .map_err(|e| format!("bad artifact: {e}"))?;
            Ok(Value::Bool(on_disk.satisfies(&artifact)))
        }

        // Whether the jar already in the server directory can be kept.
        //
        // Answers in two steps — `verify` asks for a digest the caller has not
        // paid for yet — because hashing 55 MB to answer a question a marker
        // file usually settles is the wrong default. Call once with no digest,
        // and again with one only if asked.
        "minecraft.jar.cacheDecision" => {
            let on_disk: Option<jar::OnDisk> = match args.get("onDisk") {
                Some(v) if !v.is_null() => Some(
                    serde_json::from_value(v.clone())
                        .map_err(|e| format!("bad on-disk record: {e}"))?,
                ),
                _ => None,
            };
            let artifact: jar::Artifact = serde_json::from_value(field("artifact")?.clone())
                .map_err(|e| format!("bad artifact: {e}"))?;
            let present = args
                .get("present")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let digest = optional_text("digest");
            let decision =
                jar::cache_decision(on_disk.as_ref(), present, digest.as_deref(), &artifact);
            serde_json::to_value(decision).map_err(|e| e.to_string())
        }

        // The shared cache's file name for an artifact, or null when it cannot
        // be cached. Null is a normal answer, not an error — a jar with no
        // published digest has nothing to be named after.
        "minecraft.jar.cacheKey" => {
            let artifact: jar::Artifact = serde_json::from_value(field("artifact")?.clone())
                .map_err(|e| format!("bad artifact: {e}"))?;
            Ok(match jar::cache_key(&artifact) {
                Some(name) => Value::String(name),
                None => Value::Null,
            })
        }

        // How long to wait before trying a download again, or null when there
        // is nothing left to try. Null is what ends the loop — a host reading
        // a missing delay as zero would hammer a dead endpoint.
        "minecraft.jar.retryDelay" => {
            let attempt = args.get("attempt").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            Ok(match jar::retry_delay_ms(attempt) {
                Some(ms) => Value::from(ms),
                None => Value::Null,
            })
        }

        "minecraft.jar.couldSatisfy" => {
            let on_disk: jar::OnDisk = serde_json::from_value(field("onDisk")?.clone())
                .map_err(|e| format!("bad on-disk record: {e}"))?;
            let loader = jar::Loader::parse(optional_text("loader").as_deref())
                .map_err(|e| e.to_string())?;
            Ok(Value::Bool(on_disk.could_satisfy(
                optional_text("version").as_deref(),
                loader,
            )))
        }

        // --- running the JVM ----------------------------------------------
        //
        // The portable half of a Java server's command line, how to stop one,
        // and how long to wait for it. A host adds its own platform flags
        // around this; what it stops doing is deciding any of the numbers.
        "minecraft.jvm.launch" => {
            let requested = args
                .get("memoryMb")
                .and_then(|v| v.as_u64())
                .unwrap_or(1024) as u32;
            // Absent means "no device ceiling", which is what a desktop has.
            let total = args
                .get("deviceTotalMb")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let heap = jvm::heap_mb(requested, total);
            Ok(json!({
                "heapMb": heap,
                "options": jvm::heap_options(heap),
                "programArgs": jvm::PROGRAM_ARGS,
                "eulaFile": jvm::EULA_FILE,
                "eulaContents": jvm::EULA_CONTENTS,
            }))
        }

        // Do this, wait this long, then climb. See `jvm::stop_ladder` for why
        // the first rung is not a terminate.
        "minecraft.jvm.stopLadder" => {
            let console = args
                .get("console")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(json!({
                "command": jvm::STOP_COMMAND,
                "rungs": serde_json::to_value(jvm::stop_ladder(console))
                    .map_err(|e| e.to_string())?,
            }))
        }

        "minecraft.jvm.limits" => {
            serde_json::to_value(jvm::Limits::default()).map_err(|e| e.to_string())
        }

        // One wording per refusal, so two apps turning down the same thing say
        // the same sentence. An unknown kind is an error rather than a shrug:
        // a host asking for wording that does not exist would otherwise show
        // the player an empty string.
        "minecraft.jvm.refusal" => {
            let kind = text("kind")?;
            let refusal: jvm::Refusal = serde_json::from_value(Value::String(kind.clone()))
                .map_err(|_| format!("no refusal called \"{kind}\""))?;
            Ok(Value::String(refusal.text().to_string()))
        }

        // --- the tunnel (game-agnostic) -----------------------------------
        //
        // The game names the forwards; this renders whatever it named. Nothing
        // here knows 25565 from 19132.
        "tunnel.render" => {
            let link: tunnel::Link = serde_json::from_value(field("link")?.clone())
                .map_err(|e| format!("bad link: {e}"))?;

            let forwards = match args.get("forwards") {
                // A host that already knows its forwards passes them straight
                // through — the fully game-neutral path.
                Some(explicit) => serde_json::from_value(explicit.clone())
                    .map_err(|e| format!("bad forwards: {e}"))?,
                // Otherwise ask the game, which is where the port numbers live.
                None => {
                    let game = resolve_game(&args)?;
                    let port = args.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                    game.forwards(
                        optional_text("exposure").as_deref().unwrap_or("java"),
                        port,
                        &args,
                    )
                    .map_err(|e| e.to_string())?
                }
            };

            Ok(Value::from(tunnel::Config { link, forwards }.render()))
        }

        // --- the game capability surface ----------------------------------
        //
        // These take a `game` id and route through the trait, so a host asks
        // "what does this line mean" and "what should I write" without knowing
        // which game answered.
        "game.list" => Ok(Value::Array(
            game::all().iter().map(|g| Value::from(g.id())).collect(),
        )),

        "game.classify" => {
            let meaning = resolve_game(&args)?.classify(&text("line")?);
            serde_json::to_value(meaning).map_err(|e| e.to_string())
        }

        "game.configInputs" => {
            let inputs = resolve_game(&args)?.config_inputs(field("env")?);
            serde_json::to_value(inputs).map_err(|e| e.to_string())
        }

        "game.requiredLookups" => {
            let lookups = resolve_game(&args)?.required_lookups(
                field("env")?,
                optional_text("gameType").as_deref().unwrap_or("java"),
            );
            serde_json::to_value(lookups).map_err(|e| e.to_string())
        }

        "game.configFiles" => {
            let ctx: game::BuildContext = serde_json::from_value(field("context")?.clone())
                .map_err(|e| format!("bad build context: {e}"))?;
            let files = resolve_game(&args)?
                .config_files(&ctx)
                .map_err(|e| e.to_string())?;
            serde_json::to_value(files).map_err(|e| e.to_string())
        }

        // --- backups (game-agnostic) --------------------------------------
        //
        // Decisions only. Nothing here runs an engine or touches a repository,
        // which is what lets a host that spawns a binary and a host that links
        // a library share the same answers.
        "backup.restoreDecision" => {
            let latest: Option<backup::Snapshot> = match args.get("latest") {
                Some(v) if !v.is_null() => Some(
                    serde_json::from_value(v.clone()).map_err(|e| format!("bad snapshot: {e}"))?,
                ),
                _ => None,
            };
            let decision = backup::restore_decision(
                optional_text("pinned").as_deref(),
                latest.as_ref(),
                &text("deviceId")?,
                args.get("hasLocalWorld")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            );
            serde_json::to_value(decision).map_err(|e| e.to_string())
        }

        "backup.leaseDecision" => {
            let decision = backup::lease_decision(
                optional_text("leaseDevice").as_deref(),
                &text("deviceId")?,
                args.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
            );
            serde_json::to_value(decision).map_err(|e| e.to_string())
        }

        "backup.shouldBackUp" => Ok(Value::Bool(backup::should_back_up(
            args.get("hasLocalWorld")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ))),

        "backup.classify" => {
            let failure = backup::classify(
                args.get("exitCode")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32),
                &text("message")?,
                optional_text("host").as_deref().unwrap_or_default(),
            );
            Ok(json!({
                "failure": serde_json::to_value(&failure).map_err(|e| e.to_string())?,
                "retryable": failure.is_retryable(),
                "succeeded": failure.succeeded(),
            }))
        }

        "backup.recordedBasename" => Ok(backup::recorded_basename(&text("path")?)
            .map(Value::from)
            .unwrap_or(Value::Null)),

        "backup.internalPath" => Ok(Value::from(backup::internal_path(&text("path")?))),

        "backup.stateReport" => {
            let operation = match optional_text("operation").as_deref().unwrap_or("backup") {
                "backup" => backup::Operation::Backup,
                "restore" => backup::Operation::Restore,
                other => return Err(format!("unknown backup operation \"{other}\"")),
            };
            let snapshot_id = optional_text("snapshotId");

            let report = match optional_text("error") {
                Some(error) => backup::StateReport::failed(operation, snapshot_id, error),
                None => backup::StateReport::complete(
                    operation,
                    snapshot_id,
                    args.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
                    args.get("durationSeconds")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                ),
            };
            Ok(json!({
                "body": serde_json::to_value(&report).map_err(|e| e.to_string())?,
                "releasesLease": report.releases_lease(),
            }))
        }

        // --- generic config-file merging ----------------------------------
        "properties.merge" => {
            let managed = pairs(field("managed")?, method)?;
            Ok(Value::from(properties::merge(
                optional_text("existing").unwrap_or_default().as_str(),
                &managed,
            )))
        }

        "link.fromServerBody" => Ok(match link::from_server_body(field("body")?) {
            Some(polled) => serde_json::to_value(polled).map_err(|e| e.to_string())?,
            None => Value::Null,
        }),

        "link.isUsable" => {
            let polled: link::PolledLink = serde_json::from_value(field("polled")?.clone())
                .map_err(|e| format!("bad polled link: {e}"))?;
            let before: Option<tunnel::Link> = match args.get("before") {
                Some(v) if !v.is_null() => Some(
                    serde_json::from_value(v.clone())
                        .map_err(|e| format!("bad prior link: {e}"))?,
                ),
                _ => None,
            };
            Ok(Value::Bool(link::is_usable(&polled, before.as_ref())))
        }

        // --- lifecycle ----------------------------------------------------
        "state.exit" => {
            let intentional = args
                .get("intentional")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let code = args.get("code").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
            Ok(Value::from(
                state::exit_state(intentional, code)
                    .wire()
                    .unwrap_or("stopped"),
            ))
        }

        "state.handshake" => {
            let mut watch: state::HandshakeWatch = match args.get("watch") {
                Some(v) if !v.is_null() => {
                    serde_json::from_value(v.clone()).map_err(|e| format!("bad watch: {e}"))?
                }
                _ => state::HandshakeWatch::new(),
            };
            let give_up = watch.observe(&text("line")?);
            Ok(json!({
                "watch": serde_json::to_value(&watch).map_err(|e| e.to_string())?,
                "giveUp": give_up,
                "recovered": watch.recovered(),
            }))
        }

        // --- the order a launch happens in --------------------------------
        //
        // Returns a list; the host runs it. Every step is something only a
        // platform can do, but *which comes next* is not, and both hosts had
        // the sequence written out longhand.
        "launch.plan" => {
            let inputs = launch::Inputs {
                backups: args
                    .get("backups")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                settings: args
                    .get("settings")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                tunnel: args
                    .get("tunnel")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                // Defaults to spawned, and that is load-bearing rather than
                // tidy. Android's launch order *throws* on a step missing from
                // the plan, where iOS's returns false — so a host that omits
                // this must get the plan it has always had, or it crashes at
                // `ensureJar` on the first start. Android sends no `engine`
                // key and needs no change.
                engine: match args.get("engine").and_then(|v| v.as_str()) {
                    Some("linked") => launch::Engine::Linked,
                    _ => launch::Engine::Spawned,
                },
            };
            let steps: Vec<Value> = launch::plan(inputs)
                .into_iter()
                .map(|step| {
                    json!({
                        "step": serde_json::to_value(step).unwrap_or(Value::Null),
                        "checkpoint": step.is_checkpoint(),
                    })
                })
                .collect();
            Ok(Value::Array(steps))
        }

        // --- who owns a server right now ----------------------------------
        //
        // Stateful, and the state lives with the caller: `lifecycle` goes in,
        // a new `lifecycle` comes back, exactly as `state.handshake` does with
        // its watch. Nothing is retained here, so there is no handle to leak
        // and no second copy to disagree with the host's.
        "lifecycle.apply" => {
            let mut life = load_lifecycle(&args)?;
            let id = text("serverId")?;
            let mut reply = json!({});

            match text("event")?.as_str() {
                "startRequested" => {
                    reply = serde_json::to_value(life.start_requested(&id))
                        .map_err(|e| e.to_string())?
                }
                "stopRequested" => {
                    reply =
                        serde_json::to_value(life.stop_requested(&id)).map_err(|e| e.to_string())?
                }
                "callFinished" => life.call_finished(&id),
                "spawned" => life.spawned(&id),
                "consoleReady" => life.console_ready(&id),
                "abandoned" => life.abandoned(&id),
                "exited" => {
                    let code = args.get("code").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
                    reply =
                        serde_json::to_value(life.exited(&id, code)).map_err(|e| e.to_string())?
                }
                other => return Err(format!("\"{method}\": unknown event \"{other}\"")),
            }

            // The answer plus the queries a caller always wants next, in one
            // round trip. A host that had to ask separately could act on a
            // list from before its own event landed.
            merge(&mut reply, lifecycle_view(&life, &id)?);
            Ok(reply)
        }

        // --- what a run is costing ----------------------------------------
        //
        // Same shape as `lifecycle.*`: state in, state out, nothing retained
        // here. A host reads counters — resident bytes, cumulative CPU seconds
        // — and this decides what they mean and how much to keep. It never
        // takes a percentage from a host, because a percentage is a difference
        // between two moments and that is where wrong graphs come from.
        "metrics.record" => {
            let mut history = load_history(&args)?;
            let reading: metrics::Reading = serde_json::from_value(field("reading")?.clone())
                .map_err(|e| format!("bad reading: {e}"))?;
            let appended = history.record(reading);
            Ok(json!({
                "history": serde_json::to_value(&history).map_err(|e| e.to_string())?,
                "appended": appended,
                // Re-read every time: it doubles when the buffer fills, and a
                // host still scheduling on the original keeps sampling at a
                // resolution this has stopped keeping.
                "intervalMs": history.interval_ms(),
            }))
        }

        "metrics.query" => {
            let history = load_history(&args)?;
            let mut view = json!({
                "samples": serde_json::to_value(history.samples()).map_err(|e| e.to_string())?,
                "intervalMs": history.interval_ms(),
            });
            // Answered only when a clock is offered, so a host can ask whether
            // a reading is worth taking before it pays to read /proc.
            if let Some(now) = args.get("nowMs").and_then(|v| v.as_i64()) {
                merge(&mut view, json!({ "due": history.due(now) }));
            }
            Ok(view)
        }

        "lifecycle.query" => {
            let life = load_lifecycle(&args)?;
            let id = optional_text("serverId").unwrap_or_default();
            let mut view = lifecycle_view(&life, &id)?;
            if let Some(raw) = args.get("state") {
                let state: state::State =
                    serde_json::from_value(raw.clone()).map_err(|e| format!("bad state: {e}"))?;
                merge(
                    &mut view,
                    json!({ "mayAnnounce": life.may_announce(&id, state) }),
                );
            }
            Ok(view)
        }

        // --- settings -----------------------------------------------------
        "minecraft.settings.fromEnv" => {
            let resolved = settings::from_env(
                field("env")?,
                optional_text("gameType").as_deref().unwrap_or("java"),
                optional_text("loader").as_deref().unwrap_or("vanilla"),
                optional_text("fallbackMotd").as_deref(),
            );
            serde_json::to_value(resolved).map_err(|e| e.to_string())
        }

        "minecraft.settings.properties" => {
            let resolved: settings::Settings = serde_json::from_value(field("settings")?.clone())
                .map_err(|e| format!("bad settings: {e}"))?;
            let runtime: settings::Runtime = serde_json::from_value(field("runtime")?.clone())
                .map_err(|e| format!("bad runtime: {e}"))?;
            // An ordered list, not an object: the merge appends new keys in
            // this order, and a JSON object would not preserve it.
            Ok(Value::Array(
                settings::properties(&resolved, &runtime)
                    .into_iter()
                    .map(|(k, v)| json!([k, v]))
                    .collect(),
            ))
        }

        "minecraft.settings.offlineUuid" => Ok(Value::from(settings::offline_uuid(&text("name")?))),

        "minecraft.settings.dashUuid" => settings::dash_uuid(&text("undashed")?)
            .map(Value::from)
            .map_err(|e| e.to_string()),

        "minecraft.settings.opsJson" => {
            Ok(settings::ops_json(&players(field("players")?, method)?))
        }

        "minecraft.settings.whitelistJson" => Ok(settings::whitelist_json(&players(
            field("players")?,
            method,
        )?)),

        "minecraft.settings.bannedMissing" => {
            let banned: Vec<String> = field("banned")?
                .as_array()
                .ok_or_else(|| format!("\"{method}\" needs banned to be a list"))?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            Ok(Value::from(settings::banned_missing(
                optional_text("existing").unwrap_or_default().as_str(),
                &banned,
            )))
        }

        "minecraft.settings.mergeBanned" => Ok(settings::merge_banned(
            optional_text("existing").unwrap_or_default().as_str(),
            &players(field("additions")?, method)?,
            &text("created")?,
        )
        .map(Value::from)
        .unwrap_or(Value::Null)),

        // Proves the panic guard in `call` is real rather than asserted.
        // Compiled only under test, so it cannot be reached in a shipped build.
        #[cfg(test)]
        m if m == PANIC_PROBE => panic!("deliberate panic, to prove the guard catches one"),

        // Naming the method is not enough on its own. Every time this fires in
        // practice it is not a typo, it is a *stale library* — a host built
        // after a method was added, loading a `.so` or `.a` from before it. The
        // Kotlin or Swift compiled cleanly, so the message has to say where to
        // look, or the search starts in entirely the wrong file.
        other => Err(format!(
            "the native core has no method \"{other}\" — this library was built \
             before the host calling it, so rebuild the native library"
        )),
    }
}

/// A method that exists only under test, to prove the panic guard is real.
///
/// Asserting "we call catch_unwind" by reading the code is not evidence; this
/// makes it a fact. Guarded so it cannot be reached in a shipped build.
#[cfg(test)]
const PANIC_PROBE: &str = "debug.panic";

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a reply, asserting it succeeded, and hand back the value.
    fn ok(method: &str, args: serde_json::Value) -> Value {
        let raw = call(method, &args.to_string());
        let reply: Value = serde_json::from_str(&raw).expect("replies are always JSON");
        assert_eq!(
            reply["ok"],
            true,
            "{method} failed: {}",
            reply["error"].as_str().unwrap_or("(no error text)")
        );
        reply["value"].clone()
    }

    /// Parse a reply, asserting it failed, and hand back the message.
    fn err(method: &str, args: serde_json::Value) -> String {
        let raw = call(method, &args.to_string());
        let reply: Value = serde_json::from_str(&raw).expect("replies are always JSON");
        assert_eq!(reply["ok"], false, "{method} unexpectedly succeeded: {raw}");
        reply["error"].as_str().unwrap().to_string()
    }

    // ─── the envelope ───────────────────────────────────────────────────────

    #[test]
    fn every_reply_is_one_of_two_shapes() {
        let good: Value = serde_json::from_str(&call("game.list", "{}")).unwrap();
        assert_eq!(good["ok"], true);
        assert!(good.get("value").is_some());
        assert!(good.get("error").is_none());

        let bad: Value = serde_json::from_str(&call("nope", "{}")).unwrap();
        assert_eq!(bad["ok"], false);
        assert!(bad["error"].is_string());
        assert!(bad.get("value").is_none());
    }

    /// An unknown method names itself *and* the reason it is usually unknown.
    ///
    /// This has fired for real exactly once, and the cause was a rebuilt APK
    /// packaging a library from before the method existed. The message is what
    /// stands between that and an hour spent in the Kotlin that just compiled.
    #[test]
    fn an_unknown_method_names_itself_and_says_to_rebuild() {
        let message = err("settings.fromEnv", json!({}));
        assert!(
            message.contains("settings.fromEnv"),
            "the message must name the method: {message}"
        );
        assert!(
            message.contains("rebuild"),
            "the message must say what to do about it: {message}"
        );
    }

    #[test]
    fn malformed_arguments_are_reported_not_swallowed() {
        let raw = call("game.list", "not json");
        let reply: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("bad arguments"));
    }

    #[test]
    fn a_missing_argument_names_the_method_and_the_argument() {
        let message = err("game.classify", json!({}));
        assert!(
            message.contains("game.classify") && message.contains("line"),
            "{message}"
        );
    }

    /// The guard exists so a bug in this crate cannot abort the host process —
    /// a JNI unwind aborts the VM and a C-ABI unwind is undefined behaviour.
    ///
    /// Takes `crash::test_guard` because panicking is process-global: the
    /// installed hook records into a single last-panic slot, and without this
    /// the crash module's own tests can read *this* panic instead of theirs.
    /// It passes without the guard most of the time, which is worse than
    /// failing.
    #[test]
    fn a_panic_becomes_an_error_rather_than_unwinding() {
        let _guard = crate::crash::test_guard();
        let message = err(PANIC_PROBE, json!({}));
        assert!(
            message.contains("panicked") && message.contains(PANIC_PROBE),
            "a panic must be reported as one, naming the method: {message}"
        );
    }

    // ─── game routing ───────────────────────────────────────────────────────

    #[test]
    fn calls_default_to_minecraft() {
        let meaning = ok(
            "game.classify",
            json!({ "line": "[12:00:00] [Server thread/INFO]: Done (1.0s)! For help, type \"help\"" }),
        );
        assert_eq!(meaning["ready"], true);
    }

    #[test]
    fn an_unknown_game_is_refused_rather_than_defaulted() {
        let message = err("game.classify", json!({ "game": "halflife", "line": "x" }));
        assert!(message.contains("halflife"), "{message}");
    }

    #[test]
    fn the_registry_is_reachable() {
        let games = ok("game.list", json!({}));
        assert!(games
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g == "minecraft-java"));
    }

    // ─── a real round trip ──────────────────────────────────────────────────

    /// The whole path a host takes, over the same strings it would send.
    #[test]
    fn a_host_can_build_config_files_end_to_end() {
        let env = json!({ "MOTD": "over the bridge", "OPS": "Notch", "ONLINE_MODE": "false" });

        let inputs = ok("game.configInputs", json!({ "env": env }));
        assert!(inputs
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["path"] == "server.properties"));

        // Offline, so the core asks the host for nothing.
        let lookups = ok(
            "game.requiredLookups",
            json!({ "env": env, "gameType": "java" }),
        );
        assert!(lookups.as_array().unwrap().is_empty());

        let files = ok(
            "game.configFiles",
            json!({ "context": {
                "env": env,
                "game_type": "java",
                "port": 25565,
                "bind_address": "127.0.0.1",
                "existing": {},
                "resolved": [],
                "now": "2026-08-10 14:03:22 +0000"
            }}),
        );
        let files = files.as_array().unwrap();
        let props = files
            .iter()
            .find(|f| f["path"] == "server.properties")
            .unwrap();
        assert!(props["contents"]
            .as_str()
            .unwrap()
            .contains("motd=over the bridge"));
        assert_eq!(props["encoding"], "latin1");

        let ops = files.iter().find(|f| f["path"] == "ops.json").unwrap();
        assert!(
            ops["contents"]
                .as_str()
                .unwrap()
                .contains("b50ad385-829d-3141-a216-7e7d7539ba7f"),
            "the offline UUID must be derived in the core"
        );
    }

    #[test]
    fn the_tunnel_renders_through_the_games_forwards() {
        let config = ok(
            "tunnel.render",
            json!({
                "link": {
                    "client_privkey": "K",
                    "gateway_pubkey": "P",
                    "link_address": "gw:51820"
                },
                "exposure": "java",
                "port": 25570
            }),
        );
        let text = config.as_str().unwrap();
        assert!(text.contains("[TCPServerTunnel]"));
        assert!(
            text.contains("ListenPort = 25565"),
            "the gateway port is fixed"
        );
        assert!(
            text.contains("Target = 127.0.0.1:25570"),
            "the local port follows"
        );
    }

    /// A typo must not silently produce a Java-only config for a crossplay
    /// server — that server runs, and no Bedrock player can join.
    #[test]
    fn an_unknown_exposure_is_refused() {
        err(
            "tunnel.render",
            json!({
                "link": { "client_privkey": "K", "gateway_pubkey": "P", "link_address": "gw:1" },
                "exposure": "crossplaY",
                "port": 25565
            }),
        );
    }

    // ─── backups ────────────────────────────────────────────────────────────

    /// The handoff, over the wire a host actually sends.
    #[test]
    fn the_restore_decision_crosses_the_bridge() {
        let decision = ok(
            "backup.restoreDecision",
            json!({
                "deviceId": "device-a",
                "hasLocalWorld": true,
                "latest": { "id": "s1", "time": "2026-08-10T12:00:00Z", "host": "device-b" }
            }),
        );
        assert_eq!(decision["action"], "restoreLatest");
        assert_eq!(decision["snapshot_id"], "s1");
        assert_eq!(decision["reason"], "anotherDeviceIsNewer");
    }

    #[test]
    fn a_missing_snapshot_is_null_not_an_error() {
        let decision = ok(
            "backup.restoreDecision",
            json!({ "deviceId": "device-a", "hasLocalWorld": true, "latest": null }),
        );
        assert_eq!(decision["action"], "skip");
        assert_eq!(decision["reason"], "noSnapshotAvailable");
    }

    #[test]
    fn the_lease_gate_crosses_the_bridge() {
        let blocked = ok(
            "backup.leaseDecision",
            json!({ "leaseDevice": "device-b", "deviceId": "device-a" }),
        );
        assert_eq!(blocked["action"], "blocked");
        assert_eq!(blocked["device"], "device-b");

        let forced = ok(
            "backup.leaseDecision",
            json!({ "leaseDevice": "device-b", "deviceId": "device-a", "force": true }),
        );
        assert_eq!(forced["action"], "forced");
    }

    /// Android's every-backup case: complete, verified, and non-zero.
    #[test]
    fn exit_three_reaches_the_host_as_success() {
        let verdict = ok(
            "backup.classify",
            json!({
                "exitCode": 3,
                "message": "Warning: at least one source file could not be read",
                "host": "localhost"
            }),
        );
        assert_eq!(verdict["failure"]["kind"], "completedWithWarnings");
        assert_eq!(verdict["succeeded"], true);
    }

    #[test]
    fn the_state_report_body_is_built_for_the_host() {
        let built = ok(
            "backup.stateReport",
            json!({ "operation": "backup", "snapshotId": "abc", "bytes": 10, "durationSeconds": 2.0 }),
        );
        assert_eq!(built["body"]["status"], "complete");
        assert_eq!(built["body"]["speed_bps"], 5.0);
        assert_eq!(built["releasesLease"], true);

        let failed = ok(
            "backup.stateReport",
            json!({ "operation": "backup", "error": "disk full" }),
        );
        assert_eq!(failed["body"]["status"], "failed");
        assert_eq!(
            failed["releasesLease"], true,
            "a failed backup must still release it"
        );
    }

    #[test]
    fn an_unknown_backup_operation_is_refused() {
        assert!(err("backup.stateReport", json!({ "operation": "sync" })).contains("sync"));
    }

    // --- lifecycle --------------------------------------------------------

    /// Drive one event and hand back the whole reply, carrying the state
    /// forward exactly as a host does.
    fn step(life: &Value, event: &str, id: &str, extra: Option<Value>) -> Value {
        let mut args = json!({ "lifecycle": life, "event": event, "serverId": id });
        if let Some(Value::Object(more)) = extra {
            args.as_object_mut().unwrap().extend(more);
        }
        ok("lifecycle.apply", args)
    }

    /// The whole point of the module, across the boundary: a server is this
    /// device's from the moment a start arrives until its process is gone.
    #[test]
    fn a_server_is_active_across_the_ffi_from_start_to_exit() {
        // No lifecycle yet — the first call creates one.
        let r = ok(
            "lifecycle.apply",
            json!({ "event": "startRequested", "serverId": "s" }),
        );
        assert_eq!(r["verdict"], "proceed");
        assert_eq!(r["activeIds"], json!(["s"]));
        assert_eq!(r["runningIds"], json!([]), "active is not running");

        let r = step(&r["lifecycle"], "spawned", "s", None);
        let r = step(&r["lifecycle"], "consoleReady", "s", None);
        // The start call returns once the server is up — this is the host's
        // `finally`, and the server stays active on the strength of its live
        // engine alone.
        let r = step(&r["lifecycle"], "callFinished", "s", None);
        assert_eq!(r["runningIds"], json!(["s"]));
        assert_eq!(r["state"], "running");
        assert_eq!(r["activeIds"], json!(["s"]));

        // A stop, and the whole graceful shutdown that follows. The console
        // is up, so it can be asked to save rather than terminated.
        let r = step(&r["lifecycle"], "stopRequested", "s", None);
        assert_eq!(r["verdict"], "graceful");
        assert_eq!(
            r["activeIds"],
            json!(["s"]),
            "still ours while the world saves"
        );

        let r = step(&r["lifecycle"], "exited", "s", Some(json!({ "code": 0 })));
        assert_eq!(r["state"], "stopped");
        assert_eq!(r["intentional"], true);
        assert_eq!(r["superseded"], false);

        let r = step(&r["lifecycle"], "callFinished", "s", None);
        assert_eq!(r["activeIds"], json!([]));
    }

    #[test]
    fn a_termination_after_a_stop_request_is_not_a_crash() {
        let r = ok(
            "lifecycle.apply",
            json!({ "event": "startRequested", "serverId": "s" }),
        );
        let r = step(&r["lifecycle"], "spawned", "s", None);
        // Still generating terrain — no console to hear `stop`, so it is
        // terminated rather than waited on.
        let r = step(&r["lifecycle"], "stopRequested", "s", None);
        assert_eq!(r["verdict"], "terminate");

        let r = step(&r["lifecycle"], "exited", "s", Some(json!({ "code": 143 })));
        assert_eq!(r["state"], "stopped");
        assert_eq!(r["intentional"], true);
    }

    #[test]
    fn a_stop_before_the_engine_exists_tells_the_launch_to_abandon() {
        let r = ok(
            "lifecycle.apply",
            json!({ "event": "startRequested", "serverId": "s" }),
        );
        let r = step(&r["lifecycle"], "stopRequested", "s", None);
        assert_eq!(r["verdict"], "abandonLaunch");
        assert_eq!(r["shouldAbandon"], true);
    }

    #[test]
    fn one_server_at_a_time_names_the_one_in_the_way() {
        let r = ok(
            "lifecycle.apply",
            json!({ "event": "startRequested", "serverId": "first" }),
        );
        let r = step(&r["lifecycle"], "startRequested", "second", None);
        assert_eq!(r["verdict"], "anotherServerRunning");
        assert_eq!(r["serverId"], "first");
    }

    /// `concurrency` is read only when there is no state to resume, so a host
    /// cannot change the rules halfway through a server's life.
    #[test]
    fn a_many_host_runs_several_at_once() {
        let r = ok(
            "lifecycle.apply",
            json!({ "event": "startRequested", "serverId": "a", "concurrency": "many" }),
        );
        let r = step(&r["lifecycle"], "startRequested", "b", None);
        assert_eq!(r["verdict"], "proceed");
        assert_eq!(r["activeIds"], json!(["a", "b"]));
    }

    #[test]
    fn a_query_answers_without_changing_anything() {
        let r = ok(
            "lifecycle.apply",
            json!({ "event": "startRequested", "serverId": "s" }),
        );
        let r = step(&r["lifecycle"], "spawned", "s", None);
        let r = step(&r["lifecycle"], "stopRequested", "s", None);

        let view = ok(
            "lifecycle.query",
            json!({ "lifecycle": r["lifecycle"], "serverId": "s", "state": "running" }),
        );
        assert_eq!(
            view["mayAnnounce"], false,
            "a stopping server is never announced running"
        );
        assert_eq!(view["activeIds"], json!(["s"]));
        assert_eq!(view["lifecycle"], r["lifecycle"], "a query mutates nothing");
    }

    /// The numbers a host used to hard-code, now asked for.
    #[test]
    fn the_jvm_command_line_and_the_stop_ladder_come_from_here() {
        // A phone: a third of 6 GB, not the 4 GB asked for.
        let phone = ok(
            "minecraft.jvm.launch",
            json!({ "memoryMb": 4096, "deviceTotalMb": 6144 }),
        );
        assert_eq!(phone["heapMb"], 2048);
        assert_eq!(phone["options"][0], "-Xmx2048M");
        assert_eq!(phone["options"][1], "-Xms2048M");
        assert_eq!(phone["programArgs"][0], "nogui");
        assert_eq!(phone["eulaContents"], "eula=true\n");

        // No ceiling given: what was asked for.
        let desktop = ok("minecraft.jvm.launch", json!({ "memoryMb": 4096 }));
        assert_eq!(desktop["heapMb"], 4096);

        let ladder = ok("minecraft.jvm.stopLadder", json!({ "console": true }));
        assert_eq!(ladder["command"], "stop");
        let rungs = ladder["rungs"].as_array().unwrap();
        assert_eq!(rungs[0]["action"], "console");
        assert_eq!(rungs[0]["waitMs"], 30_000);
        assert_eq!(rungs.last().unwrap()["action"], "kill");

        // Nothing listening on stdin: no point asking.
        let blunt = ok("minecraft.jvm.stopLadder", json!({ "console": false }));
        assert_eq!(blunt["rungs"][0]["action"], "terminate");

        let limits = ok("minecraft.jvm.limits", json!({}));
        assert_eq!(limits["startTimeoutMs"], 300_000);
        assert_eq!(limits["previousExitWaitMs"], 120_000);

        let text = ok("minecraft.jvm.refusal", json!({ "kind": "startTimedOut" }));
        assert_eq!(text, "The server did not finish starting in time.");

        // A wording that does not exist is an error, not an empty string the
        // player would be shown.
        assert!(call(
            "minecraft.jvm.refusal",
            &json!({ "kind": "nope" }).to_string()
        )
        .contains("\"ok\":false"));
    }

    /// The loop a host implements, on the wire: ask, read, record, read back.
    #[test]
    fn a_host_can_sample_a_run_without_deciding_anything() {
        // A fresh history wants a sample immediately.
        let empty = ok("metrics.query", json!({ "nowMs": 0 }));
        assert_eq!(empty["due"], true);
        assert_eq!(empty["intervalMs"], 30_000);
        assert_eq!(empty["samples"].as_array().unwrap().len(), 0);

        let first = ok(
            "metrics.record",
            json!({ "reading": { "atMs": 0, "memUsedKb": 2_097_152, "cpuSeconds": 0.0 } }),
        );
        assert_eq!(first["appended"], true);
        let history = first["history"].clone();

        // Offered again a second later: kept as the anchor for the next rate,
        // but not graphed.
        let early = ok(
            "metrics.record",
            json!({
                "history": history,
                "reading": { "atMs": 1_000, "memUsedKb": 2_097_152, "cpuSeconds": 1.0 },
            }),
        );
        assert_eq!(early["appended"], false);

        let due = ok(
            "metrics.record",
            json!({
                "history": early["history"].clone(),
                "reading": { "atMs": 30_000, "memUsedKb": 3_145_728, "cpuSeconds": 30.0 },
            }),
        );
        assert_eq!(due["appended"], true);

        let graph = ok(
            "metrics.query",
            json!({ "history": due["history"].clone() }),
        );
        let samples = graph["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0]["memUsedMb"], 2048);
        // Nothing to measure the first against.
        assert!(samples[0]["cpuPercent"].is_null());
        // 29 s of CPU over the 29 s since the dropped reading — one core.
        assert_eq!(samples[1]["cpuPercent"], 100.0);
        assert_eq!(samples[1]["memUsedMb"], 3072);
    }

    /// The two-step shape the host has to implement, pinned on the wire.
    ///
    /// `verify` naming its own algorithm is what lets the host hash without
    /// knowing whether it is holding a Mojang sha1 or a PaperMC sha256.
    #[test]
    fn the_cache_decision_asks_for_a_digest_before_it_gives_a_verdict() {
        let artifact = json!({
            "url": "https://example/server.jar",
            "loader": "vanilla",
            "version": "1.21.4",
            "required_java": 21,
            "checksum": { "algorithm": "Sha1", "hex": "abc123" },
        });

        // No marker, but a jar is there: hash it.
        let ask = ok(
            "minecraft.jar.cacheDecision",
            json!({ "artifact": artifact, "present": true }),
        );
        assert_eq!(ask["action"], "verify");
        assert_eq!(ask["algorithm"], "Sha1");

        // Asked again with the answer.
        let verdict = ok(
            "minecraft.jar.cacheDecision",
            json!({ "artifact": artifact, "present": true, "digest": "abc123" }),
        );
        assert_eq!(verdict["action"], "adopt");

        // A marker that already agrees never reaches the hashing at all.
        let cheap = ok(
            "minecraft.jar.cacheDecision",
            json!({
                "artifact": artifact,
                "present": true,
                "onDisk": { "loader": "vanilla", "version": "1.21.4", "checksum": "abc123" },
            }),
        );
        assert_eq!(cheap["action"], "use");

        // And nothing on disk is a download without asking anything.
        let absent = ok(
            "minecraft.jar.cacheDecision",
            json!({ "artifact": artifact, "present": false }),
        );
        assert_eq!(absent["action"], "download");
    }

    /// The order the Android host actually runs, pinned against the core's.
    ///
    /// This is the point of the module: the host may add platform detail
    /// around a step, but if it reorders one relative to another, this fails
    /// rather than the reordering surfacing months later as a world that
    /// downloaded itself again or a green card for an unreachable server.
    #[test]
    fn the_plan_is_the_order_the_hosts_run() {
        let steps = ok(
            "launch.plan",
            json!({ "backups": true, "settings": true, "tunnel": true }),
        );
        let names: Vec<&str> = steps
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["step"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "cancelOnStopBackup",
                "announceStarting",
                "beginResolveTunnel",
                "ensureRuntime",
                "ensureJar",
                "acceptEula",
                "resolveMainClass",
                "awaitPreviousExit",
                "restoreWorld",
                "writeSettings",
                "spawn",
                "awaitConsole",
                "openTunnel",
                "announceRunning",
            ]
        );

        // And the checkpoints travel with the steps, so a host does not have
        // to remember which ones a stop must be honoured before.
        let checkpoints: Vec<&str> = steps
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["checkpoint"] == true)
            .map(|s| s["step"].as_str().unwrap())
            .collect();
        assert_eq!(
            checkpoints,
            vec!["restoreWorld", "spawn", "openTunnel", "announceRunning"]
        );
    }

    /// The order the iOS host runs: the same plan minus the two steps that are
    /// about a jar.
    ///
    /// `ensureRuntime` and `acceptEula` are still here on purpose — neither is
    /// about the jar, so neither is gated on how the engine arrives. iOS skips
    /// them by not asking, which is a host's business.
    #[test]
    fn a_linked_host_is_planned_without_the_jar_steps() {
        let steps = ok(
            "launch.plan",
            json!({ "backups": true, "settings": true, "tunnel": true, "engine": "linked" }),
        );
        let names: Vec<&str> = steps
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["step"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "cancelOnStopBackup",
                "announceStarting",
                "beginResolveTunnel",
                "ensureRuntime",
                "acceptEula",
                "awaitPreviousExit",
                "restoreWorld",
                "writeSettings",
                "spawn",
                "awaitConsole",
                "openTunnel",
                "announceRunning",
            ]
        );
    }

    /// An unknown engine is spawned, not an error.
    ///
    /// The alternative is a host that fails to launch because it sent a word
    /// this build has not heard of. Spawned is the plan every host had before
    /// the field existed, so it is the safe answer — and Android, which sends
    /// no `engine` at all, depends on it.
    #[test]
    fn an_unrecognised_engine_falls_back_to_the_plan_everyone_had() {
        for engine in [json!("wasm"), json!(null), json!(7)] {
            let steps = ok(
                "launch.plan",
                json!({ "backups": true, "settings": true, "tunnel": true, "engine": engine }),
            );
            let names: Vec<&str> = steps
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["step"].as_str().unwrap())
                .collect();
            assert!(
                names.contains(&"ensureJar"),
                "{engine} dropped the jar step"
            );
            assert_eq!(names.len(), 14, "{engine}");
        }
    }

    #[test]
    fn an_unknown_lifecycle_event_is_refused_by_name() {
        let message = err(
            "lifecycle.apply",
            json!({ "event": "restarted", "serverId": "s" }),
        );
        assert!(message.contains("restarted"), "{message}");
    }
}

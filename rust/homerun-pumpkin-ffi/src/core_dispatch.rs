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
use homerun_core::minecraft::{
    self, account, argfile, crossplay, hosting, jar, jvm, loader, modjar, modpack, mods, ops,
    settings,
};
use homerun_core::reporting::{app_error, crash, minigame, stats};
use homerun_core::{
    backup, bundle, device_ws, game, launch, lifecycle, link, metrics, properties, state, tunnel,
};

/// Dispatch one call and render the reply envelope.
///
/// Never panics and never fails: every outcome, including a panic, is a JSON
/// string the caller can hand straight back to its host language.
pub fn call(method: &str, args: &str) -> String {
    // Every path into this crate passes through here, on both platforms, so
    // this is where the panic hook becomes live — on the first core call at
    // boot rather than at server start, which is far too late and never
    // happens at all on a device that only browses. `install_hook` is
    // idempotent behind an atomic, so the cost is one relaxed load per call.
    crate::crash::install_hook();

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

/// Read the caller's [`app_error::Context`].
///
/// A malformed context is refused rather than defaulted. A report filed
/// against an empty device id and no version is a row nobody can act on, and
/// it would look exactly like a successful report.
fn context_arg(value: &Value, method: &str) -> Result<app_error::Context, String> {
    serde_json::from_value(value.clone())
        .map_err(|e| format!("\"{method}\" got a context it could not read: {e}"))
}

/// Read the caller's [`app_error::Occurrence`].
fn occurrence_arg(value: &Value, method: &str) -> Result<app_error::Occurrence, String> {
    serde_json::from_value(value.clone())
        .map_err(|e| format!("\"{method}\" got an occurrence it could not read: {e}"))
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

/// A run's console, as the crash reporters take it.
///
/// Missing is not empty here — a host that failed to collect the console and a
/// run that printed nothing are different situations, and only one of them is
/// worth a report. Absent is an error; an empty list is honoured.
fn console_lines(args: &Value, method: &str) -> Result<Vec<String>, String> {
    let raw = args
        .get("lines")
        .ok_or_else(|| format!("\"{method}\" needs the console lines"))?;
    serde_json::from_value(raw.clone())
        .map_err(|e| format!("\"{method}\" needs lines as a list of strings: {e}"))
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

        "minecraft.jar.selectRuntime" => {
            let artifact: jar::Artifact = serde_json::from_value(field("artifact")?.clone())
                .map_err(|e| format!("bad artifact: {e}"))?;
            let bundled: Vec<u16> = args
                .get("bundled")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_u64)
                        .map(|v| v as u16)
                        .collect()
                })
                .unwrap_or_default();
            // The loader is passed separately rather than read from the
            // artifact: a loader that installs itself is judged against the
            // *vanilla* artifact for the version it targets, so the artifact's
            // own `loader` field would say "vanilla" and lose the policy that
            // refuses NeoForge a runtime it cannot use.
            let kind = jar::Loader::parse(optional_text("loader").as_deref())
                .map_err(|e| e.to_string())?;
            jar::select_runtime(&artifact, kind, &bundled)
                .map(Value::from)
                .map_err(|e| e.to_string())
        }

        "minecraft.jar.selectRuntimeFor" => {
            let bundled: Vec<u16> = args
                .get("bundled")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_u64)
                        .map(|v| v as u16)
                        .collect()
                })
                .unwrap_or_default();
            let required = args
                .get("requiredJava")
                .and_then(|v| v.as_u64())
                .ok_or("requiredJava is required")? as u16;
            let kind = jar::Loader::parse(optional_text("loader").as_deref())
                .map_err(|e| e.to_string())?;
            jar::select_runtime_for(required, &text("what")?, kind, &bundled)
                .map(Value::from)
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

        // --- loaders that install by running an installer ------------------

        // What the host advertises as `HostCapabilities.serverLoaders`, so the
        // create flow offers exactly what the launch will accept.
        "minecraft.loader.hostable" => Ok(Value::Array(
            jar::Loader::hostable()
                .iter()
                .map(|l| Value::from(l.as_str()))
                .collect(),
        )),

        "minecraft.loader.isInstalled" => jar::Loader::parse(optional_text("loader").as_deref())
            .map(|l| Value::Bool(l.is_installed()))
            .map_err(|e| e.to_string()),

        "minecraft.loader.launchJar" => jar::Loader::parse(optional_text("loader").as_deref())
            .map(|l| match loader::launch_jar(l) {
                Some(name) => Value::from(name),
                None => Value::Null,
            })
            .map_err(|e| e.to_string()),

        // The endpoints, so the host has no copy of them to drift from.
        // `jar`'s equivalents predate this and are still spelled on both sides.
        "minecraft.loader.fabricInstallerMeta" => Ok(Value::from(loader::FABRIC_INSTALLER_META)),
        "minecraft.loader.quiltInstallerMeta" => Ok(Value::from(loader::QUILT_INSTALLER_META)),
        "minecraft.loader.neoforgeMetadata" => Ok(Value::from(loader::NEOFORGE_METADATA)),
        "minecraft.loader.forgeMetadata" => Ok(Value::from(loader::FORGE_METADATA)),

        "minecraft.loader.resolveVersion" => {
            let kind = jar::Loader::parse(optional_text("loader").as_deref())
                .map_err(|e| e.to_string())?;
            let xml = text("metadata")?;
            let mc = text("mcVersion")?;
            let pinned = optional_text("pinned");
            match kind {
                jar::Loader::NeoForge => {
                    loader::resolve_neoforge_version(&xml, &mc, pinned.as_deref())
                }
                jar::Loader::Forge => loader::resolve_forge_version(&xml, &mc, pinned.as_deref()),
                other => Err(homerun_core::Error::Unsupported(format!(
                    "{} has no versioned metadata",
                    other.as_str()
                ))),
            }
            .map(Value::from)
            .map_err(|e| e.to_string())
        }

        "minecraft.loader.installerUrl" => {
            let kind = jar::Loader::parse(optional_text("loader").as_deref())
                .map_err(|e| e.to_string())?;
            loader::installer_url(kind, &text("version")?)
                .map(Value::from)
                .map_err(|e| e.to_string())
        }

        // --- argfiles ------------------------------------------------------
        //
        // Forge and NeoForge launch entirely through `@argfile`s, and
        // expanding one is a feature of the `java` binary this platform does
        // not have. See `minecraft::argfile`.

        "minecraft.argfile.expand" => {
            let contents: Vec<String> = args
                .get("contents")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            serde_json::to_value(argfile::expand(&contents)).map_err(|e| e.to_string())
        }

        "minecraft.argfile.referenced" => Ok(Value::from(argfile::referenced_argfiles(&text(
            "runScript",
        )?))),

        "minecraft.argfile.runScript" => {
            let present: Vec<String> = args
                .get("present")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Ok(match argfile::preferred_run_script(&present) {
                Some(name) => Value::from(name),
                None => Value::Null,
            })
        }

        "minecraft.argfile.fallback" => {
            let paths: Vec<String> = args
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Ok(match argfile::fallback_argfile(&paths) {
                Some(path) => Value::from(path),
                None => Value::Null,
            })
        }

        "minecraft.loader.fabricInstallerUrl" => loader::fabric_installer_url(field("meta")?)
            .map(Value::from)
            .map_err(|e| e.to_string()),

        // Separate from Fabric's because the selection rule is different, not
        // just the URL — Quilt's index has no `stable` flag. See
        // `loader::quilt_installer_url`.
        "minecraft.loader.quiltInstallerUrl" => loader::quilt_installer_url(field("meta")?)
            .map(Value::from)
            .map_err(|e| e.to_string()),

        "minecraft.loader.quiltIntermediaryUrl" => {
            Ok(Value::from(loader::quilt_intermediary_url(&text("mcVersion")?)))
        }

        // Returns null on success and an error otherwise, so the refusal text
        // reaches the player through the same path every other refusal does.
        "minecraft.loader.ensureQuiltSupports" => {
            loader::ensure_quilt_supports(&text("mcVersion")?, field("intermediary")?)
                .map(|()| Value::Null)
                .map_err(|e| e.to_string())
        }

        "minecraft.loader.needsReinstall" => {
            // Absent and unparseable are the same answer — reinstall — and
            // that is the safe direction: a marker we cannot read is one we
            // cannot trust to describe what is on disk.
            let installed: Option<loader::Installed> = args
                .get("installed")
                .filter(|v| !v.is_null())
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let kind = jar::Loader::parse(optional_text("loader").as_deref())
                .map_err(|e| e.to_string())?;
            Ok(Value::Bool(loader::needs_reinstall(
                installed.as_ref(),
                kind,
                &text("mcVersion")?,
                optional_text("loaderVersion").as_deref(),
            )))
        }

        "minecraft.loader.filesToClean" => {
            let entries: Vec<String> = args
                .get("entries")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Ok(Value::from(loader::files_to_clean(&entries)))
        }

        // --- which mods a server gets --------------------------------------
        //
        // A driver, not a function: installing mods is three phases of
        // interleaved HTTP with a graph search in the middle, and this crate
        // has no I/O. `begin` says what to fetch, `advance` says what the
        // answers meant and what to fetch next. See `minecraft::mods`.

        "minecraft.mods.begin" => {
            let inputs: mods::Inputs = serde_json::from_value(field("inputs")?.clone())
                .map_err(|e| format!("bad mod inputs: {e}"))?;
            serde_json::to_value(mods::begin(inputs)).map_err(|e| e.to_string())
        }

        "minecraft.mods.advance" => {
            let session: mods::Session = serde_json::from_value(field("state")?.clone())
                .map_err(|e| format!("bad mod session: {e}"))?;
            let replies: Vec<mods::Reply> = args
                .get("replies")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| format!("bad mod replies: {e}"))?
                .unwrap_or_default();
            serde_json::to_value(mods::advance(session, replies)).map_err(|e| e.to_string())
        }

        "minecraft.mods.subDir" => Ok(Value::from(mods::sub_dir(&text("loader")?))),

        // --- crossplay ------------------------------------------------------
        //
        // A Java server Bedrock clients can also join: Geyser and Floodgate, as
        // plugins, inside the server's own JVM. Four calls because the work
        // happens at four different moments in a launch — before the mods are
        // resolved, around the Floodgate fetch, and when the settings are
        // written. See `minecraft::crossplay`.
        "minecraft.crossplay.isCrossplay" => {
            Ok(Value::Bool(crossplay::is_crossplay(&text("gameType")?)))
        }

        "minecraft.exposure" => Ok(Value::from(minecraft::exposure_for(&text("gameType")?))),

        "minecraft.crossplay.mergeProjects" => Ok(Value::from(crossplay::merge_projects(
            &text("gameType")?,
            &text("loader")?,
            &optional_text("configured").unwrap_or_default(),
        ))),

        "minecraft.crossplay.floodgate" => Ok(
            match crossplay::floodgate(&text("gameType")?, &text("loader")?) {
                Some(flavour) => json!({
                    "metaUrl": crossplay::FLOODGATE_META,
                    "flavour": flavour,
                }),
                None => Value::Null,
            },
        ),

        "minecraft.crossplay.floodgateBuild" => {
            match crossplay::floodgate_build(field("meta")?, &text("flavour")?) {
                Some(fetch) => serde_json::to_value(fetch).map_err(|e| e.to_string()),
                None => Ok(Value::Null),
            }
        }

        "minecraft.crossplay.config" => {
            match crossplay::config(&text("gameType")?, &text("loader")?) {
                Some(file) => serde_json::to_value(file).map_err(|e| e.to_string()),
                None => Ok(Value::Null),
            }
        }

        // --- minigames ------------------------------------------------------
        //
        // Our own plugin jars, and what makes a lobby different from a world
        // somebody lives in. `minecraft::minigame`, not the `reporting::minigame`
        // imported above — one decides what to install and how to launch it, the
        // other reads a finished match off the console. Spelled out in full here
        // rather than aliased, so nobody has to go and check which is which.

        "minecraft.minigame.isMinigame" => {
            Ok(Value::Bool(minecraft::minigame::is_minigame(field("env")?)))
        }

        "minecraft.minigame.customPlugins" => serde_json::to_value(
            minecraft::minigame::custom_plugins(&text("loader")?, field("env")?),
        )
        .map_err(|e| e.to_string()),

        "minecraft.minigame.pluginEnv" => {
            serde_json::to_value(minecraft::minigame::forwarded_env(field("env")?))
                .map_err(|e| e.to_string())
        }

        // --- Minecraft accounts ---------------------------------------------
        //
        // The Microsoft sign-in chain, as a set of "build this request" /
        // "read that response" pairs. Same division as `mods` above and for the
        // same reason: five sequential HTTP calls whose bodies are full of
        // details that fail silently when wrong, and two hosts that would
        // otherwise each get them wrong differently. See `minecraft::account`.

        "minecraft.account.deviceCodeRequest" => {
            serde_json::to_value(account::device_code_request()).map_err(|e| e.to_string())
        }

        "minecraft.account.deviceCodeFrom" => {
            let code = account::device_code_from(field("body")?).map_err(|e| e.to_string())?;
            let mut value = serde_json::to_value(&code).map_err(|e| e.to_string())?;
            // Attached here so every host opens the same URL rather than each
            // one reassembling it from the parts.
            value["approvalUrl"] = Value::from(code.approval_url());
            Ok(value)
        }

        "minecraft.account.pollRequest" => {
            serde_json::to_value(account::poll_request(&text("deviceCode")?))
                .map_err(|e| e.to_string())
        }

        "minecraft.account.pollOutcome" => {
            serde_json::to_value(account::poll_outcome(field("body")?).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())
        }

        // Needed on the refresh path, where the body is Microsoft's own
        // snake_case rather than a `Poll` this crate already normalised.
        "minecraft.account.msaTokensFrom" => {
            serde_json::to_value(account::msa_tokens_from(field("body")?).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())
        }

        "minecraft.account.refreshRequest" => {
            serde_json::to_value(account::refresh_request(&text("refreshToken")?))
                .map_err(|e| e.to_string())
        }

        "minecraft.account.xblRequest" => {
            serde_json::to_value(account::xbl_request(&text("msaAccessToken")?))
                .map_err(|e| e.to_string())
        }

        "minecraft.account.xstsRequest" => {
            serde_json::to_value(account::xsts_request(&text("xblToken")?))
                .map_err(|e| e.to_string())
        }

        "minecraft.account.xboxTokenFrom" => {
            serde_json::to_value(account::xbox_token_from(field("body")?).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())
        }

        "minecraft.account.xstsRefusal" => Ok(Value::from(account::xsts_refusal(field("body")?))),

        "minecraft.account.minecraftLoginRequest" => {
            let xsts: account::XboxToken = serde_json::from_value(field("xsts")?.clone())
                .map_err(|e| format!("bad xsts token: {e}"))?;
            serde_json::to_value(account::minecraft_login_request(&xsts))
                .map_err(|e| e.to_string())
        }

        "minecraft.account.minecraftTokenFrom" => Ok(Value::from(
            account::minecraft_token_from(field("body")?).map_err(|e| e.to_string())?,
        )),

        "minecraft.account.profileRequest" => {
            serde_json::to_value(account::profile_request(&text("minecraftToken")?))
                .map_err(|e| e.to_string())
        }

        "minecraft.account.sessionFrom" => {
            let msa: account::MsaTokens = serde_json::from_value(field("msa")?.clone())
                .map_err(|e| format!("bad msa tokens: {e}"))?;
            let session = account::session_from(
                field("profile")?,
                &text("minecraftToken")?,
                &msa,
                args.get("nowMs").and_then(Value::as_i64).unwrap_or_default(),
            )
            .map_err(|e| e.to_string())?;
            serde_json::to_value(session).map_err(|e| e.to_string())
        }

        // The only shape of a session that may cross into JavaScript.
        "minecraft.account.redacted" => {
            let session: account::Session = serde_json::from_value(field("session")?.clone())
                .map_err(|e| format!("bad session: {e}"))?;
            Ok(session.redacted())
        }

        "minecraft.account.needsRefresh" => Ok(Value::Bool(account::needs_refresh(
            args.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
            args.get("nowMs").and_then(Value::as_i64).unwrap_or(0),
        ))),

        // --- modpacks -------------------------------------------------------
        //
        // Same shape as `mods`: the core says what to fetch and what the
        // answers meant, the host moves bytes and reads the zip.

        "minecraft.modpack.plan" => {
            serde_json::to_value(modpack::plan(&text("modpack")?)).map_err(|e| e.to_string())
        }

        "minecraft.modpack.sourceFrom" => {
            let of: modpack::Lookup = serde_json::from_value(field("of")?.clone())
                .map_err(|e| format!("bad lookup kind: {e}"))?;
            modpack::source_from(of, field("json")?)
                .map_err(|e| e.to_string())
                .and_then(|source| match source {
                    Some(source) => serde_json::to_value(source).map_err(|e| e.to_string()),
                    None => Ok(Value::Null),
                })
        }

        "minecraft.modpack.fallbackUrl" => Ok(
            match modpack::fallback_versions_url(&text("modpack")?) {
                Some(url) => Value::from(url),
                None => Value::Null,
            },
        ),

        "minecraft.modpack.requires" => modpack::requires(field("manifest")?)
            .and_then(|r| {
                serde_json::to_value(r).map_err(|e| homerun_core::Error::Malformed(e.to_string()))
            })
            .map_err(|e| e.to_string()),

        "minecraft.modpack.begin" => {
            let inputs: modpack::Inputs = serde_json::from_value(field("inputs")?.clone())
                .map_err(|e| format!("bad modpack inputs: {e}"))?;
            serde_json::to_value(modpack::begin(inputs)).map_err(|e| e.to_string())
        }

        "minecraft.modpack.advance" => {
            let session: modpack::Session = serde_json::from_value(field("state")?.clone())
                .map_err(|e| format!("bad modpack session: {e}"))?;
            let replies: Vec<mods::Reply> = args
                .get("replies")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| format!("bad modpack replies: {e}"))?
                .unwrap_or_default();
            serde_json::to_value(modpack::advance(session, replies)).map_err(|e| e.to_string())
        }

        "minecraft.modpack.reconcile" => {
            let jars: Vec<modpack::Assembled> = serde_json::from_value(field("jars")?.clone())
                .map_err(|e| format!("bad assembled jars: {e}"))?;
            Ok(Value::from(modpack::reconcile(&jars)))
        }

        "minecraft.modpack.excluded" => {
            let patterns: Vec<String> = args
                .get("patterns")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let path = text("path")?;
            Ok(Value::Bool(
                patterns.iter().any(|p| modpack::ant_matches(p, &path)),
            ))
        }

        "minecraft.modjar.read" => {
            let tomls: Vec<String> = args
                .get("tomls")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            serde_json::to_value(modjar::read(optional_text("fabric").as_deref(), &tomls))
                .map_err(|e| e.to_string())
        }

        "minecraft.loader.bundlerJavaMajor" => {
            let head: Vec<u8> = args
                .get("head")
                .and_then(|v| v.as_array())
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_u64)
                        .map(|v| v as u8)
                        .collect()
                })
                .unwrap_or_default();
            Ok(match loader::bundler_java_major(&head) {
                Some(major) => Value::from(major),
                None => Value::Null,
            })
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

        // Can this host run this server at all? Answered before a launch is
        // planned, because the expensive half of a launch — fetching and
        // unpacking a modpack — happens before the engine would have a chance
        // to object, and a linked engine never objects at all: it starts
        // vanilla and looks like it worked.
        //
        // `host` is absent-means-conservative on purpose (see `hosting::Host`),
        // so a host that has not been taught this yet refuses rather than
        // launches. `server` is required: guessing it would defeat the point.
        "minecraft.hosting.refuse" => {
            let host: hosting::Host = match args.get("host") {
                Some(v) if !v.is_null() => {
                    serde_json::from_value(v.clone()).map_err(|e| format!("bad host: {e}"))?
                }
                _ => hosting::Host::default(),
            };
            let server: hosting::Server = serde_json::from_value(field("server")?.clone())
                .map_err(|e| format!("bad server: {e}"))?;
            serde_json::to_value(hosting::refuse(host, &server)).map_err(|e| e.to_string())
        }

        // Which of this host's engines runs this server — the routing answer,
        // and the other half of `refuse` above. A host with two backends asks
        // this instead of deciding in Kotlin, because "a Pumpkin server goes
        // to Pumpkin, a Java server prefers a real JVM, and a device with no
        // JVM serves Java with Pumpkin anyway" is three rules that both
        // platforms would otherwise write separately.
        //
        // Answers `{"engine":"jvm"|"pumpkin"|"bedrock"}` or the same refusal
        // shape `minecraft.hosting.refuse` returns, so a caller needs one
        // round trip rather than two.
        "minecraft.hosting.serves" => {
            let host: hosting::Host = match args.get("host") {
                Some(v) if !v.is_null() => {
                    serde_json::from_value(v.clone()).map_err(|e| format!("bad host: {e}"))?
                }
                _ => hosting::Host::default(),
            };
            let server: hosting::Server = serde_json::from_value(field("server")?.clone())
                .map_err(|e| format!("bad server: {e}"))?;
            Ok(match hosting::serves(host, &server) {
                Ok(engine) => json!({ "engine": engine.as_str(), "refusal": Value::Null }),
                Err(refusal) => json!({
                    "engine": Value::Null,
                    "refusal": serde_json::to_value(refusal).map_err(|e| e.to_string())?,
                }),
            })
        }

        // Whether a launch has a jar and a `Main-Class` to resolve, from the
        // one thing that knows: the game type. Feeds `needsJvm` above.
        "minecraft.hosting.needsJvm" => Ok(Value::Bool(hosting::needs_jvm(&text("gameType")?))),

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

        // --- the device websocket -----------------------------------------
        //
        // A device link, not a server one: it arrives flat from
        // `link_up`, and null means the task is still running rather than
        // that anything failed. See `homerun_core::device_ws`.
        "deviceWs.fromLinkUpBody" => Ok(match device_ws::from_link_up_body(field("body")?) {
            Some(device) => serde_json::to_value(device).map_err(|e| e.to_string())?,
            None => Value::Null,
        }),

        // `httpTarget` absent or null omits the ACME forward — the shape a
        // device serving without a certificate takes.
        "deviceWs.tunnelConfig" => {
            let link: tunnel::Link = serde_json::from_value(field("link")?.clone())
                .map_err(|e| format!("bad link: {e}"))?;
            let port = |name: &str| args.get(name).and_then(|v| v.as_u64()).map(|p| p as u16);
            let https = port("httpsTarget").ok_or("httpsTarget is required")?;
            Ok(Value::String(
                device_ws::tunnel_config(link, https, port("httpTarget")).render(),
            ))
        }

        // --- over-the-air UI bundles ---------------------------------------
        //
        // Verifying and judging are **one call** on purpose. Two calls would
        // let a host judge a manifest it had not verified, and that mistake
        // has no symptom: everything keeps working, against any manifest
        // anyone serves. `bundle::verify` is the only way to obtain a
        // `Manifest`, and this is the only way to reach it.
        "bundle.evaluate" => {
            let installed: bundle::Installed =
                serde_json::from_value(field("installed")?.clone())
                    .map_err(|e| format!("bad installed record: {e}"))?;
            let manifest = bundle::verify(&text("manifest")?, &text("publicKey")?)
                .map_err(|e| e.to_string())?;
            let verdict = bundle::judge(&manifest, &installed);
            Ok(json!({
                "manifest": serde_json::to_value(&manifest).map_err(|e| e.to_string())?,
                "verdict": serde_json::to_value(&verdict).map_err(|e| e.to_string())?,
                // Rendered here rather than in each host, so the sentence in
                // an Android log and an iOS log is the same sentence.
                "reason": verdict.reason(),
                "install": verdict.should_install(),
            }))
        }

        // The host hashes the archive it downloaded — see the module docs in
        // `homerun_core::bundle` for why that is not done here — and this says
        // whether it is the digest that was signed.
        "bundle.digestMatches" => Ok(Value::Bool(bundle::digest_matches(
            &text("expected")?,
            &text("actual")?,
        ))),

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
                // Absent means "infer it from the engine", which is what
                // every host did before a Pumpkin server could be spawned.
                // Only a host that knows the game type can answer this, and
                // only Android needs to: a spawned Pumpkin looks exactly like
                // a JVM server to `engine` and has no jar to fetch.
                needs_jvm: args.get("needsJvm").and_then(|v| v.as_bool()),
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

        // --- reporting ----------------------------------------------------
        //
        // What the API is told about a run. Every arm here answers with a
        // `Request` the host performs verbatim: it picks no path, builds no
        // body, and above all does not decide which credential signs it —
        // getting that wrong is either a silent 403 or a report filed against
        // the wrong person.
        "reporting.crash.diagnose" => {
            let lines = console_lines(&args, method)?;
            // Absent means a first attempt. The budget is the host's to keep
            // — it knows whether a launch ever reached running — and the
            // core's to interpret.
            let used = args
                .get("retriesUsed")
                .and_then(Value::as_u64)
                .unwrap_or_default() as u32;
            serde_json::to_value(crash::diagnose(&lines, used)).map_err(|e| e.to_string())
        }

        "reporting.crash.report" => serde_json::to_value(crash::report(
            &text("serverId")?,
            &text("deviceId")?,
            &console_lines(&args, method)?,
        ))
        .map_err(|e| e.to_string()),

        // -- app error reporting --------------------------------------------
        //
        // Four arms and one funnel. Every intake on every platform — a React
        // error boundary, a rejected promise, an API failure, a Kotlin or
        // Swift uncaught exception, a panic in this crate — arrives at
        // `error.report`, or is stashed for the next launch to drain. One
        // ledger sees all of them, which is the only way the caps mean
        // anything; see `crate::errors`.
        "error.attach" => Ok(crate::errors::attach(&text("dataDir")?)),

        "error.report" => {
            let context = context_arg(field("context")?, method)?;
            let seen = occurrence_arg(field("occurrence")?, method)?;
            Ok(crate::errors::report(&context, &seen))
        }

        "error.stash" => {
            let context = context_arg(field("context")?, method)?;
            let seen = occurrence_arg(field("occurrence")?, method)?;
            crate::errors::stash(&context, &seen)
        }

        "error.drain" => {
            let context = context_arg(field("context")?, method)?;
            Ok(crate::errors::drain(&context))
        }

        "reporting.stats.report" => {
            let stats: stats::Stats = serde_json::from_value(field("stats")?.clone())
                .map_err(|e| format!("\"{method}\" got stats it could not read: {e}"))?;
            serde_json::to_value(stats::report(
                &text("serviceId")?,
                &text("deviceId")?,
                &stats,
            ))
            .map_err(|e| e.to_string())
        }

        "reporting.stats.parseRoster" => {
            serde_json::to_value(stats::parse_list_uuids(&text("reply")?))
                .map_err(|e| e.to_string())
        }

        "reporting.stats.parseServerAge" => Ok(stats::parse_server_age(&text("reply")?).into()),

        // Which spelling of a command survives a plugin that shadows it.
        "reporting.stats.pinned" => {
            let loader = jar::Loader::parse(optional_text("loader").as_deref())
                .map_err(|e| e.to_string())?;
            Ok(Value::from(stats::pinned(&text("command")?, loader)))
        }

        // Per-core in, percent-of-device out. The two scales agree on a
        // single-core reading, which is why a host that skips this looks
        // right in testing and reports a melting phone in the field.
        "reporting.stats.cpuPercentOfDevice" => {
            let per_core = field("perCorePercent")?
                .as_f64()
                .ok_or_else(|| format!("\"{method}\" needs perCorePercent as a number"))?;
            let cores = args
                .get("cores")
                .and_then(Value::as_u64)
                .unwrap_or_default() as u32;
            Ok(stats::cpu_percent_of_device(per_core, cores).into())
        }

        // The cadence, held by the host between calls like `metrics.record`.
        // No schedule means a run that has just started, and a fresh one is
        // due immediately — the desktop reports the moment a server is up.
        "reporting.stats.schedule" => {
            let now = field("nowMs")?
                .as_i64()
                .ok_or_else(|| format!("\"{method}\" needs nowMs as a number"))?;
            let mut schedule = match args.get("schedule") {
                Some(held) if !held.is_null() => serde_json::from_value(held.clone())
                    .map_err(|e| format!("\"{method}\" got a schedule it could not read: {e}"))?,
                _ => stats::Schedule::started(stats::Cadence::default(), now),
            };

            let trigger = match optional_text("event").as_deref() {
                // A join or a leave. Coalesced, so a party arriving together
                // is one report rather than six.
                Some("presence") => {
                    schedule.presence(now);
                    None
                }
                Some("poll") | None => schedule.poll(now),
                Some(other) => {
                    return Err(format!("\"{method}\" does not know the event {other:?}"))
                }
            };

            Ok(json!({
                "schedule": serde_json::to_value(schedule).map_err(|e| e.to_string())?,
                "trigger": serde_json::to_value(trigger).map_err(|e| e.to_string())?,
                "waitMs": schedule.wait_ms(now),
                "nextAtMs": schedule.next_at_ms(),
            }))
        }

        // A line a server plugin printed for us. Nothing else in the console
        // is looked at here.
        "reporting.minigame.fromLine" => serde_json::to_value(minigame::from_console_line(
            &text("serverId")?,
            &text("line")?,
        ))
        .map_err(|e| e.to_string()),

        // --- ops and bans typed into the console ----------------------------
        //
        // `ops.json` is rewritten from the API at every launch, so an operator
        // granted only in the console loses it on the next start unless this
        // runs. Two calls: recognise the command, then decide against what the
        // API currently holds. The host does the GET in between, and must
        // serialise the pair per server — two rapid commands would otherwise
        // each read the list before either wrote it.
        "minecraft.ops.parse" => {
            serde_json::to_value(ops::parse(&text("command")?)).map_err(|e| e.to_string())
        }

        "minecraft.ops.sync" => {
            let command: ops::Command = serde_json::from_value(field("command")?.clone())
                .map_err(|e| format!("\"{method}\" got a command it could not read: {e}"))?;
            serde_json::to_value(ops::sync(&command, field("server")?, &text("serverId")?))
                .map_err(|e| e.to_string())
        }

        // Where a player connects — the gateway's name and the external port
        // it assigned, which is the only address worth measuring latency to.
        // Java's listen port, because that is the only kind of server this
        // build hosts; a Bedrock backend would have to ask for 19132/udp.
        "link.publicAddress" => {
            Ok(link::public_address(field("body")?, minecraft::LISTEN_JAVA, "tcp").into())
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

    // ─── app error reporting ────────────────────────────────────────────────

    /// Serialise against the process-global ledger and start from empty.
    ///
    /// Shared with `crate::errors`' own tests through the same lock — the
    /// ledger is one per process on purpose, so the tests over it have to be
    /// one at a time.
    fn error_test_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::crash::test_guard();
        crate::errors::reset_ledger();
        guard
    }

    fn error_context() -> Value {
        json!({
            "deviceId": "device-1",
            "session": "session-1",
            "platform": "android",
            "appVersion": "0.4.2",
            "apiUrl": "https://api.gethomerun.app",
        })
    }

    #[test]
    fn an_error_report_comes_back_as_a_request_the_host_can_sign() {
        let _guard = error_test_guard();
        // The shape both hosts parse. `Reporting.send` reads `auth` to pick
        // between the device token and the user token, and a report signed
        // with the wrong one is a silent 403.
        let value = ok(
            "error.report",
            json!({
                "context": error_context(),
                "occurrence": {
                    "source": "ui",
                    "severity": "fatal",
                    "kind": "TypeError",
                    "message": "cannot read properties of undefined",
                    "stack": "    at ServerCard (https://h/_next/static/chunks/a.js:1:2)",
                    "atMs": 1_755_640_000_000i64,
                },
            }),
        );

        assert_eq!(value["request"]["method"], "post");
        assert_eq!(value["request"]["path"], "/api/app-error/");
        assert_eq!(value["request"]["auth"], "device");
        assert_eq!(value["request"]["body"]["source"], "ui");
        assert!(value["held"].is_null(), "{value}");
    }

    #[test]
    fn a_held_report_is_a_success_with_no_request_rather_than_an_error() {
        let _guard = error_test_guard();
        // A hold is the common case by design. A host that saw it as a
        // failure would log a warning per sighting and reproduce, in the
        // log, exactly the flood the ledger just prevented on the network.
        let occurrence = json!({
            "source": "host",
            "severity": "error",
            "kind": "Repeat",
            "message": "the very same thing, twice",
            "atMs": 1_755_640_000_000i64,
        });
        let args = json!({ "context": error_context(), "occurrence": occurrence });

        ok("error.report", args.clone());
        let second = ok("error.report", args);

        assert!(second["request"].is_null(), "{second}");
        assert_eq!(second["held"], "cooldown");
    }

    #[test]
    fn a_context_the_core_cannot_read_is_refused_rather_than_defaulted() {
        let _guard = error_test_guard();
        // Defaulting would file the report against an empty device and no
        // version — a row nobody can act on that looks like a success.
        let message = err(
            "error.report",
            json!({
                "context": { "appVersion": 42 },
                "occurrence": { "message": "boom", "atMs": 1i64 },
            }),
        );
        assert!(message.contains("context"), "{message}");
    }

    #[test]
    fn draining_answers_even_when_no_host_has_attached_a_directory() {
        let _guard = error_test_guard();
        let value = ok("error.drain", json!({ "context": error_context() }));
        assert!(value["requests"].is_array(), "{value}");
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

    /// A still-running `link_up` task must read as "not yet", not as a
    /// failure. A host that treats it as one abandons a link that was seconds
    /// from being provisioned, and the device never becomes reachable.
    #[test]
    fn a_device_link_that_is_not_ready_replies_null_rather_than_failing() {
        assert_eq!(
            ok(
                "deviceWs.fromLinkUpBody",
                json!({ "body": { "fqdn": "d.example.com" } })
            ),
            Value::Null
        );
    }

    #[test]
    fn a_device_link_crosses_the_boundary_whole() {
        let device = ok(
            "deviceWs.fromLinkUpBody",
            json!({ "body": {
                "fqdn": "d.example.com",
                "gateway_version": 2,
                "native_config": {
                    "client_privkey": "K",
                    "gateway_pubkey": "P",
                    "link_address": "gw:51820",
                    "address": "10.8.0.7/32"
                }
            }}),
        );
        assert_eq!(device["fqdn"], "d.example.com");
        assert_eq!(device["gateway_v2"], true);
        assert_eq!(device["link"]["client_privkey"], "K");
    }

    #[test]
    fn the_device_tunnel_forwards_the_two_ports_the_gateway_dnats_to() {
        let config = ok(
            "deviceWs.tunnelConfig",
            json!({
                "link": {
                    "client_privkey": "K",
                    "gateway_pubkey": "P",
                    "link_address": "gw:51820"
                },
                "httpsTarget": 8444,
                "httpTarget": 8081
            }),
        );
        let text = config.as_str().unwrap();
        assert!(text.contains("ListenPort = 8443\nTarget = 127.0.0.1:8444"));
        assert!(text.contains("ListenPort = 8080\nTarget = 127.0.0.1:8081"));
    }

    /// Serving without a certificate is a real state, and it must not leave a
    /// forward pointing at a listener that was never started.
    #[test]
    fn a_device_tunnel_without_a_cert_manager_omits_the_challenge_forward() {
        let config = ok(
            "deviceWs.tunnelConfig",
            json!({
                "link": {
                    "client_privkey": "K",
                    "gateway_pubkey": "P",
                    "link_address": "gw:51820"
                },
                "httpsTarget": 4000
            }),
        );
        let text = config.as_str().unwrap();
        assert!(text.contains("ListenPort = 8443"));
        assert!(!text.contains("ListenPort = 8080"));
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

    // ─── crossplay ──────────────────────────────────────────────────────────

    /// The four calls a launch makes, in the order it makes them, over the
    /// wire a host actually sends. The point is the JSON shapes: a host reads
    /// `metaUrl`, `flavour`, `fileName` and `sha256` by name, and a rename here
    /// is a silent no-op at the other end rather than a compile error.
    #[test]
    fn a_paper_crossplay_launch_gets_all_four_answers() {
        let projects = ok(
            "minecraft.crossplay.mergeProjects",
            json!({ "gameType": "native-crossplay", "loader": "paper", "configured": "worldedit" }),
        );
        assert_eq!(
            projects,
            json!(
                "worldedit
geyser"
            )
        );

        let floodgate = ok(
            "minecraft.crossplay.floodgate",
            json!({ "gameType": "native-crossplay", "loader": "paper" }),
        );
        assert_eq!(floodgate["flavour"], "spigot");
        assert!(floodgate["metaUrl"]
            .as_str()
            .unwrap()
            .starts_with("https://download.geysermc.org/"));

        let fetch = ok(
            "minecraft.crossplay.floodgateBuild",
            json!({
                "meta": {
                    "version": "2.2.5",
                    "build": 140,
                    "downloads": {
                        "spigot": { "name": "floodgate-spigot.jar", "sha256": "9f43" }
                    }
                },
                "flavour": "spigot"
            }),
        );
        assert_eq!(fetch["fileName"], "floodgate-spigot.jar");
        assert_eq!(fetch["sha256"], "9f43");
        assert_eq!(fetch["subDir"], "plugins");

        assert_eq!(
            ok(
                "minecraft.exposure",
                json!({ "gameType": "native-crossplay" })
            ),
            json!("crossplay")
        );

        let config = ok(
            "minecraft.crossplay.config",
            json!({ "gameType": "native-crossplay", "loader": "paper" }),
        );
        assert_eq!(config["path"], "plugins/Geyser-Spigot/config.yml");
        assert!(config["contents"].as_str().unwrap().contains("19132"));
    }

    /// **`null`, not an error, and not an empty object.** Every one of these is
    /// called on every Java launch, so the ordinary answer for the ordinary
    /// server has to be cheap and unmistakable — a host that read an error here
    /// would log a failure on every start of every server that is not crossplay.
    #[test]
    fn an_ordinary_java_server_is_told_there_is_nothing_to_do() {
        assert_eq!(
            ok(
                "minecraft.crossplay.mergeProjects",
                json!({ "gameType": "native", "loader": "paper", "configured": "sodium" })
            ),
            json!("sodium")
        );
        assert_eq!(
            ok(
                "minecraft.crossplay.floodgate",
                json!({ "gameType": "native", "loader": "paper" })
            ),
            Value::Null
        );
        assert_eq!(
            ok(
                "minecraft.crossplay.config",
                json!({ "gameType": "native", "loader": "paper" })
            ),
            Value::Null
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

    // --- over-the-air UI bundles ------------------------------------------

    /// Sign a manifest the way the publish workflow will, so these tests
    /// exercise the real payload rather than a fixture that would keep
    /// passing after the signed bytes changed.
    fn signed_manifest(mutate: impl FnOnce(&mut Value)) -> (String, String) {
        use ed25519_dalek::{Signer, SigningKey};

        let key = SigningKey::from_bytes(&[3u8; 32]);
        let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };

        let mut manifest = json!({
            "bundle": "2026-08-14.1",
            "url": "https://cdn.gethomerun.app/ui/2026-08-14.1.zip",
            "sha256": "b".repeat(64),
            "minHost": 1,
            "serial": 5,
            "platform": "android",
        });
        mutate(&mut manifest);

        // Must match `Manifest::signing_payload` exactly. Building it here by
        // hand rather than importing the helper is deliberate: if the two ever
        // disagree, this is the test that says so.
        let field = |name: &str| manifest[name].as_str().unwrap_or_default().to_string();
        let number = |name: &str| manifest[name].as_u64().unwrap_or_default().to_string();
        let payload = format!(
            "homerun-bundle-v1\n{}\n{}\n{}\n{}\n{}\n{}\n",
            field("bundle"),
            field("url"),
            field("sha256"),
            number("minHost"),
            number("serial"),
            field("platform"),
        );

        manifest["signature"] = json!(hex(&key.sign(payload.as_bytes()).to_bytes()));
        (
            manifest.to_string(),
            hex(key.verifying_key().as_bytes()),
        )
    }

    fn installed() -> Value {
        json!({ "bundle": null, "serial": 0, "hostRevision": 1, "platform": "android" })
    }

    #[test]
    fn a_signed_manifest_crosses_the_boundary_and_says_install() {
        let (manifest, public_key) = signed_manifest(|_| {});
        let reply = ok(
            "bundle.evaluate",
            json!({ "manifest": manifest, "publicKey": public_key, "installed": installed() }),
        );
        assert_eq!(reply["install"], true);
        assert_eq!(reply["verdict"]["verdict"], "install");
        assert_eq!(reply["manifest"]["bundle"], "2026-08-14.1");
        assert_eq!(
            reply["manifest"]["url"],
            "https://cdn.gethomerun.app/ui/2026-08-14.1.zip"
        );
    }

    /// The reason a host must never be handed a parsed manifest it has not
    /// verified: this reply is the *only* place it can get one.
    #[test]
    fn a_forged_manifest_yields_no_manifest_at_all() {
        let (manifest, public_key) = signed_manifest(|_| {});
        let forged = manifest.replace("2026-08-14.1.zip", "2026-08-14.9.zip");
        let message = err(
            "bundle.evaluate",
            json!({ "manifest": forged, "publicKey": public_key, "installed": installed() }),
        );
        assert!(message.contains("signature"), "{message}");
    }

    /// A declined bundle is not an error — the host logs the reason and waits.
    #[test]
    fn a_bundle_this_host_cannot_run_is_declined_with_a_sentence() {
        let (manifest, public_key) = signed_manifest(|m| m["minHost"] = json!(9));
        let reply = ok(
            "bundle.evaluate",
            json!({ "manifest": manifest, "publicKey": public_key, "installed": installed() }),
        );
        assert_eq!(reply["install"], false);
        assert_eq!(reply["verdict"]["verdict"], "tooNew");
        let reason = reply["reason"].as_str().unwrap();
        assert!(reason.contains('9') && reason.contains('1'), "{reason}");
    }

    #[test]
    fn digests_are_compared_in_one_place() {
        let matches = |expected: &str, actual: &str| {
            ok(
                "bundle.digestMatches",
                json!({ "expected": expected, "actual": actual }),
            )
        };
        assert_eq!(matches(&"a".repeat(64), &"A".repeat(64)), json!(true));
        assert_eq!(matches(&"a".repeat(64), &"c".repeat(64)), json!(false));
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

    /// What a host asks before it starts fetching a modpack it cannot run.
    #[test]
    fn a_host_can_ask_whether_it_may_host_a_server() {
        let ios = json!({ "engine": "linked", "bedrock": false });
        let android = json!({ "engine": "spawned", "bedrock": false });

        // Null is "go ahead" on the wire, so a host branches on presence.
        let fine = ok(
            "minecraft.hosting.refuse",
            json!({ "host": ios, "server": { "gameType": "native", "env": {} } }),
        );
        assert!(fine.is_null(), "vanilla java on a linked engine is fine: {fine}");

        let modded = ok(
            "minecraft.hosting.refuse",
            json!({ "host": ios, "server": { "gameType": "native", "env": { "TYPE": "FORGE" } } }),
        );
        assert_eq!(modded["code"], "mods-unsupported");
        assert!(modded["message"].as_str().unwrap().contains("mods or plugins"));

        // The same server, on the host that ships a JVM.
        let allowed = ok(
            "minecraft.hosting.refuse",
            json!({ "host": android, "server": { "gameType": "native", "env": { "TYPE": "FORGE" } } }),
        );
        assert!(allowed.is_null(), "android runs mods: {allowed}");

        // An omitted host refuses rather than launching something it may not
        // be able to honour.
        let cautious = ok(
            "minecraft.hosting.refuse",
            json!({ "server": { "gameType": "native", "env": { "TYPE": "PAPER" } } }),
        );
        assert_eq!(cautious["code"], "mods-unsupported");

        // The server is required — guessing it would defeat the check.
        assert!(err("minecraft.hosting.refuse", json!({ "host": ios })).contains("server"));
    }

    /// The loop a host implements, on the wire: ask, read, record, read back.
    /// The routing answer both Android backends now depend on. A device with
    /// two engines asks once and gets the engine *and* the refusal.
    #[test]
    fn the_core_routes_a_server_to_an_engine() {
        let android = json!({ "jvm": true, "pumpkin": true, "bedrock": false });

        let paper = ok(
            "minecraft.hosting.serves",
            json!({ "host": android, "server": { "gameType": "native", "env": { "TYPE": "PAPER" } } }),
        );
        assert_eq!(paper["engine"], "jvm");
        assert!(paper["refusal"].is_null());

        let pumpkin = ok(
            "minecraft.hosting.serves",
            json!({ "host": android, "server": { "gameType": "native-pumpkin", "env": {} } }),
        );
        assert_eq!(pumpkin["engine"], "pumpkin");

        // A Pumpkin server is never quietly substituted with the JVM, even
        // though this device has one and a JVM is the better engine.
        let modded = ok(
            "minecraft.hosting.serves",
            json!({
                "host": android,
                "server": { "gameType": "native-pumpkin", "env": { "TYPE": "FABRIC" } },
            }),
        );
        assert!(modded["engine"].is_null());
        assert_eq!(modded["refusal"]["code"], "mods-unsupported");
    }

    /// iOS sends the field this struct used to have and must keep the answers
    /// it has always had, without being rebuilt.
    #[test]
    fn a_host_naming_only_its_engine_is_still_understood() {
        let served = ok(
            "minecraft.hosting.serves",
            json!({
                "host": { "engine": "linked", "bedrock": false },
                "server": { "gameType": "native", "env": {} },
            }),
        );
        assert_eq!(served["engine"], "pumpkin");

        let refused = ok(
            "minecraft.hosting.refuse",
            json!({
                "host": { "engine": "linked", "bedrock": false },
                "server": { "gameType": "native", "env": { "TYPE": "FORGE" } },
            }),
        );
        assert_eq!(refused["code"], "mods-unsupported");
    }

    /// What feeds `needsJvm` into a launch plan. Getting this wrong sends a
    /// Pumpkin server to download a Mojang jar it cannot run.
    #[test]
    fn only_a_java_server_is_planned_around_a_jar() {
        for (game_type, expected) in [
            ("native", true),
            ("native-crossplay", true),
            ("native-pumpkin", false),
            ("native-bedrock", false),
        ] {
            assert_eq!(
                ok("minecraft.hosting.needsJvm", json!({ "gameType": game_type })),
                json!(expected),
                "{game_type}",
            );
        }

        // And the plan actually drops the two steps when told.
        let steps = ok(
            "launch.plan",
            json!({ "backups": false, "settings": true, "tunnel": false, "needsJvm": false }),
        );
        let names: Vec<&str> = steps
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["step"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"ensureJar"), "{names:?}");
        assert!(!names.contains(&"resolveMainClass"), "{names:?}");
        assert!(names.contains(&"ensureRuntime"), "{names:?}");
    }

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

    /// The mod driver survives the JSON boundary, which is the risky part.
    ///
    /// `minecraft::mods` tests the pipeline directly, in Rust. What that
    /// cannot catch is a session that resolves perfectly and then fails to
    /// round-trip: the host holds the state as opaque JSON and hands it back,
    /// so a field that does not serialise breaks the second call and nothing
    /// earlier. This drives a whole install through `homerun_core_call`.
    #[test]
    fn a_mod_install_round_trips_through_the_json_boundary() {
        let mut progress = ok(
            "minecraft.mods.begin",
            json!({ "inputs": {
                "loader": "fabric",
                "gameVersion": "1.21.4",
                "projects": "lithium",
            }}),
        );
        assert_eq!(progress["kind"], "steps");

        // Resolve, sides, download — the host answers each batch in turn.
        for _ in 0..8 {
            if progress["kind"] == "done" {
                break;
            }
            let replies: Vec<serde_json::Value> = progress["steps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|step| {
                    let id = step["id"].as_str().unwrap();
                    let body = match step["kind"].as_str().unwrap() {
                        "download" => serde_json::Value::Null,
                        _ if step["url"].as_str().unwrap().contains("/projects?ids=") => {
                            json!([{ "id": "lith01", "server_side": "required" }])
                        }
                        _ => json!([{
                            "id": "v1",
                            "project_id": "lith01",
                            "version_type": "release",
                            "files": [{
                                "primary": true,
                                "url": "https://cdn/lithium.jar",
                                "filename": "lithium.jar",
                            }],
                        }]),
                    };
                    json!({ "id": id, "json": body })
                })
                .collect();

            progress = ok(
                "minecraft.mods.advance",
                json!({ "state": progress["state"], "replies": replies }),
            );
        }

        assert_eq!(progress["kind"], "done", "{progress}");
        let outcome = &progress["outcome"];
        assert_eq!(outcome["installed"][0], "lithium");
        assert_eq!(outcome["subDir"], "mods");
        assert_eq!(
            outcome["records"]["lithium"]["filePath"],
            "mods/lithium.jar"
        );
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

    /// The contract the Android host ships must advertise exactly the loaders
    /// this library will accept at launch.
    ///
    /// `HostCapabilities.serverLoaders` is a hand-written list in Kotlin,
    /// mirrored from `bridge-v1.json`, and `scripts/check-capabilities.js`
    /// checks those two against each other — but nothing checked either against
    /// the code that does the refusing. That gap is how the create flow came to
    /// offer Spigot and Quilt on a phone while `Loader::parse` refused both.
    ///
    /// This closes it from the Rust side, where the answer actually lives. The
    /// contract is read off disk rather than restated, so the assertion is
    /// against the bytes that ship.
    #[test]
    fn the_android_contract_advertises_exactly_the_loaders_this_core_hosts() {
        const CONTRACT: &str = include_str!("../../../shared/conformance/bridge-v1.json");
        let doc: Value = serde_json::from_str(CONTRACT).expect("the contract is JSON");

        let advertised = doc["profiles"]["android"]["capabilities"]["serverLoaders"]
            .as_array()
            .expect("android advertises serverLoaders");

        let hostable = ok("minecraft.loader.hostable", json!({}));
        assert_eq!(
            advertised,
            hostable.as_array().unwrap(),
            "the Android contract and the core disagree about hostable loaders"
        );

        // And the whole point of the exercise: the two BuildTools loaders are
        // not in it, so the UI stops offering them.
        for refused in ["spigot", "bukkit"] {
            assert!(
                !advertised.iter().any(|l| l == refused),
                "{refused} is still advertised to the Android UI"
            );
        }
    }

    // ─── minigames ──────────────────────────────────────────────────────────

    /// The three calls a launch makes, over one server's env, in the order the
    /// host makes them. Together they are the whole minigame contract across
    /// the FFI, so a change to any of them fails here rather than on a phone.
    #[test]
    fn a_minigame_servers_env_yields_its_jars_its_flag_and_its_plugin_env() {
        let env = json!({
            "TYPE": "PAPER",
            "VERSION": "1.21.4",
            "MINIGAME": "bedwars",
            "MINIGAME_MIN_PLAYERS": "4",
            "CUSTOM_PLUGINS":
                "https://api.gethomerun.app/api/minigame/plugins/homerun-minigames/download/?channel=release",
        });

        assert_eq!(ok("minecraft.minigame.isMinigame", json!({ "env": env })), true);

        let plugins = ok(
            "minecraft.minigame.customPlugins",
            json!({ "loader": "paper", "env": env }),
        );
        assert_eq!(plugins.as_array().unwrap().len(), 1);
        assert_eq!(plugins[0]["filename"], "homerun-plugin-dafc06fcd9c7.jar");

        let forwarded = ok("minecraft.minigame.pluginEnv", json!({ "env": env }));
        assert_eq!(forwarded["MINIGAME_MIN_PLAYERS"], "4");
        // The rule that matters: nothing outside our own namespace crosses
        // into the server process's environment.
        assert_eq!(forwarded.as_object().unwrap().len(), 2);
        assert!(forwarded.get("VERSION").is_none());
    }

    /// An ordinary world asks for none of it, and says so without an error —
    /// the host calls these on every launch, not only on minigame launches.
    #[test]
    fn an_ordinary_server_is_not_a_minigame_and_wants_no_plugins() {
        let env = json!({ "TYPE": "PAPER", "VERSION": "1.21.4" });

        assert_eq!(ok("minecraft.minigame.isMinigame", json!({ "env": env })), false);
        assert_eq!(
            ok("minecraft.minigame.customPlugins", json!({ "loader": "paper", "env": env })),
            json!([])
        );
        assert_eq!(ok("minecraft.minigame.pluginEnv", json!({ "env": env })), json!({}));
    }

    // ─── Minecraft accounts ─────────────────────────────────────────────────

    /// The whole sign-in, driven across the FFI exactly as the host drives it.
    ///
    /// Each step alone is covered in `minecraft::account`; what this adds is
    /// that the output of one call is accepted as the input of the next
    /// *through JSON*. That is the seam a host actually runs on, and a field
    /// renamed on one side of it would pass every unit test in the core.
    #[test]
    fn a_sign_in_can_be_driven_one_call_at_a_time_across_the_bridge() {
        // 1. Start it, and read Microsoft's answer.
        let start = ok("minecraft.account.deviceCodeRequest", json!({}));
        assert_eq!(start["url"], "https://login.live.com/oauth20_connect.srf");

        let code = ok(
            "minecraft.account.deviceCodeFrom",
            json!({ "body": {
                "user_code": "MWNYUL2R",
                "device_code": "secret-half",
                "verification_uri": "https://www.microsoft.com/link",
                "interval": 5,
                "expires_in": 900,
            }}),
        );
        assert_eq!(code["approvalUrl"], "https://www.microsoft.com/link?otc=MWNYUL2R");

        // 2. Poll — waiting is the normal answer and must not read as failure.
        let poll = ok(
            "minecraft.account.pollRequest",
            json!({ "deviceCode": code["deviceCode"] }),
        );
        assert!(poll["body"].as_str().unwrap().contains("secret-half"));
        assert_eq!(
            ok("minecraft.account.pollOutcome", json!({ "body": { "error": "authorization_pending" }}))["kind"],
            "pending",
        );

        // 3. Approved.
        let approved = ok(
            "minecraft.account.pollOutcome",
            json!({ "body": {
                "access_token": "ms-access",
                "refresh_token": "ms-refresh",
                "expires_in": 3600,
            }}),
        );
        assert_eq!(approved["kind"], "approved");
        let msa = &approved["fields"];
        // serde's adjacent tagging puts the payload under the variant's own
        // key; take it however it landed rather than assuming.
        let msa = if msa.is_null() { &approved } else { msa };
        assert_eq!(msa["accessToken"], "ms-access");

        // 4. Xbox Live, then XSTS.
        let xbl = ok(
            "minecraft.account.xblRequest",
            json!({ "msaAccessToken": "ms-access" }),
        );
        assert!(xbl["body"].as_str().unwrap().contains("d=ms-access"));

        let xbox = ok(
            "minecraft.account.xboxTokenFrom",
            json!({ "body": { "Token": "xbl", "DisplayClaims": { "xui": [{ "uhs": "hash" }] } }}),
        );
        assert_eq!(xbox["userHash"], "hash");

        let login = ok("minecraft.account.minecraftLoginRequest", json!({ "xsts": xbox }));
        assert!(login["body"]
            .as_str()
            .unwrap()
            .contains("XBL3.0 x=hash;xbl"));

        // 5. Profile in, session out.
        let session = ok(
            "minecraft.account.sessionFrom",
            json!({
                "profile": { "id": "069a79f444e94726a5befca90e38aaf5", "name": "Notch" },
                "minecraftToken": "header.eyJ4dWlkIjoiMjUzNTQyODM5NCJ9.sig",
                "msa": { "accessToken": "ms-access", "refreshToken": "ms-refresh", "expiresInSecs": 3600 },
                "nowMs": 1_700_000_000_000i64,
            }),
        );
        assert_eq!(session["uuid"], "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        assert_eq!(session["xuid"], "2535428394");

        // 6. And the only shape of it the web view is allowed to see.
        let view = ok("minecraft.account.redacted", json!({ "session": session }));
        assert_eq!(view["accessToken"], "0");
        assert_eq!(view["refreshToken"], "0");
        assert_eq!(view["username"], "Notch");
    }

    /// The account-shaped failures, which are the common ones and the only ones
    /// a player can do anything about.
    #[test]
    fn an_xbox_refusal_crosses_the_bridge_as_something_a_player_can_act_on() {
        let message = ok(
            "minecraft.account.xstsRefusal",
            json!({ "body": { "XErr": 2148916233u64 }}),
        );
        assert!(message.as_str().unwrap().contains("xbox.com"));
    }

    /// Minigames ship as Paper plugins. A contract that offers to host one on a
    /// host that will not start Paper is not a contract anybody can honour, and
    /// the two flags live far enough apart that nothing else would catch it.
    #[test]
    fn a_host_that_advertises_minigames_advertises_the_loader_they_run_on() {
        const CONTRACT: &str = include_str!("../../../shared/conformance/bridge-v1.json");
        let doc: Value = serde_json::from_str(CONTRACT).expect("the contract is JSON");

        for (name, profile) in doc["profiles"].as_object().expect("profiles is an object") {
            let capabilities = &profile["capabilities"];
            if capabilities["minigames"] != json!(true) {
                continue;
            }
            let loaders = capabilities["serverLoaders"].as_array();
            assert!(
                // An absent list means "no narrowing" — every loader — which
                // includes Paper. Present, it has to say so.
                loaders.is_none_or(|l| l.iter().any(|loader| loader == "paper")),
                "{name} offers to host minigames but does not advertise Paper"
            );
            assert_eq!(
                capabilities["moddedServers"],
                json!(true),
                "{name} offers to host minigames with plugins switched off"
            );
        }
    }
}

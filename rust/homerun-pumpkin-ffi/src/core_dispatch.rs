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
use homerun_core::minecraft::{self, jar, settings};
use homerun_core::{backup, game, link, properties, state, tunnel};

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

        other => Err(format!("the native core has no method \"{other}\"")),
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

    /// An unknown method names itself, so a host that forgot to rebuild the
    /// native library learns why rather than seeing a blank failure.
    #[test]
    fn an_unknown_method_names_itself() {
        let message = err("settings.fromEnv", json!({}));
        assert!(
            message.contains("settings.fromEnv"),
            "the message must name the method: {message}"
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
}

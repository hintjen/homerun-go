//! The Android host's way into `homerun-core`.
//!
//! # One entry point on purpose
//!
//! Every call goes through `nativeCall(method, argsJson)` and comes back as
//! `{"ok":true,"value":…}` or `{"ok":false,"error":"…"}`. A dozen mangled
//! `Java_…` symbols would be faster by a few microseconds and would mean a
//! dozen places to keep two languages agreeing about argument order. This way
//! Kotlin has one parse path, adding a call is one match arm, and the method
//! names are checked at runtime against a list rather than at link time
//! against a symbol table.
//!
//! The cost is real but small: a JSON round trip per call. The busiest caller
//! is console-line parsing at a few hundred lines a second during world
//! generation, which is nothing next to the JVM producing them.
//!
//! # Panics must not cross this boundary
//!
//! A panic unwinding through JNI aborts the VM, which on a phone is the whole
//! app — the same reason `jni_bridge` calls the C functions rather than
//! reaching into the engine. Everything here runs inside `catch_unwind`, and a
//! panic becomes an error string like any other failure.

use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;
use serde_json::{json, Value};

use homerun_core::game::Game as _;
use homerun_core::minecraft::{self, jar, settings};
use homerun_core::{game, link, properties, state, tunnel};

/// Dispatch one core call. See the module docs for the envelope.
#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_Core_nativeCall(
    mut env: JNIEnv,
    _class: JClass,
    method: JString,
    args: JString,
) -> jstring {
    let method: String = match env.get_string(&method) {
        Ok(s) => s.into(),
        Err(_) => return reply(&env, Err("method name was not readable".into())),
    };
    let args: String = match env.get_string(&args) {
        Ok(s) => s.into(),
        Err(_) => return reply(&env, Err("arguments were not readable".into())),
    };

    let result = std::panic::catch_unwind(|| dispatch(&method, &args)).unwrap_or_else(|_| {
        // Reaching here means a bug in this crate, not bad input. Say so
        // plainly rather than dressing it up as a user-facing failure.
        Err(format!("the native core panicked handling \"{method}\""))
    });

    reply(&env, result)
}

fn reply(env: &JNIEnv, result: Result<Value, String>) -> jstring {
    let body = match result {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    match env.new_string(body.to_string()) {
        Ok(s) => s.into_raw(),
        // Only if the VM is already out of memory. Null lets Kotlin surface
        // it rather than us panicking into an abort.
        Err(_) => std::ptr::null_mut(),
    }
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

        other => Err(format!("the native core has no method \"{other}\"")),
    }
}

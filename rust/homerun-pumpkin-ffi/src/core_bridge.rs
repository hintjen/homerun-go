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

use homerun_core::{console, jar, link, state, wireproxy};

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
        args.get(name)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };

    match method {
        // --- jars ---------------------------------------------------------
        "jar.resolveVersion" => jar::resolve_version(field("manifest")?, optional_text("version").as_deref())
            .map(Value::from)
            .map_err(|e| e.to_string()),

        "jar.metadataUrl" => jar::version_metadata_url(field("manifest")?, &text("version")?)
            .map(Value::from)
            .map_err(|e| e.to_string()),

        "jar.vanilla" => jar::vanilla(field("metadata")?, &text("version")?)
            .and_then(|a| serde_json::to_value(a).map_err(|e| homerun_core::Error::Malformed(e.to_string())))
            .map_err(|e| e.to_string()),

        "jar.paper" => {
            let required_java = args
                .get("requiredJava")
                .and_then(|v| v.as_u64())
                .unwrap_or(21) as u16;
            jar::paper(field("builds")?, &text("version")?, required_java)
                .and_then(|a| serde_json::to_value(a).map_err(|e| homerun_core::Error::Malformed(e.to_string())))
                .map_err(|e| e.to_string())
        }

        "jar.parseLoader" => jar::Loader::parse(optional_text("type").as_deref())
            .map(|l| Value::from(l.as_str()))
            .map_err(|e| e.to_string()),

        "jar.checkJava" => {
            let artifact: jar::Artifact = serde_json::from_value(field("artifact")?.clone())
                .map_err(|e| format!("bad artifact: {e}"))?;
            let bundled = args.get("bundledJava").and_then(|v| v.as_u64()).map(|v| v as u16);
            jar::check_java(&artifact, bundled)
                .map(|_| Value::Bool(true))
                .map_err(|e| e.to_string())
        }

        "jar.satisfies" => {
            let on_disk: jar::OnDisk = serde_json::from_value(field("onDisk")?.clone())
                .map_err(|e| format!("bad on-disk record: {e}"))?;
            let artifact: jar::Artifact = serde_json::from_value(field("artifact")?.clone())
                .map_err(|e| format!("bad artifact: {e}"))?;
            Ok(Value::Bool(on_disk.satisfies(&artifact)))
        }

        "jar.couldSatisfy" => {
            let on_disk: jar::OnDisk = serde_json::from_value(field("onDisk")?.clone())
                .map_err(|e| format!("bad on-disk record: {e}"))?;
            let loader = jar::Loader::parse(optional_text("loader").as_deref())
                .map_err(|e| e.to_string())?;
            Ok(Value::Bool(
                on_disk.could_satisfy(optional_text("version").as_deref(), loader),
            ))
        }

        // --- the tunnel ---------------------------------------------------
        "wireproxy.render" => {
            let link: wireproxy::Link = serde_json::from_value(field("link")?.clone())
                .map_err(|e| format!("bad link: {e}"))?;
            let port = |name: &str, fallback: u16| {
                args.get(name).and_then(|v| v.as_u64()).unwrap_or(fallback as u64) as u16
            };
            let exposure = match args.get("exposure").and_then(|v| v.as_str()).unwrap_or("java") {
                "java" => wireproxy::Exposure::Java { port: port("port", 25565) },
                "bedrock" => wireproxy::Exposure::Bedrock { port: port("port", 19132) },
                "crossplay" => wireproxy::Exposure::Crossplay {
                    java_port: port("port", 25565),
                    geyser_port: port("geyserPort", 19132),
                },
                other => return Err(format!("unknown exposure \"{other}\"")),
            };
            let config = wireproxy::Config {
                link,
                exposure,
                voice_chat_port: args
                    .get("voiceChatPort")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u16),
            };
            Ok(Value::from(config.render()))
        }

        "link.fromServerBody" => Ok(match link::from_server_body(field("body")?) {
            Some(polled) => serde_json::to_value(polled).map_err(|e| e.to_string())?,
            None => Value::Null,
        }),

        "link.isUsable" => {
            let polled: link::PolledLink = serde_json::from_value(field("polled")?.clone())
                .map_err(|e| format!("bad polled link: {e}"))?;
            let before: Option<wireproxy::Link> = match args.get("before") {
                Some(v) if !v.is_null() => {
                    Some(serde_json::from_value(v.clone()).map_err(|e| format!("bad prior link: {e}"))?)
                }
                _ => None,
            };
            Ok(Value::Bool(link::is_usable(&polled, before.as_ref())))
        }

        // --- lifecycle ----------------------------------------------------
        "state.exit" => {
            let intentional = args.get("intentional").and_then(|v| v.as_bool()).unwrap_or(false);
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

        // --- console ------------------------------------------------------
        "console.classify" => {
            let line = text("line")?;
            Ok(json!({
                "ready": console::is_ready(&line),
                "joined": console::joined(&line),
                "left": console::left(&line),
            }))
        }

        other => Err(format!("the native core has no method \"{other}\"")),
    }
}

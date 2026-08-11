//! JNI surface for the Android host.
//!
//! iOS links the C ABI in `lib.rs` directly. Android cannot: the JVM resolves
//! `external fun` by mangled symbol name (`Java_<package>_<Class>_<method>`),
//! so plain `homerun_server_start` is invisible to it. This module is that
//! adapter and nothing more.
//!
//! It deliberately calls the **same C functions** rather than reaching into
//! `server::host()` itself. Those functions own the `catch_unwind` that keeps
//! a panic from crossing the boundary — and a panic crossing a JNI boundary
//! aborts the whole VM, which on a phone is the whole app. Re-implementing
//! them here would mean re-implementing that guarantee.
//!
//! Every method returns the JSON string the C layer produced. The Rust
//! allocation is released here, so Kotlin has nothing to free.

use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong, jstring};
use jni::JNIEnv;

use std::ffi::{c_char, CStr, CString};

/// Take ownership of a string from the C layer, then release the original.
///
/// A null return means the C layer failed to allocate. Answering with a JSON
/// error rather than an empty string keeps the Kotlin side on one parse path.
unsafe fn take_json(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return r#"{"ok":false,"error":"native allocation failed"}"#.to_string();
    }
    let owned = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    crate::homerun_free_string(ptr);
    owned
}

fn to_jstring(env: &JNIEnv, value: String) -> jstring {
    match env.new_string(value) {
        Ok(s) => s.into_raw(),
        // Only happens if the VM is already out of memory; returning null lets
        // Kotlin surface it rather than us panicking into an abort.
        Err(_) => std::ptr::null_mut(),
    }
}

fn from_jstring(env: &mut JNIEnv, value: &JString) -> Option<CString> {
    let text: String = env.get_string(value).ok()?.into();
    CString::new(text).ok()
}

/// Route Rust's `log` output to logcat. Idempotent; safe to call repeatedly.
#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeInitLogging(
    _env: JNIEnv,
    _class: JClass,
) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("HomerunNative"),
    );
}

#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeAbiVersion(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    crate::FFI_ABI_VERSION as jint
}

/// **Blocks until the server stops.** Kotlin must call this on a dedicated
/// thread with at least a 16 MB stack — see `NativeServer.startBlocking`.
#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    server_id: JString,
    data_dir: JString,
    port: jint,
    invocation: JString,
) -> jstring {
    let (Some(id), Some(dir)) = (
        from_jstring(&mut env, &server_id),
        from_jstring(&mut env, &data_dir),
    ) else {
        return to_jstring(
            &env,
            r#"{"ok":false,"error":"server_id and data_dir must be valid strings"}"#.to_string(),
        );
    };

    // The C surface takes one JSON request so a launch can carry both the
    // player's settings and what to run. Android's Kotlin signature stays
    // scalar and the request is assembled here.
    //
    // `invocation` is what makes this a *child process* rather than the
    // linked engine: the argv and environment the host composed, which is the
    // one part of running a JVM that is genuinely Android's. Absent — the
    // Pumpkin backend — runs the linked engine, exactly as before.
    let invocation = from_jstring(&mut env, &invocation)
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw.to_string_lossy()).ok());

    let Ok(request) = CString::new(
        serde_json::json!({
            "serverId": id.to_string_lossy(),
            "dataDir": dir.to_string_lossy(),
            "port": port as u16,
            "invocation": invocation,
        })
        .to_string(),
    ) else {
        return to_jstring(
            &env,
            r#"{"ok":false,"error":"server_id and data_dir must not contain NUL"}"#.to_string(),
        );
    };

    let json = unsafe { take_json(crate::homerun_server_start(request.as_ptr())) };
    to_jstring(&env, json)
}

/// What this run has cost. See `homerun_server_metrics`.
#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeMetrics(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let json = unsafe { take_json(crate::homerun_server_metrics()) };
    to_jstring(&env, json)
}

#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeStop(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let json = unsafe { take_json(crate::homerun_server_stop()) };
    to_jstring(&env, json)
}

#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeState(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let json = unsafe { take_json(crate::homerun_server_state()) };
    to_jstring(&env, json)
}

#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeStats(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let json = unsafe { take_json(crate::homerun_server_stats()) };
    to_jstring(&env, json)
}

#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativePlayers(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let json = unsafe { take_json(crate::homerun_server_players()) };
    to_jstring(&env, json)
}

#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeLogsSince(
    env: JNIEnv,
    _class: JClass,
    cursor: jlong,
) -> jstring {
    // Cursors are monotonic and never negative; a negative value can only come
    // from a Kotlin bug, and clamping beats reading a wild offset.
    let cursor = cursor.max(0) as u64;
    let json = unsafe { take_json(crate::homerun_server_logs_since(cursor)) };
    to_jstring(&env, json)
}

#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_NativeServer_nativeCommand(
    mut env: JNIEnv,
    _class: JClass,
    command: JString,
) -> jstring {
    let Some(cmd) = from_jstring(&mut env, &command) else {
        return to_jstring(
            &env,
            r#"{"ok":false,"error":"command must be a valid string"}"#.to_string(),
        );
    };
    let json = unsafe { take_json(crate::homerun_server_command(cmd.as_ptr())) };
    to_jstring(&env, json)
}

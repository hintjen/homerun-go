//! The Android host's way into `homerun-core`.
//!
//! Thin on purpose: the JVM resolves `external fun` by mangled symbol name, so
//! Kotlin needs an adapter, but the adapter's whole job is moving two strings
//! across the boundary. Everything that decides anything lives in
//! [`crate::core_dispatch`], which iOS reaches through the C ABI instead.
//!
//! # Panics must not cross this boundary
//!
//! A panic unwinding through JNI aborts the VM, which on a phone is the whole
//! app. `core_dispatch::call` already contains its own `catch_unwind`, so
//! nothing here can unwind; this module only has to avoid introducing a panic
//! of its own, which is why every step below matches on its error.

use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;

use crate::host_dispatch;

/// Dispatch one core call. See [`crate::core_dispatch`] for the envelope, and
/// [`crate::host_dispatch`] for the few calls answered before it.
#[no_mangle]
pub extern "system" fn Java_app_gethomerun_mobile_Core_nativeCall(
    mut env: JNIEnv,
    _class: JClass,
    method: JString,
    args: JString,
) -> jstring {
    let method: String = match env.get_string(&method) {
        Ok(s) => s.into(),
        Err(_) => return failure(&env, "method name was not readable"),
    };
    let args: String = match env.get_string(&args) {
        Ok(s) => s.into(),
        Err(_) => return failure(&env, "arguments were not readable"),
    };

    match env.new_string(host_dispatch::call(&method, &args)) {
        Ok(s) => s.into_raw(),
        // Only if the VM is already out of memory. Null lets Kotlin surface it
        // rather than us panicking into an abort.
        Err(_) => std::ptr::null_mut(),
    }
}

/// An envelope for the two failures that happen before dispatch is reachable.
fn failure(env: &JNIEnv, why: &str) -> jstring {
    let body = serde_json::json!({ "ok": false, "error": why }).to_string();
    match env.new_string(body) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

//! Starts a JVM that was downloaded at runtime.
//!
//! # Why this exists
//!
//! Android since API 29 refuses to `execve()` anything in the app's writable
//! storage, so a downloaded `java` binary can never be run. It does still
//! allow `dlopen()` of an ordinary position-independent shared library from
//! there — which is how every Android Java runtime works, PojavLauncher
//! included.
//!
//! This program is the smallest thing that bridges those two facts. It ships
//! inside the APK as `libjavabin.so` (the packager only keeps `lib*.so`
//! entries), so it lives in `nativeLibraryDir` and may legally be exec'd. Once
//! running, it `dlopen`s the *downloaded* `libjvm.so` and starts the VM
//! in-process.
//!
//! The result is a real child process per server, which is what keeps the
//! Android host on the same contract as the desktop supervisor: stdout is the
//! console, stdin takes `stop`, and a JVM that dies takes nothing else with
//! it.
//!
//! # Usage
//!
//! ```text
//! libjavabin.so <libjvm.so> <main-class> [jvm-option ...] -- [program arg ...]
//! ```
//!
//! `main-class` is resolved by the caller (Kotlin reads it out of the jar's
//! manifest), so this stays free of zip parsing.

use std::ffi::{c_void, CString};
use std::process::ExitCode;

use jni::sys::{JavaVM as RawJavaVM, JavaVMInitArgs, JavaVMOption, JNI_OK, JNI_VERSION_1_8};
use jni::JNIEnv;

type CreateJavaVm = unsafe extern "system" fn(
    *mut *mut RawJavaVM,
    *mut *mut c_void,
    *mut c_void,
) -> jni::sys::jint;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 3 {
        eprintln!(
            "usage: {} <libjvm.so> <main-class> [jvm-option ...] -- [program arg ...]",
            argv.first().map(String::as_str).unwrap_or("launcher")
        );
        return ExitCode::from(2);
    }

    let libjvm = argv[1].clone();
    let main_class = argv[2].replace('.', "/");

    let split = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let jvm_options: Vec<String> = argv[3..split].to_vec();
    let program_args: Vec<String> = argv.get(split + 1..).unwrap_or(&[]).to_vec();

    // Announced before the VM exists, so the host has it from the first line
    // of the console.
    //
    // The host samples `/proc/<pid>` to graph what a server is costing, and it
    // cannot learn this pid any other way: `java.lang.Process.pid()` does not
    // resolve against the Android SDK, and the private field behind it is on
    // the non-SDK interface list. We own this process, so it says who it is —
    // which is simpler than either, and cannot be taken away by a platform
    // release.
    //
    // On stdout rather than in a pid file: stdout is already the console the
    // host reads, so this needs no path, no cleanup, and has no way to go
    // stale. `[launcher]` marks it as ours, like the error path below.
    println!("[launcher] pid={}", std::process::id());

    match run(&libjvm, &main_class, &jvm_options, &program_args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // stderr is merged into the server console, so this reaches the
            // user rather than disappearing into logcat.
            eprintln!("[launcher] {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    libjvm: &str,
    main_class: &str,
    jvm_options: &[String],
    program_args: &[String],
) -> Result<(), String> {
    // Kept alive for the whole process: unloading the JVM's library while the
    // VM is running would be fatal.
    let library = unsafe { libloading::Library::new(libjvm) }
        .map_err(|err| format!("could not load {libjvm}: {err}"))?;

    let create: libloading::Symbol<CreateJavaVm> = unsafe { library.get(b"JNI_CreateJavaVM\0") }
        .map_err(|err| format!("{libjvm} has no JNI_CreateJavaVM: {err}"))?;

    // The option strings must outlive the call, so they are held here rather
    // than built inline where they would be dropped mid-flight.
    let owned: Vec<CString> = jvm_options
        .iter()
        .map(|opt| CString::new(opt.as_str()).map_err(|_| format!("bad JVM option: {opt}")))
        .collect::<Result<_, _>>()?;

    let mut options: Vec<JavaVMOption> = owned
        .iter()
        .map(|opt| JavaVMOption {
            optionString: opt.as_ptr() as *mut _,
            extraInfo: std::ptr::null_mut(),
        })
        .collect();

    let mut args = JavaVMInitArgs {
        version: JNI_VERSION_1_8,
        nOptions: options.len() as i32,
        options: options.as_mut_ptr(),
        ignoreUnrecognized: 0,
    };

    let mut raw_vm: *mut RawJavaVM = std::ptr::null_mut();
    let mut raw_env: *mut c_void = std::ptr::null_mut();

    let status = unsafe {
        create(
            &mut raw_vm,
            &mut raw_env,
            &mut args as *mut JavaVMInitArgs as *mut c_void,
        )
    };
    if status != JNI_OK {
        return Err(format!("JNI_CreateJavaVM failed with status {status}"));
    }

    let vm = unsafe { jni::JavaVM::from_raw(raw_vm) }
        .map_err(|err| format!("could not adopt the created VM: {err}"))?;
    let mut env: JNIEnv = unsafe { JNIEnv::from_raw(raw_env as *mut jni::sys::JNIEnv) }
        .map_err(|err| format!("could not adopt the JNI environment: {err}"))?;

    let result = invoke_main(&mut env, main_class, program_args);

    // Blocks until every non-daemon thread has finished — which for a
    // Minecraft server is the server itself. `main` returns almost
    // immediately after handing off to the server thread, so without this the
    // process would exit while the world was still running.
    unsafe {
        // Detach before destroying: DestroyJavaVM waits for non-daemon
        // threads, and this one still counts as attached until told otherwise.
        let _ = vm.detach_current_thread();
        if let Some(destroy) = (**raw_vm).DestroyJavaVM {
            destroy(raw_vm);
        }
    }

    result
}

fn invoke_main(env: &mut JNIEnv, main_class: &str, program_args: &[String]) -> Result<(), String> {
    let class = env.find_class(main_class).map_err(|err| {
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        format!("could not find {main_class}: {err}")
    })?;

    let string_class = env
        .find_class("java/lang/String")
        .map_err(|err| format!("could not find java/lang/String: {err}"))?;
    let empty = env
        .new_string("")
        .map_err(|err| format!("could not allocate a string: {err}"))?;

    let args_array = env
        .new_object_array(program_args.len() as i32, &string_class, &empty)
        .map_err(|err| format!("could not allocate the argument array: {err}"))?;

    for (index, arg) in program_args.iter().enumerate() {
        let value = env
            .new_string(arg)
            .map_err(|err| format!("could not allocate an argument: {err}"))?;
        env.set_object_array_element(&args_array, index as i32, value)
            .map_err(|err| format!("could not set an argument: {err}"))?;
    }

    let called = env.call_static_method(
        class,
        "main",
        "([Ljava/lang/String;)V",
        &[jni::objects::JValue::Object(&args_array)],
    );

    // Print the Java stack trace before clearing. Without this every failure
    // reads "Java exception was thrown", which says nothing at all — and this
    // is the one place a server's startup failure surfaces.
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
        return Err(format!("{main_class}.main threw — stack trace above"));
    }

    called.map_err(|err| format!("could not invoke {main_class}.main: {err}"))?;
    Ok(())
}

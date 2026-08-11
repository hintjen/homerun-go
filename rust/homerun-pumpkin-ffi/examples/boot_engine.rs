//! Boots the real engine through the C surface, exactly as a host would.
//!
//! The unit tests all run against `StubEngine`, so nothing in the suite proves
//! that Pumpkin itself starts, reports itself running, and shuts down when
//! asked. This does, host-native, without a device or a simulator:
//!
//! ```bash
//! cargo run --example boot_engine --features pumpkin-engine
//! ```
//!
//! It mirrors the host's own sequence — a dedicated thread with a 16 MB stack
//! for the blocking start call, poll the state, drain the console by cursor,
//! then stop — so a failure here is a real failure on device too.

// dup(2)/write(2) below are POSIX; this example is a developer tool run on
// the machines that build for a device, so gate it rather than port it.
#![cfg(unix)]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use homerun_pumpkin_ffi::{
    homerun_free_string, homerun_server_logs_since, homerun_server_start, homerun_server_state,
    homerun_server_stop,
};

unsafe extern "C" {
    fn dup(fd: i32) -> i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

/// The terminal's real stdout, saved before the engine takes fd 1.
static TERMINAL: OnceLock<i32> = OnceLock::new();

/// Print to the actual terminal.
///
/// Starting the server replaces fds 1 and 2 with a pipe so the console can be
/// captured, which means an ordinary `println!` from this point on would be
/// swallowed. Worse, printing *drained console lines* would feed them back
/// into the buffer they came from and loop forever. So progress goes to a
/// duplicate of the original stdout, taken before any of that happens.
fn say(message: &str) {
    let fd = *TERMINAL.get().unwrap_or(&1);
    let line = format!("{message}\n");
    unsafe { write(fd, line.as_ptr(), line.len()) };
}

/// Take ownership of an FFI string, exactly as the hosts must.
fn take(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { homerun_free_string(ptr) };
    text
}

fn state() -> String {
    take(homerun_server_state())
}

fn drain(cursor: &mut u64) {
    let raw = take(homerun_server_logs_since(*cursor));
    if std::env::var("BOOT_ENGINE_RAW").is_ok() {
        say(&format!("  raw: {}", &raw[..raw.len().min(300)]));
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    for line in value["lines"].as_array().into_iter().flatten() {
        say(&format!("  | {}", line.as_str().unwrap_or_default()));
    }
    if let Some(next) = value["cursor"].as_u64() {
        *cursor = next;
    }
}

fn main() {
    let dir = std::env::temp_dir().join("homerun-boot-engine");
    std::fs::create_dir_all(&dir).expect("create data dir");
    TERMINAL.set(unsafe { dup(1) }).ok();
    say(&format!("data dir: {}", dir.display()));

    // The same request shape the hosts send, with settings — so this example
    // exercises the mapping onto Pumpkin's config, which is the part a unit
    // test can check but not observe on a running server.
    let request = CString::new(
        serde_json::json!({
            "serverId": "boot-test",
            "dataDir": dir.to_string_lossy(),
            "port": 25565,
            "settings": {
                "gameType": "java",
                "env": {
                    "MOTD": "boot_engine",
                    "MAX_PLAYERS": "7",
                    "ONLINE_MODE": "false",
                    "GAMEMODE": "creative",
                },
                "resolved": [],
            },
        })
        .to_string(),
    )
    .unwrap();

    // The same 16 MB stack the iOS host uses. The default overflows inside the
    // engine and dies with no panic report.
    let runner = std::thread::Builder::new()
        .name("homerun-server".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || take(unsafe { homerun_server_start(request.as_ptr()) }))
        .expect("spawn server thread");

    let mut cursor = 0u64;
    let started = Instant::now();
    let mut running = false;

    // Generous, because first boot generates a world — but bounded, because
    // this is a test and a hang should fail rather than sit there.
    while started.elapsed() < Duration::from_secs(180) {
        drain(&mut cursor);
        if state().contains("running") {
            running = true;
            break;
        }
        if runner.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    if !running {
        drain(&mut cursor);
        say(&format!(
            "\nFAILED: never reached running. state={}",
            state()
        ));
        say(&format!(
            "start returned: {}",
            runner.join().unwrap_or_default()
        ));
        std::process::exit(1);
    }

    say(&format!("\nRUNNING after {:?}\n", started.elapsed()));
    std::thread::sleep(Duration::from_secs(3));
    drain(&mut cursor);

    say("\nstopping…");
    say(&format!("stop: {}", take(homerun_server_stop())));

    let outcome = runner.join().unwrap_or_default();
    drain(&mut cursor);

    say(&format!("\nstart returned: {outcome}"));
    say(&format!("final state: {}", state()));

    if outcome.contains("\"ok\":true") {
        say("\nOK: booted and shut down cleanly.");
    } else {
        say("\nFAILED: unclean shutdown.");
        std::process::exit(1);
    }
}

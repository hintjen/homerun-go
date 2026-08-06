# `homerun-pumpkin-ffi` — the C surface

The Rust library both mobile hosts link. It owns the server's lifecycle,
console buffer, and crash reporting, and exposes them over a C ABI so Swift
and Kotlin talk to one implementation instead of two.

Source: `rust/homerun-pumpkin-ffi/`.

## Why this crate exists at all

The Pumpkin server was written to be a process. Two of its habits are fatal
inside an app:

- it calls `process::exit(1)` when it cannot bind its port, which on a phone
  terminates the entire app with no report;
- it writes to stdout, which neither platform surfaces, so the console — a
  headline feature — would be empty.

The prototype fixed both by patching Pumpkin. That works but makes the fork
expensive to rebase. Here the fixes live in **our** crate, so the fork can
shrink to library-mode patches only and eventually disappear upstream.

## Layout

| Module | Responsibility |
|---|---|
| `lib.rs` | The `extern "C"` surface. Marshalling, panic containment. |
| `server.rs` | Composes everything into the one server this device hosts. Owns the global. |
| `engine.rs` | The `Engine` trait — the only part that needs Pumpkin. |
| `state.rs` | The state machine and its legal transitions. |
| `log_buffer.rs` | Bounded console buffer with monotonic cursors. |
| `preflight.rs` | Port availability, checked before the engine can exit the process. |
| `crash.rs` | Panic hook, crash reports, last-panic capture. |

Everything except `Engine::run` is platform-independent and unit-tested on
any machine — 36 tests, no device and no Pumpkin required.

## Calling convention

Every function returns a heap-allocated JSON C string.

**The caller must free it with `homerun_free_string`.** Leaking these leaks
the server's entire console over a long session.

```c
uint32_t homerun_abi_version(void);
void     homerun_free_string(char *ptr);

char *homerun_server_start(const char *server_id, const char *data_dir, uint16_t port);
char *homerun_server_stop(void);
char *homerun_server_state(void);
char *homerun_server_stats(void);
char *homerun_server_players(void);
char *homerun_server_logs_since(uint64_t cursor);
char *homerun_server_command(const char *command);
```

Fallible calls answer `{"ok":true,…}` or `{"ok":false,"error":"…"}`.

**Error strings are shown to players.** They are written for players, and a
test asserts they contain no `errno`, `unwrap`, `panicked at`, `Mutex`, or
`null pointer`. Keep it that way.

Check `homerun_abi_version()` at startup; it is bumped whenever the surface
changes shape.

### Responses

`homerun_server_state` → `{"state":"stopped|starting|running|stopping|crashed"}`

`homerun_server_stats` → `{"running":bool,"state":str,"serverId":str?,"startedAtMs":n?,"port":n?}`

`homerun_server_players` → `{"players":[{"name":str,"uuid":str?}],"max":n?}`, or
`null` when not running. Do not render a roster for a server nobody can join.

`homerun_server_logs_since` → `{"lines":[str],"cursor":n,"dropped":bool}`

## Host integration rules

These are not style preferences. Each one corresponds to a failure that is
miserable to diagnose after the fact.

**Call `homerun_server_start` on a dedicated thread with ≥16 MB of stack.**
The default 512 KB overflows inside the engine and kills the process with no
panic report. It blocks for the server's entire lifetime.

```swift
let thread = Thread { /* homerun_server_start(...) */ }
thread.stackSize = 16 * 1024 * 1024
thread.start()
```

**Never set a timeout on start.** Starting a server legitimately takes
minutes on a phone — world generation, mod downloads. The bridge has no
blanket call timeout for the same reason (PROTOCOL.md §5).

**Poll logs with the returned cursor, not a stored count.** Cursors are
monotonic sequence numbers and never reused. `dropped: true` means the
buffer discarded lines you had not read — surface a "…output skipped…"
marker rather than pretending the gap did not happen.

Cursors do **not** survive a restart. The buffer clears between runs but
sequences keep climbing, so a cursor held across a restart reports
`dropped` instead of silently replaying the new run as if it continued the
old one.

**Free every returned string**, including from calls that failed.

## The console buffer

Bounded at 2000 lines (a few hundred KB). A backgrounded phone may not poll
for minutes while a busy server emits thousands of lines, so the buffer
evicts the oldest rather than growing without limit — and reports the gap
instead of hiding it.

## Crash handling

`crash.rs` installs a panic hook that writes `crash-reports/panic-<ts>.txt`
into the server's data directory, with a backtrace, and keeps the message so
a failed start can explain itself.

Two things worth knowing:

- **Every `extern "C"` function wraps its body in `catch_unwind`.** A panic
  crossing the FFI boundary is undefined behaviour, not a crash you can
  debug.
- **The last-panic slot is cleared at the start of every run.** Without
  that, a panic from anywhere earlier in the process leaks into the *next*
  crash's message and blames the wrong thing. That was a real bug, caught by
  a test, and `a_crash_is_not_blamed_on_an_older_unrelated_panic` pins it.

## One server at a time

Enforced in `state.rs` and again in the hosts. The engine keeps global state
and distinguishes worlds by process working directory, so a second concurrent
server is not a feature that was skipped — it is not expressible without
restructuring upstream.

Starting a second server returns a player-facing message rather than an
error code:

> Another server is already running. Stop it first — this device can host one at a time.

This matches the desktop app, where users create many servers and run one.

## Wiring Pumpkin in

`Engine` is the seam. Today `StubEngine` stands in — it reports startup,
honours stop requests, and can be told to fail, which is how the failure
paths are tested without a real server.

To go live:

1. Pin the fork in `Cargo.toml` (currently commented out, deliberately: the
   crate compiles and its safety tests run without the engine, so the FFI
   surface can be reviewed first).
2. Implement `Engine` for Pumpkin — `run` blocks until shutdown, emits
   console lines through `on_line`, and **returns** rather than exiting.
3. Swap the default in `server::host()`.

Nothing else should need to change. If it does, the seam is in the wrong
place.

## Building

```bash
cargo test                     # host-native; no device, no Pumpkin
cargo clippy --all-targets
cargo build --release

# mobile targets
cargo build --release --target aarch64-apple-ios          # iOS device
cargo build --release --target aarch64-apple-ios-sim      # iOS simulator
cargo ndk -t arm64-v8a build --release                    # Android
```

`crate-type` is `["staticlib", "cdylib", "rlib"]` — iOS links the `.a`,
Android loads the `.so`, and `rlib` keeps the test build fast.

Release uses `opt-level = "z"` + LTO + strip: a debug build of the server is
around 1.8 GB, which is impractical to ship or even copy to a device.

## Status

Implemented and tested: state machine, console buffer, pre-flight, crash
capture, the whole C surface, one-server enforcement.

Not implemented: the Pumpkin engine itself, and stdout/stderr redirection
(`pipe`/`dup2` — needed once a real engine writes to fd 1 rather than
calling `on_line`).

Never run on a device. Everything here is verified on a desktop host.

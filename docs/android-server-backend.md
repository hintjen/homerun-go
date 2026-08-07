# The Android server backend

How `native-server-*` becomes a real server running inside the app.

Source: `rust/homerun-pumpkin-ffi/src/jni_bridge.rs`,
`android/.../NativeServer.kt`, `android/.../PumpkinBackend.kt`.

## Overview

The server runs **in this process**, not as a child. There is no pid, no
stdio pipe, and no separate address space to measure — the engine is a Rust
library linked into the app.

Three layers, each with one job:

| Layer | Job |
|---|---|
| `jni_bridge.rs` | Makes the C ABI callable from the JVM. Nothing else. |
| `NativeServer.kt` | The raw JNI surface plus the thread the engine needs. |
| `PumpkinBackend.kt` | Lifecycle, polling, and the `ServerBackend` contract. |

## Why a JNI layer exists — `jni_bridge.rs`

iOS links the C symbols directly. The JVM cannot: it resolves `external fun`
by mangled name (`Java_<package>_<Class>_<method>`), so `homerun_server_start`
is invisible to it. This module is that adapter and deliberately nothing more.

It calls the **same C functions** rather than reaching into `server::host()`.
Those functions own the `catch_unwind` that stops a panic crossing the
boundary, and **a panic crossing a JNI boundary aborts the VM** — on a phone,
the whole app. Re-implementing the calls here would mean re-implementing that
guarantee, which is exactly the kind of duplication that rots.

The module is `#[cfg(target_os = "android")]`, so iOS builds never see it.

Every method returns the JSON string the C layer produced and releases the
Rust allocation before returning, so Kotlin has nothing to free.

## The 16 MB stack — `NativeServer.startBlocking`

`nativeStart` blocks for the server's entire lifetime **and** needs a thread
with at least a 16 MB stack. A default thread overflows inside the engine and
takes the process down with no panic report and nothing useful in logcat —
the single worst failure mode in this stack, because it looks like a random
crash.

`startBlocking` therefore constructs the thread explicitly:

```kotlin
Thread(null, { … }, "homerun-engine", 16L * 1024 * 1024)
```

Never call `nativeStart` from a coroutine dispatcher. Dispatchers own their
threads and you do not control the stack size.

The ABI version is checked once at load. A mismatch logs loudly and leaves
`NativeServer.available` false, so the app runs without a server backend
rather than crashing on the first call.

## Lifecycle and polling — `PumpkinBackend`

**Start returns when the server is up, not when the call returns.** Since
`nativeStart` only returns at shutdown, `start()` launches the engine thread
and then polls `nativeState()` until it reports *running*. The bridge has no
call timeout precisely so this can take as long as it takes; the 120 s cap
here exists only so a wedged engine reports instead of hanging forever.

**Nothing calls us when a console line arrives.** The engine writes into a
bounded ring buffer with a monotonic cursor, so a poller drains it every
second and re-emits each line as a `native-server-log` event. When the buffer
has overflowed, the drain reports the gap rather than letting the console look
like it merely skipped ahead.

The same tick samples memory and player count into a bounded history for
`native-server-get-perf-history`, matching the desktop sampler's window.

**One server at a time.** Enforced in the crate and again here. The engine
distinguishes worlds by process CWD, so a second concurrent run would quietly
share the first one's world. Attempting it raises `AnotherServerRunning`,
whose message is written for players because they are the ones who see it.

## What the metrics actually measure

Be careful reading these — the in-process design makes some of them
approximations, and the UI presents them as facts.

| Channel | Source | Caveat |
|---|---|---|
| `native-server-get-mem-usage` | `Debug.getNativeHeapAllocatedSize()` | Process-wide native heap, not the server alone. The engine is Rust, so JVM heap would be the wrong number entirely. |
| `native-server-get-cpu-usage` | not reported | Returns null. Per-process CPU needs sampling `/proc/self/stat` over an interval; a wrong number becomes a wrong graph, and null renders as "unavailable", which is true. |
| `native-server-get-uptime` | engine `startedAtMs` | Real. |
| `native-server-get-ops` | empty | The engine does not expose an op list yet. |

## Storage

Worlds live under `filesDir/servers/<serverId>` — app-private and preserved
across updates. **Not `cacheDir`**: the system may delete that under storage
pressure, and it would take the player's world with it.

## Current engine

`StubEngine`, because the Pumpkin fork is not pinned yet. It is not a no-op:
it reports startup, emits console lines, honours stop requests, and can be
told to fail, so every path above is exercised for real. Swapping in Pumpkin
changes `engine.rs` and nothing in this document.

Verified on the emulator end to end: start → `running` + port + console
lines → metrics and perf samples → stop → `stopped`, with the active-id list
correct on both sides of it.

## Triage

**`UnsatisfiedLinkError` on launch.** The `.so` is missing for this ABI. Run
`npm run rust:android` (arm64) or `npm run rust:android-x86_64` (emulator).

**The app dies the moment a server starts, with no stack.** The engine thread
did not get its 16 MB stack. Check nothing bypasses `startBlocking`.

**`native-server-start` resolves but nothing happens.** The engine thread
started and exited immediately; look for its exit JSON in logcat under
`HomerunBackend`.

**Console is empty but the server is running.** The log pump is not running,
or `onLog` was never wired. Both are set up in `BridgeRouter`'s `backend`
initialiser.

**Server appears to start twice.** Two `native-server-start` calls raced.
The second gets `alreadyRunning: true` rather than an error, which is what
the desktop contract expects.

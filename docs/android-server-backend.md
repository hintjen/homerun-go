# The Android server backend

How `native-server-*` becomes a real server running inside the app.

Source: `android/.../ServerHost.kt`, `JavaServerBackend.kt`, `JavaRuntime.kt`,
`PumpkinBackend.kt`, `NativeServer.kt`, and
`rust/homerun-pumpkin-ffi/src/jni_bridge.rs`.

## Overview

**Two backends, and Android prefers the JVM.** Running the real server jar is
the differentiated Android product — actual mods, plugins and parity with
desktop. Pumpkin exists as the fallback for builds that ship no JRE, and as
the only option on iOS, which cannot spawn a process at all.

`ServerHost` picks at startup and logs which it chose. It also owns the
backend **process-wide** rather than per-activity: a WebView can be destroyed
and rebuilt while a server keeps running, so a backend hanging off
`lifecycleScope` would have its log pump cancelled and its engine orphaned.

| Layer | Job |
|---|---|
| `ServerHost.kt` | Chooses the backend, owns it for the process, fans out its callbacks. |
| `JavaServerBackend.kt` | The real server jar as a child process. **Preferred.** |
| `JavaRuntime.kt` | Finds and unpacks the bundled JVM. |
| `jni_bridge.rs` | Makes the Rust C ABI callable from the JVM. Nothing else. |
| `NativeServer.kt` | The raw JNI surface plus the thread the engine needs. |
| `PumpkinBackend.kt` | The Rust engine, in-process. Fallback. |

## The JVM backend — `JavaServerBackend`

The desktop supervisor (`src/electron/supervisor.js` in the `homerun` repo) is
the spec, and the contract is deliberately identical so a world behaves the
same on a phone as on a PC:

```
java -Xmx<N>M -Xms<N>M -jar <server.jar> nogui     # cwd = the server directory
```

**Stopping is `stop` on stdin, never a signal.** Killing the JVM risks the
world it was mid-save on. This waits 30 s, then terminates, then forcibly —
the same escalation the desktop uses, for the same reason.

**Player tracking reads the console, not RCON.** Vanilla prints join and leave
lines; parsing them costs nothing and avoids a port, a password and a second
protocol to keep alive. The regexes are vanilla's wording, so the roster is
best-effort and never blocks anything. RCON becomes worth adding when
moderation (kick/ban/op) lands.

**Heap is capped at a third of device RAM.** Android kills *the whole app*
under memory pressure, not just the server, so an over-generous heap does not
lose you a server — it loses you the app that was hosting it.

### Getting a JVM onto the device

The desktop downloads Azul Zulu JREs on demand. Android can do the same, with
one twist that decides the whole design.

[Android 10 (API 29)](https://developer.android.com/about/versions/10/behavior-changes-10)
says:

> Untrusted apps that target Android 10 cannot invoke `execve()` directly on
> files within the app's home directory.

So a downloaded `java` binary can never be run. But the same page restricts
`dlopen()` only for libraries with text relocations — an ordinary
position-independent `.so` loads from app storage fine. That is the gap every
Android Java runtime goes through, PojavLauncher included.

Hence `rust/homerun-java-launcher`: a ~200-line program that ships **inside the
APK** as `jniLibs/<abi>/libjavabin.so`, so it lives in `nativeLibraryDir` and
may legally be exec'd. Once running it `dlopen`s the *downloaded* `libjvm.so`
and calls `JNI_CreateJavaVM`.

The result is a real child process per server, which is what keeps Android on
the same contract as the desktop supervisor: stdout is the console, stdin
takes `stop`, and a JVM that dies takes nothing else with it. Running the VM
in-process — the other option — would have meant a crash killing the app and
`System.exit` from a plugin doing the same.

```text
libjavabin.so <libjvm.so> <main-class> [jvm-option ...] -- [program arg ...]
```

The main class is resolved by the caller from the jar manifest, so the
launcher needs no zip parsing.

Two packaging rules still bind, and neither bends:

1. **The packager only ships `jniLibs` entries named `lib*.so`.** A file called
   `homerun-java-launcher` is silently dropped from the APK — the same class of
   silent omission as the `_next/` asset filter.
2. **`jniLibs.useLegacyPackaging` must stay `true`.** With it false nothing is
   extracted to `nativeLibraryDir` at all — the linker maps libraries straight
   out of the APK — and there is no real file to exec.

#### Verified on the emulator

A real Minecraft 1.20.4 server, on a downloaded Java 17 runtime, reached its
EULA gate:

```
Unpacking 1.20.4/server-1.20.4.jar (versions:1.20.4) to versions/...
Unpacking io/netty/netty-handler/4.1.97.Final/... to libraries/...
[ServerMain/INFO]: You need to agree to the EULA in order to run the server.
```

That is the whole chain: exec from `nativeLibraryDir`, `dlopen` of a
`libjvm.so` in app storage, `JNI_CreateJavaVM`, class loading from the
downloaded runtime, Mojang's bundler unpacking its libraries, and
`net.minecraft.server.Main` running far enough to write `server.properties`
and `eula.txt`. Console output arrived over `bridge/v1` throughout.

Two things only a real run found:

- **`LD_LIBRARY_PATH` must include `<javaHome>/lib`.** The runtime's own
  natives carry `DT_NEEDED` entries for each other (`libnio.so` needs
  `libnet.so`), and Android's linker will not find them otherwise. It has to
  be in the child's *environment* — the linker reads it at process start, so
  setting it afterwards is too late. Without it the VM boots and then dies the
  moment anything touches `java.nio`.
- **Stopping at the EULA reports as a crash.** `start()` waits for `Done (…)!`
  and the process exits before that, so the user is told "the server stopped
  unexpectedly while starting". Accurate but unhelpful; the EULA needs
  detecting and surfacing as its own state.

#### What to bundle vs download

Only the launcher (0.3 MB) ships in the APK. The runtime is downloaded, so
install size stays small and several Java versions can coexist — which matters,
because Minecraft's required Java version moves with the game.

#### Where runtimes come from

Two sources, and the catalog currently points at the weaker one.

**PojavLauncher multiarch builds** — what `JavaRuntime.CATALOG` uses today, and
what the emulator run above was proven against. Plain `.tar.xz`, no external
dependencies, drops straight into place. But the newest x86_64 asset is from
2021 and tops out at Java 17.

**Termux** — [`packages.termux.dev`](https://packages.termux.dev/apt/termux-main/)
publishes current OpenJDK for **both** architectures:

| Package | x86_64 | aarch64 |
|---|---|---|
| openjdk-17 | 17.0.20 | 17.0.20 |
| openjdk-21 | 21.0.12 | 21.0.12 |
| openjdk-25 | 25.0.4 | 25.0.4 |

Strictly better on version currency and maintenance, with two costs worth
knowing before switching:

- **`.deb`, not `.tar.xz`.** An `ar` archive wrapping a tar — commons-compress
  reads both, so this is plumbing, not a blocker.
- **Built for Termux's prefix.** These expect
  `/data/data/com.termux/files/usr` and declare real dependencies
  (`libandroid-shmem`, `libandroid-spawn`, `libiconv`, `libjpeg-turbo`, `zlib`,
  `littlecms`), which would have to be fetched and merged into the same tree.
  We already override `java.home` and `LD_LIBRARY_PATH`, so the prefix is
  probably survivable — but "probably" is doing real work in that sentence and
  it is unproven. **Assume nothing here until a server actually boots on one.**

## The Pumpkin backend

### Why a JNI layer exists — `jni_bridge.rs`

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

### The 16 MB stack — `NativeServer.startBlocking`

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

### Lifecycle and polling — `PumpkinBackend`

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

**`no JVM bundled` in logcat, and Java servers refuse to start.** Expected on
any build without a JRE — see the packaging section. It falls back to Pumpkin.

**`EACCES` launching the JVM.** The launcher is not in `nativeLibraryDir`, or
`useLegacyPackaging` was flipped to false so nothing was extracted there.

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

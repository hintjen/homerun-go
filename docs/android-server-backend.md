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
| `ServerJar.kt` | Resolves and downloads the server jar. |
| `HomerunApi.kt` | Reads a server's version and loader from the backend. |
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

The runtime **ships inside the app**. It is not downloaded, and that is a
policy constraint rather than a preference:
[Google Play](https://support.google.com/googleplay/android-developer/answer/16559646)
says an app "may not download executable code (such as dex, JAR, .so files)
from a source other than Google Play". The carve-out for code that runs *in* a
virtual machine does not help — `libjvm.so` **is** the virtual machine.

Server *jars* are different: they are data the JVM reads, and they are still
downloaded. [Anvil-MC](https://anvil-mc.com/), which hosts Java servers on Play
today with 100k+ downloads, draws the same line — a 154 MB arm64-only download
with the runtime inside it, fetching server software at runtime.

#### Staging it

```bash
npm run jre:android          # arm64-v8a, what ships
npm run jre:android-x86_64   # emulator
```

`scripts/stage-jre.py` pulls OpenJDK from
[Termux](https://packages.termux.dev/apt/termux-main/) — the only source
publishing current OpenJDK for Android on both architectures (17.0.20, 21.0.12,
25.0.4) — and unpacks it into `assets/jre/`. Pinned by exact version, so an
upstream bump cannot silently change what ships. It needs only the Python
standard library: `ar`, `xz` and `tar` are none of them reliably present on
Windows.

It prunes ~85 MB the runtime never uses on a phone — `jmods/` (jlink input),
`demo/`, `man/`, `include/`, `lib/ct.sym`. `legal/` stays: these are GPLv2+CE
builds and the notices ship with them.

Result: **~167 MB staged, a 79.5 MB APK.**

#### Why assets rather than jniLibs

`jniLibs` only packages files ending in `.so`, which would silently drop
`libz.so.1` — a versioned soname the linker asks for by name — and would
flatten a tree the JVM walks from `java.home`. Assets keep the layout, and the
runtime is unpacked once into app storage on first start.

That is legal because the W^X rule bans **exec** from app storage, not
`dlopen` of an ordinary position-independent library. Only the launcher is
exec'd, and it alone lives in `nativeLibraryDir`.

Two packaging traps, both of which fail silently:

1. **`jniLibs.useLegacyPackaging` must stay `true`**, or nothing is extracted
   to `nativeLibraryDir` and there is no file to exec.
2. **The version marker must not be dot-prefixed.** aapt's asset filter
   includes `.*`, so `.java-major` never reached the APK and the app reported
   "no JVM bundled" on a build that had one. It is `java-major` now — the same
   trap as the UI bundle's `_next/`.

Ship one ABI per build (`-Pabi=arm64-v8a`); the runtime is architecture-
specific and packaging both doubles the download for no one's benefit.

#### Termux's prefix, and the dependency closure

The packages are `.deb` (an `ar` wrapping `data.tar.xz`) built for
`/data/data/com.termux/files/usr`, so unpacking strips that prefix. Their
`DT_RUNPATH` points there too, which `LD_LIBRARY_PATH` overrides by being
searched first.

Four dependency packages are load-bearing, established by reading `DT_NEEDED`
across the whole **closure**:

| Library | Needed by |
|---|---|
| `libandroid-shmem` | `libjvm.so` |
| `libandroid-spawn` | `libjvm.so`, `libjava.so` |
| `zlib` | `libzip.so`, `libjli.so` want `libz.so.1`; Android's system `libz.so` has no such soname |
| `libc++` | `libandroid-spawn.so` itself wants `libc++_shared.so` |

That last one is why the scan must cover the closure: a pass over only the
JRE's own libraries missed it, and the VM would not load. Four more are
referenced but only by things a headless server never touches — `libasound`,
`libiconv`, `libjpeg`, `liblcms2`.

**Symlinks are materialised as real copies** at stage time. zlib ships
`libz.so.1` as a link, which is exactly the soname the linker wants; keeping
them as links also fails on Windows, where this may well be built.

#### Verified on the emulator

| Configuration | Result |
|---|---|
| Java 21.0.12, x86_64 | Minecraft 1.21.11 boots to the EULA gate (`Thread.java:1583`) |
| Java 25.0.4, x86_64 | same, on the newer runtime (`Thread.java:1474`) |
| Java 21, **networking disabled** | still boots — the runtime is genuinely self-contained |
| Java 25.0.4, **arm64** | stages and verifies (`OS_ARCH="aarch64"`, ELF `Machine: AArch64`), **not** runtime-tested — no arm64 device here |

Two things only a real run found:

- **`LD_LIBRARY_PATH` must include `<javaHome>/lib` and the dependency
  directory**, in the child's *environment* — the linker reads it at process
  start, so setting it later is too late. Without it the VM boots and then dies
  the moment anything touches `java.nio`, several layers from the cause.
- **`java.io.tmpdir` has Termux's prefix compiled in**, a path that does not
  exist outside Termux, so anything writing a temp file fails on a path no one
  can explain. `JavaServerBackend` overrides it to `<serverDir>/tmp`.

#### The JNA stack trace at boot is expected

Every Java server start logs a wall of `com.sun.jna` / `oshi` stack traces
ending in `dlopen failed: library "libc.so.6" not found`. It is not a bug in
this host and it is not fixable here: JNA ships a **glibc** `libjnidispatch.so`
and Android is bionic, so it will not load wherever it is unpacked. Minecraft
wraps that probe in `ignoreErrors` and boots regardless. The cost is no
hardware detail in crash reports.

### Getting a server jar onto the device — `ServerJar`

Server jars **are** downloaded, and that is consistent with the runtime being
bundled: Play's rule is about executable code, and a jar is data a virtual
machine reads — the carve-out the policy names. `libjvm.so` is the VM and
cannot use it; `server.jar` is not and can.

`mod-installer.ts` in the `homerun` repo is the spec. Same endpoints, same
"resolve the Mojang manifest first" order — it names the required Java for
every loader, not just vanilla, and it is what turns "latest" into a version.

| Loader | Resolved from |
|---|---|
| vanilla | `launchermeta.mojang.com` version manifest → per-version meta → `downloads.server` |
| paper | `fill.papermc.io/v3` builds for the resolved version |
| everything else | refused, with the reason — Fabric, Forge, NeoForge and Quilt install by *running* an installer, which is a separate piece of work |

If the jar needs a newer Java than the build ships, that is said plainly before
anything launches, rather than surfacing as `UnsupportedClassVersionError`.

Three deliberate differences from the desktop, all because this is a phone:

- **Downloads resume.** The partial file is named after the artifact's own
  digest, so a resume can only ever continue the file it began — that is what
  makes it safe without the desktop's ETag sidecar. Both CDNs answer `206`.
- **Every download is checksum-verified**, vanilla by SHA-1 and Paper by the
  SHA-256 the desktop already fetches and discards. A mismatch is not retried:
  it means corrupt or substituted, not transient.
- **A failed lookup falls back to the jar on disk.** Hosting on a LAN with no
  internet is a real thing to want, and refusing to start a world that is
  already downloaded would be worse than starting it.

`homerun-jar.json` in the server directory records loader, version and digest.
It is what makes a restart free and a version change re-download.

**Which version and loader comes from the backend, not the UI.** The UI sends
only a name and a memory ceiling, so `BridgeRouter` reads the rest from
`/api/server/<id>/` (`HomerunApi`), exactly as `nativeServerManager` does — a
version changed on the web dashboard then takes effect on the next start. The
lookup lives in the router rather than the backend so the access token never
reaches the server process's environment. If it fails, vanilla-latest, which
is the desktop's fallback too.

### The EULA is accepted for the user

`JavaServerBackend.start` writes `eula=true` on **every** start, which is
byte-for-byte what the desktop does in `nativeServerManager.startServer`.

Worth being explicit about what that means: Mojang's EULA binds the person
operating the server, and no Homerun client — desktop, web or mobile — shows
it or asks. The product's own ToS says only that users "must adhere to any
applicable third-party terms of service", without naming Minecraft. That gap
is already logged as **T-6 / P1-18** in
`docs/privacy-tos-audit-2026-08-03.md` in the `homerun` repo, where it is
filed as a document-wording item rather than an engineering one.

This also retired a failure mode the earlier build had: a server stopping at
the EULA gate was reported as "the server stopped unexpectedly while
starting", because `start()` waits for `Done (…)!` and the process exited
first. Nothing stops there now.

#### Verified on the emulator

| Case | Result |
|---|---|
| Vanilla, cold, no account | 26.2 resolved, 58 MB fetched, SHA-1 verified, `Done (3.249s)` — **27 s** end to end |
| Restart | no re-download, `Done (0.375s)` |
| Paper 1.21.4 | build **232** picked (newest STABLE), SHA-256 verified, `plugins/` and `bukkit.yml` present, `Done (21.759s)` |

Both were driven straight over the bridge with no login, so the path that ran
is the no-token one: API lookup fails, vanilla-latest.

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

The server jar lives in that directory too, one copy per server, which is what
the desktop does. On a phone that is worth revisiting — two servers on the same
version is ~110 MB of identical bytes — but one-server-at-a-time makes it rare
enough not to have earned a shared cache yet.

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

**"Homerun for Android cannot host <loader> servers yet".** Working as
intended — only vanilla and Paper resolve. The server's `TYPE` comes from the
API, so this is what a Fabric or Forge server created on desktop does when
someone tries to start it on a phone.

**The jar re-downloads on every start.** `homerun-jar.json` is missing or its
digest does not match what the endpoint now publishes. For Paper that is
expected after an upstream build: a new build is a new file.

**"needs Java N, and this version of Homerun ships Java M".** The bundled
runtime is older than the Minecraft version asks for. Restage with
`npm run jre:android -- --java <N>` and rebuild.

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

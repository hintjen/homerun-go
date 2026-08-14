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
| `ServerSettingsWriter.kt` | Writes the config files the server reads at boot. |
| `WireProxy.kt` | The gateway tunnel that makes the server reachable. |
| `HomerunApi.kt` | Reads a server's version, loader and tunnel from the backend. |
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
world it was mid-save on — on Windows a terminate is `TerminateProcess`, which
ends the VM without running its shutdown hook, so the world is never flushed
and the on-stop backup captures a stale auto-save.

The escalation is `homerun_core::minecraft::jvm::stop_ladder`: console, wait
30 s, terminate, wait 8 s, kill. The **supervisor** climbs it — it holds the
process and its stdin, so every rung including the first is carried out there.
A server stopped before its console existed gets the same ladder minus that
first rung, which is the core's answer to `console: false` rather than a branch
in either host. This file only asks for a stop.

**The heap ceiling is the core's too.** This file measures the device;
`jvm::heap_mb` decides what fraction of it is safe to hand over — a third,
because Android kills *the whole app* under memory pressure, so an
over-generous heap costs you the app that was hosting, mid-save. Same call
gives `-Xmx`/`-Xms`, `nogui`, and the EULA file, all of which any host running
this server would pass. Only the Android-specific flags — `libjvm`,
`java.io.tmpdir`, `LD_LIBRARY_PATH` — are built here.

**Player tracking reads the console, not RCON.** Vanilla prints join and leave
lines; parsing them costs nothing and avoids a port, a password and a second
protocol to keep alive. The wording is vanilla's, so the roster is best-effort
and never blocks anything. RCON becomes worth adding when moderation
(kick/ban/op) lands.

That reading happens **once, in the supervisor**, which is already looking at
every line as it arrives. This host asks it who is playing (`nativePlayers`)
and whether the console has said `Done (…)` yet (`nativeState`) rather than
classifying the same lines a second time — two parses of one console is how
two answers to one question appear, and only one of them can be right.

**Heap is capped at a third of device RAM.** Android kills *the whole app*
under memory pressure, not just the server, so an over-generous heap does not
lose you a server — it loses you the app that was hosting it.

### Who owns a server, and in what order — `homerun-core`

Neither question is answered in Kotlin any more. `ServerHost.lifecycle` is a
`Core.Lifecycle`, and this backend reports only what it can see — a call
arrived, a process spawned, a process exited — while `homerun_core::lifecycle`
answers what any of it meant.

It matters because the same bug was written three times in one week: a server
that is *starting* or *stopping* is still this device's. Report it idle in
either window and the UI's reconcile loop reads a missing id as a start issued
from another device, asks the API to `force_link_up`, and regenerates the
gateway's keys underneath a launch that already resolved its tunnel config —
a tunnel that handshakes and carries nothing.

| Question | Core call |
|---|---|
| may this start proceed, or is it a duplicate | `startRequested` |
| may this stop proceed, and is anything spawned yet | `stopRequested` |
| what did that exit mean — crash, stop, or a launch since replaced | `exited` |
| which ids does `native-server-active-ids` return | `activeIds` |
| may this state change be announced | `mayAnnounce` |
| must this launch wait for a previous engine | `awaitPreviousExit` |
| must starting cancel an on-stop backup | `supersedesOnStopBackup` |

State is opaque and lives in the host — it goes in, a new one comes back, like
a `HandshakeWatch` — so there is no native handle to free. Access is
synchronised because starts arrive on the bridge's coroutines while exits
arrive on the process-watcher thread.

The **order** of a launch comes from `Core.launchPlan`, and `LaunchOrder`
enforces it: a step that arrives before one the plan puts ahead of it throws
here rather than surfacing months later as a re-downloaded world or a green
card for an unreachable server. The plan also marks which steps are
checkpoints, so a stop that arrived mid-launch is honoured at the right
moments without this file remembering which those are. Writing that plan out
found a real disagreement: the launch waits for a previous engine *before*
restoring a world, because a mobile launch writes the server directory before
it spawns, and the core's first draft had the wait later.

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

**This is the second of two refusals, and they are not the same question.**
`homerun-core::minecraft::hosting` runs first, in `native-server-start`, and
asks whether this *device* can host this server at all — it is where Bedrock is
turned away, and it is shared with iOS so the two apps refuse the same things in
the same words. The table above is narrower: given that the device could host
it, can this build get a jar for that loader. Forge passes the first gate here
(a spawned engine can load mods; iOS's linked one cannot) and is refused by the
second, with a reason about installers rather than about phones.

Keeping them separate is deliberate. Folding the loader table into the hosting
rule would put "we have not written the Forge installer yet" — a fact about this
build, changeable next release — into the crate that iOS also asks.

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

#### Never downloading a jar this device already has

A 58 MB pull over mobile data is the most expensive thing a launch can do, so
there are three ways out of one before it starts. All three verdicts come from
`homerun_core::minecraft::jar` — this host gathers facts and carries out
answers.

**1. The marker agrees.** `cache_decision` returns `use`, and nothing is
hashed. The common case: restarting a server whose version has not changed.

**2. The marker is missing or stale, but the jar is right.** The marker is a
*cache* of something the file itself proves, and it can be wrong in two ways
that would cost a re-download for nothing:

- the finished download is renamed into place and the marker is written after
  the retry loop returns, so a process death in between leaves a perfect jar
  that nothing remembers;
- a world restore rewrites the server directory and can land an older
  snapshot's marker beside a newer jar.

So `cache_decision` answers `verify`, naming the algorithm; the host hashes the
file and asks again; `adopt` rewrites the marker and launches. This is what the
desktop's `verifyExistingJar` does. The difference is that the marker lets the
common case skip hashing entirely, where the desktop pays for it every launch.

**3. Another server on this device has it.** `files/jars/<digest>.jar`, a
sibling of `servers/`, content-addressed by `jar::cache_key` so every server
asking for one Minecraft version names one entry. Before this existed, a fifth
server on Minecraft 26.2 downloaded 58 MB that four other servers already had.

**This saves downloads, not disk.** A server takes its own *copy* out of the
cache, so the jar is still duplicated per server, and the cache itself is one
more copy on top. That is deliberate, for two independent reasons:

- Android refuses `link(2)` in app-private storage. `ln` there fails with
  `Permission denied`, so the hard link that would have collapsed those copies
  into one file is simply not available.
- Backups cover the **whole server directory** — `engine.backup(repo, dir, dir,
  …)`, not just `world/`. A symlink, which Android *does* allow, would restore
  on another device pointing at a path that does not exist there.

The duplication buys the thing that actually costs a player something: minutes
and mobile data. It also means a world restored from another device arrives
with its jar, and the digest check in step 2 adopts it instead of fetching it
again.

A cache hit is **verified, not trusted**. The entry is hashed and put through
the same `cache_decision`, because a corrupt entry would be handed to every
server that asks for that version, which is far worse than one bad download.
An entry that disagrees with its own name is deleted.

`cache_key` refuses a digest that is not hex. That string arrives in a
publisher's JSON and is about to become a path, and `..` in that position
writes outside the directory.

**Eviction is by reference.** An entry no server's marker names is dropped —
after a download, and when a server is deleted, which is the moment that
actually orphans one. Deleting an entry can never cost a server its jar,
precisely because it is a copy — the server's own is untouched. That is why an
unreadable
marker is allowed to prune rather than having to abort the sweep, and why
`.part` files are skipped — one of those is a download someone may still be
resuming, and by definition no marker names it.

**Which version and loader comes from the backend, not the UI.** The UI sends
only a name and a memory ceiling, so `BridgeRouter` reads the rest from
`/api/server/<id>/` (`HomerunApi`), exactly as `nativeServerManager` does — a
version changed on the web dashboard then takes effect on the next start. The
lookup lives in the router rather than the backend so the access token never
reaches the server process's environment. If it fails, vanilla-latest, which
is the desktop's fallback too.

### Reaching the server from outside — `WireProxy`

A phone on cellular sits behind carrier-grade NAT. There is no router to
forward a port on and no UPnP to negotiate with, so unlike desktop **there is
no fallback**: without the tunnel a server runs perfectly and nobody in the
world can join it.

`wireproxy` dials the Homerun gateway as a WireGuard peer. Players connect to
the gateway, the gateway DNATs to a fixed port on the WireGuard interface,
wireproxy accepts there and forwards to loopback. Same gateway, same config
format and the same binary lineage as desktop — `wireproxyConfig.ts` is the
spec, and a divergence is a bug by definition.

**No VPN permission is involved**, and that is the most important property of
the design. wireproxy terminates WireGuard in its own userspace netstack: the
`Address` in the config is virtual, inside that process, never registered with
Android. So no TUN device, no `VpnService`, no permission prompt, and none of
the Play policy surface a real VPN carries.

Built by `npm run wireproxy:android` from the private fork
`hintjen/wireproxy-fork`, checked out beside this repo or pointed at with
`HOMERUN_WIREPROXY_SRC`. The fork is required, not a preference: it adds
`[UDPServerTunnel]`, which upstream lacks and which Bedrock, crossplay and
voice chat all need.

Three things about that build are easy to get wrong, so
`scripts/build-wireproxy.js` asserts all three by parsing the ELF rather than
trusting the toolchain:

- **`GOOS=android`, never `GOOS=linux`.** A `linux/arm64` PIE binary builds
  without complaint and then will not start, because Go stamps `PT_INTERP` as
  `/lib/ld-linux-aarch64.so.1` — a glibc path bionic does not have. Same class
  of failure as the JNA problem above.
- **PIE is mandatory** since API 21. `GOOS=android` gives it by default.
- **arm64 needs no NDK; x86_64 does** — Go reports `android/amd64 requires
  external (cgo) linking`. Emulator-only, so the ship path is unaffected.

It ships as `libwireproxy.so` in `jniLibs` and is exec'd from
`nativeLibraryDir`, exactly like the JVM launcher and for the same reason. It
is a Go executable, not a library.

#### Where the credentials come from, and where they must not go

The gateway provisions the WireGuard peer asynchronously once a server is
marked running, so at launch they usually do not exist yet. `HomerunApi.awaitTunnel`
polls `/api/server/<id>/` for `config.links[0].native_config` — 3 s apart, 20
attempts, the desktop's numbers. It runs **in parallel** with the JVM booting,
because a minute spent waiting is a minute the world could have been
generating.

The legacy provisioner mints fresh keys per session, so a config identical to
the pre-launch snapshot is the *dead previous set* and using it fails the
handshake. Gateway v2 reuses credentials deliberately, so that staleness check
is skipped for `provisioner == "gateway2"` links — without the exception a v2
link would poll until timeout on every start.

`ServerConfig.resolveTunnel` is a **function, not a value**. Partly so the slow
poll overlaps the boot, but mainly because it closes over the user's access
token: `ServerConfig.extra` is forwarded into the server process's
environment, and a credential must never be able to reach it.

#### `running` means reachable

`start()` waits for `Done (…)!`, then brings up the tunnel, and only then
reports `running`. The server accepting connections on loopback is not the
same as players being able to reach it, and announcing `running` first is how
a server looks healthy to everyone except the people trying to join. The
desktop learned this the hard way — its comment about "a silently-rejected
start masquerading as running" is the same bug.

**A tunnel failure stops the server, in both of its forms.** A server nobody
can reach is not a working server, and leaving one up would be worse than
stopping it — it looks healthy to its owner and is unjoinable to everyone
else. This is the desktop's rule, not a mobile one:
`pollAndProvisionWireproxy` throws when the config never arrives, and
`server-started`'s catch calls `stopServer`.

| Failure | `kind` | When |
|---|---|---|
| Never came up | `provisioning` | The gateway did not provision within 60 s, or wireproxy would not spawn |
| Came up, then died | `handshake` | Ten consecutive `Handshake did not complete after 5 seconds` (~50 s) — the gateway's keys were regenerated and these credentials are permanently dead |

Both emit **`native-server-network-error`** before stopping. That event is
load-bearing: the stop goes through the normal clean path, so without it the
UI cannot tell a tunnel failure from the user pressing Stop, and the card just
flips to stopped with no explanation. The shared UI already listens for it and
toasts wording specific to each kind.

The stop itself is graceful — `stop` on stdin, world saved — because the
world is not what failed.

One consequence worth knowing when testing: **a start with no account now
stops.** No token means no tunnel, and no tunnel means no server, exactly as
on desktop. Driving a server up over the bridge with a synthetic envelope
needs a real token now.

### The settings a player chose — `ServerSettingsWriter`

A server reads its config **once**, before it accepts anyone. So the files are
written on every launch, between the jar landing and the JVM starting.

That timing is what makes a change on the web dashboard take effect on the next
start — and, more importantly, what makes a *removal* take effect. The files
are the source of truth at launch, so a de-opped player stops being an operator
because `ops.json` no longer lists them. An add-only pass over RCON, which the
desktop used to do, leaves them op forever.

Until this existed **Android wrote nothing**, and every server ran vanilla
defaults no matter what the creation wizard was told. It was not subtle once
looked for: a server created with `MOTD=Android tunnel test` answered a
server-list ping with `"description": "A Minecraft Server"`.

#### This file does not know what Minecraft is

It asks the core three questions and does as it is told:

```kotlin
val existing = Core.configInputs(env).mapNotNull { ... readText(it.encoding.charset) }
val resolved = Core.requiredLookups(env, gameType).mapNotNull { fetchIdentity(it) }
val files    = Core.configFiles(env, gameType, port, bindAddress, existing, resolved, now)
for (file in files) File(dir, file.path).writeText(file.contents, file.encoding.charset)
```

No property keys, no file names, no encoding constant, no UUID derivation, no
ban semantics. All of that is `homerun-core::minecraft::settings` behind the
`Game` trait, so this app, iOS and the desktop cannot drift — see
[`core-bridge.md`](./core-bridge.md).

**The encoding travels with each file, for reading as well as writing.**
`server.properties` is latin-1. Decoding it as UTF-8 turns `§` — the colour
code marker in a MOTD — into U+FFFD, and writing that back as latin-1 turns it
into `?`. A player's coloured MOTD destroyed by a launch that changed nothing.

**Identity lookups are only what the core cannot derive.** An offline server
returns *no* lookups and makes no Mojang requests, because its UUIDs are an
MD5 of the name and the core computes them. Asking the network there would be
both wasted and wrong: an online UUID does not match an offline server's idea
of the same player. A name that fails to resolve is left out, and the core
skips it rather than writing an id that can never match.

**Nothing here throws.** A settings failure must not stop a server that would
otherwise run, so every failure is logged and surfaced to the console. The
flip side is that a bug looks like silence — which is why the exact JSON this
sends is pinned by a test in the core.

#### Two desktop bugs fixed rather than copied

- **Online mode is derived once.** The desktop computes it twice and the two
  disagree: `server.properties` uses `crossplay && vanilla` and ignores
  `ONLINE_MODE`, while UUID resolution uses `ONLINE_MODE`. Both mismatches are
  reachable, and both silently break op-ing.
- **Numbers cannot become `NaN`.** `parseInt(env.MAX_PLAYERS)` on a
  non-numeric value writes the literal text `NaN` into `server.properties`.

#### Not yet proven end to end

The logic is covered by tests, including the wire shape, but writing settings
needs a logged-in server start and that has not been run. The app installs,
launches and loads the native library; the last mile is untested.

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

The same tick watches the roster, so a join reaches the UI within a second.
Performance sampling is a **separate** job on the interval the core asks for —
the history crosses JNI by value, and offering it every second would ship up to
360 samples in each direction thirty times over for one kept point.

**One server at a time.** Enforced in the crate and again here. The engine
distinguishes worlds by process CWD, so a second concurrent run would quietly
share the first one's world. Attempting it raises `AnotherServerRunning`,
whose message is written for players because they are the ones who see it.

### The console is the supervisor's, including Homerun's own lines

`JavaServerBackend` used to keep a ring buffer of its own — a reimplementation
of `log_buffer.rs` — for one reason: Homerun writes lines *before* there is a
run to have a console. The jar, the runtime, the world coming back from a
backup, the tunnel. It now writes those through `nativeNote`, so there is one
buffer and one ordering.

Three things about this that are easy to get wrong, all of which were:

- **The pump is the only emitter.** `note` writes and does not emit. Doing
  both sends every line twice — visibly, as a doubled
  `[Homerun] Connecting to the Homerun gateway…`.
- **The pump starts at `announceStarting`, not at the spawn.** It is what turns
  console lines into events, and the minutes before the spawn are exactly the
  ones worth watching. Starting it later delivered them in one burst at the end
  of the wait.
- **It stops after the on-stop backup, not at the exit.** A backup writes
  `[Backup] …` for minutes after the JVM is gone. The paths that end a launch
  without ever spawning stop it themselves, because no exit will arrive to do
  it for them.

`reset()` calls `nativeConsoleBegin`, which is what empties the previous run's
console. Not the first note — see [`ffi.md`](./ffi.md#what-clears-it-and-why-it-is-not-start)
for why that distinction is load-bearing.

## What the metrics actually measure

Both backends now read counters and let `homerun_core::metrics` decide what
they mean, so the two graphs — and the desktop's — cover the same span. What
still differs between them is *which process* is being measured, and that is
the one thing worth reading carefully here: the UI presents all of it as fact.

### The JVM backend — real numbers, from `/proc`

The server is a child process, so it can be measured properly. The Insights
panel was empty before this existed: `memoryUsage` returned a null `usedKb`,
`cpuUsage` returned null, and `perfHistory` was not overridden at all.

| Channel | Source |
|---|---|
| `native-server-get-mem-usage` | `VmRSS` from `/proc/<pid>/status`, against the heap ceiling the JVM was given |
| `native-server-get-cpu-usage` | the rate of the last two samples, worked out by the core |
| `native-server-get-perf-history` | `homerun_core::metrics`, sampled every 30 s |

**The host reads counters, never percentages.** `ProcMetrics` hands over
resident KiB and cumulative CPU seconds; `metrics::History` decides what they
mean, whether a reading is due, and how much history to keep — see
[`shared-core.md`](./shared-core.md). A percentage is a difference between two
moments, and computing it per platform is how three hosts ended up with three
different graphs.

Two things worth knowing:

- **`cpuPercent` is fractional.** An idle Minecraft server sits under one
  percent, and an `Int` drew a flat zero line for a server measurably using
  0.6 % of a core.
- **The pid comes from the launcher, not from `Process`.**
  `java.lang.Process.pid()` does not resolve against the Android SDK, and the
  private field behind it is on the non-SDK interface list. So
  `homerun-java-launcher` prints `[launcher] pid=<n>` as its first console
  line, before the VM exists, and the backend reads it from there.

### The Pumpkin backend — the same numbers, from this process

The engine is linked in, so there is no child to point at and `Process.myPid()`
is the right pid: while a world is up this app *is* the server. That makes an
app-wide number the honest one here, where on the JVM path it would be a
near-miss.

| Channel | Source | Caveat |
|---|---|---|
| `native-server-get-mem-usage` | `VmRSS` from `/proc/self/status`, against `largeMemoryClass` | RSS against a heap-shaped ceiling can read over 100 %. Same on the JVM path. iOS reports the equivalent limit — what the app is killed for exceeding — so the two gauges now ask the same question. |
| `native-server-get-cpu-usage` | the rate of the last two samples, worked out by the core | Was null until this backend adopted `metrics`. |
| `native-server-get-perf-history` | `homerun_core::metrics`, sampled every 30 s | Was a local 1 s × 1800 deque whose comment claimed the desktop's window — the desktop's is three hours, that was thirty minutes. |
| `native-server-get-uptime` | engine `startedAtMs` | Real. |
| `native-server-get-ops` | empty | The engine does not expose an op list yet. |

## Storage

Worlds live under `filesDir/servers/<serverId>` — app-private and preserved
across updates. **Not `cacheDir`**: the system may delete that under storage
pressure, and it would take the player's world with it.

The server jar lives in that directory too, one copy per server, which is what
the desktop does and what backups require — they cover the whole server
directory, so a jar shared by reference would restore on another device
pointing at nothing.

`filesDir/jars/` is a sibling holding one copy of each jar any server has
fetched, keyed by digest. It saves the **download**, not the disk: see
[Never downloading a jar this device already has](#never-downloading-a-jar-this-device-already-has).

### The id is checked before it is a path

`serverId` reaches the host verbatim from the page, so `requireValidServerId`
(in `ServerBackend.kt`) refuses anything outside `[A-Za-z0-9._-]{1,128}`, and
anything starting with a dot. Both backends call it from `dataDir`, which is
the one place either builds a path out of an id.

Without it, `native-server-delete` with an id of `../..` deletes the app's
private root — `shared_prefs` with the credentials and the device token, the
unpacked JRE, the jar cache and every world. The `activeIds` membership check
is not a guard against that: it asks whether an id is *busy*, and any invented
id passes.

An allowlist rather than a canonical-path check at each sink, because the id is
a path segment in more places than the filesystem — `/api/server/<id>/`,
restic's recorded basename, `cacheDir/restore-<id>` — and a rule about the id
holds at all of them, including the one somebody adds next.

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

**The jar downloads and then nothing happens — no JVM, no error, no crash
report.** Usually DNS in restic rather than anything about the JVM. `start`
restores the world from the backup repository *before* it launches, so a
restic that cannot resolve `backups.gethomerun.app` retries with backoff for
ever and the launch never proceeds. Nothing crashes, so nothing is reported,
and `crash::report` has no console output to read because no server ever ran.

Confirm with `adb logcat --pid=$(adb shell pidof app.gethomerun.mobile)` and
look for **`HomerunBackup`** — filtering on `HomerunJava` or `HomerunHost`
misses it completely. `lookup … on [::1]:53: connection refused` means a Go
binary was built without cgo; see
[`building.md`](building.md#cgo-is-mandatory-on-every-android-target).

This cannot reproduce on the emulator, which is why it shipped.

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

**"Failed to establish network tunnel… Stopping server."** Expected with no
token, or when the gateway never provisioned one within 60 s. Check the API
returned `config.links[0].native_config` for this server. The server stops on
purpose — see the tunnel section.

**The server is running but nobody can join, and the console says nothing.**
Look for `HomerunTunnel` in logcat. wireproxy retries a failed handshake
forever, so a dead credential set looks like a slow network for the first ~50
seconds by design.

**"could not be started" from the tunnel.** `libwireproxy.so` is missing for
this ABI. Run `npm run wireproxy:android` (arm64) or
`npm run wireproxy:android-x86_64` (emulator).

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

**The roster is empty, or a join never reaches the UI.** Both hosts read it
from the supervisor now, so the question is whether the supervisor saw the
line: check the console for the join, then `homerun_server_players`. It
returns `null` — not an empty roster — until the run reaches *running*, so a
roster that is empty during startup is correct rather than broken.

**A start hangs at "starting" with a healthy console.** `start()` waits for
the supervisor to report *running*, which happens on `Done (…)`. A server
whose wording the core does not recognise boots fine and is never announced;
the fix belongs in `homerun_core::minecraft::console`, where both platforms
get it, and not in a host.

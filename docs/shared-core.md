# The shared core

`rust/homerun-core` holds the host logic all three Homerun apps need: which
server jar to run, how the tunnel config is laid out, when a tunnel counts as
failed, what state a server is in, and how the gateway's credentials are judged
fresh or stale.

```
npm run test:core        # the suite
npm run test:core:lint   # fmt + clippy, both enforced
```

## Why it exists

Bringing Android up meant hand-mirroring, from TypeScript into Kotlin, every
decision the desktop already made. That drifts, and it already had. Two
divergences existed **before** the crate was written:

- The desktop picks Paper's **oldest** build — an alpha. The v3 API returns
  builds newest-first and `mod-installer.ts:803` takes the last array element.
  Android picked the newest stable, so the two apps were installing different
  server software from the same request.
- The desktop pushes an instance report the moment a server goes running;
  Android only sent one on its 30-second heartbeat. A server that was genuinely
  up read as stopped in the UI for up to half a minute.

Neither was a bug in one place. Both were two implementations of one decision,
drifting apart. That is what this crate is for.

## What belongs here

**Decisions and shapes. Not transport, not processes.**

Every function is pure: hand it the JSON an endpoint returned, or the state
something is in, and it tells you what that means. It opens no sockets, spawns
nothing, and has no async runtime.

That is not a limitation, it is the point:

- pure functions make the awkward cases — a version that does not exist, an
  array ordered the other way round, a half-written config — exhaustively
  testable with no network and no device
- the FFI surface stays a plain C ABI rather than a runtime bridge
- **iOS cannot spawn a process at all**, so a "shared supervisor" that owned
  process handling could never have been shared with it. What *can* be shared
  is everything such a supervisor would have decided.

The platform keeps what only the platform can do: making the request, spawning
the JVM, spawning the tunnel, sampling CPU, talking to the UI.

| Shared (this crate) | Platform |
|---|---|
| jar resolution, checksums, Java-level check | HTTP |
| wireproxy config generation | process spawn/kill |
| link parsing and the staleness rule | JVM launch |
| server state machine, exit classification | RCON transport |
| who owns a server, and the order a launch runs in | doing each step |
| handshake supervision and its threshold | CPU/memory sampling |
| console ring buffer, join/leave/ready parsing | bridge IPC |
| which config files a server needs, and their contents | writing them |
| how a player name becomes the id a server matches | fetching it |

## Layout

The crate splits in two, and the split is the point: **a game is one
implementation, not the core.**

```
game.rs        the capability surface every game exposes
launch.rs      the order a launch runs in, as data
lifecycle.rs   who owns a server right now, and what an exit meant
link.rs        gateway credentials and the staleness rule
metrics.rs     what a run is costing, and how much of it to remember
properties.rs  key=value config merging, comments preserved
state.rs       handshake supervision, exit classification
tunnel.rs      WireGuard config from a list of forwards
backup.rs      what a backup failure meant, and who holds the lease
bundle.rs      OTA manifests: signature, version ordering, rollback
device_ws/     the dashboard socket's frames, auth scope and PROXY v1 — no socket
md5.rs         a hash nothing trusts, to avoid a dependency
sha1.rs        checksums for downloaded artifacts
reporting/     crash reports and the stats a run reports home
minecraft/     jar, jvm, console, settings — one implementation of game.rs
```

`lifecycle`, `launch` and `metrics` are the only three that hold state or
describe a sequence, and each stays pure by refusing to own either. `lifecycle`
and `metrics` are serialised and handed back to the caller on every call — the
host holds them, the same way it holds a `HandshakeWatch` — so there is no
native handle to leak and no second copy to disagree with the host's. `launch` returns a list of
steps and executes none of them, because every step is something only a
platform can do; what stops being the platform's is *which comes next*.

Nothing above `minecraft/` knows that a Java server listens on 25565 or what
`Done (12.431s)!` means. `tunnel` renders whatever `Forward`s it is given;
Minecraft is what names them.

| Module | Reference implementation |
|---|---|
| `lifecycle` | `nativeServerManager.ts` — `runningServers` ∪ `pendingStartup`, and `waitForSupervisorIdle` |
| `metrics` | `nativeServerManager.ts` — `takePerfSample`, `schedulePerfSample`, `stopPerfSampling` |
| `launch` | `nativeServerManager.startServer`, read top to bottom |
| `minecraft::jar` | `src/electron/mod-installer.ts` |
| `minecraft::jvm` | `supervisor.js` — its spawn arguments and `stopServer` |
| `minecraft::console` | `JavaServerBackend` (Android) + supervisor log handling |
| `minecraft::settings` | `writeServerProperties`, `writeOpsAndWhitelistFiles` |
| `tunnel` | `src/electron/wireproxyConfig.ts` |
| `properties` | `mergeServerProperties` |
| `link` | `pollForNativeConfig` in `nativeServerManager.ts` |
| `state` | `onServerFullyRunning` + supervisor exit handling |

The desktop is the reference for all of it, and each module names the file its
behaviour came from. Where this crate deliberately differs, it says so and why
— `minecraft::jar::paper` and `minecraft::settings`' online-mode note are the
clearest examples.

### The `Game` trait is frozen

`game.rs` is `game/v1`: **additive changes only**. Three codebases build
against it in parallel, and because the bridge resolves methods by string at
runtime rather than by symbol at link time, a signature change breaks them
silently for anyone who has not rebuilt.

It is enforced rather than requested — `game.rs`'s `tests::Frozen` implements
the trait against the exact signatures, so any change fails to compile with a
pointer back to the rule. New methods need defaults; new struct fields need
`#[serde(default)]`.

**Artifact resolution is deliberately outside the trait.** Minecraft resolves a
jar from Mojang's manifest; another game might resolve a Steam depot, a
container image, or nothing at all because it ships in the app. There is no
honest common signature, and forcing one produces a method every implementation
ignores half the arguments of.

## The tests are the deliverable

573 tests, and they are checked for teeth rather than counted. Deliberate
regressions are introduced and each must be caught by its own test:

| Regression | Caught by |
|---|---|
| take Paper's last array element, as the desktop does | `paper_picks_the_newest_stable_not_the_last_element`, `paper_is_insensitive_to_array_order` |
| make `ListenPort` follow the local port | `a_nonstandard_local_port_moves_only_the_target` |
| drop the gateway-v2 staleness exception | `gateway_v2_accepts_an_unchanged_config` |

If you change behaviour here, do the same: break it on purpose first and check
something fails. A test that cannot fail is documentation with a runtime cost.
The `tests-that-bite` skill has the failure modes this keeps catching — most
often a test that performs the very step it is meant to be testing, and so
passes right through the step being removed.

`the_desktop_expression_would_pick_an_alpha` is worth knowing about — it writes
out the desktop's algorithm and asserts we disagree with it. If PaperMC ever
flips the ordering back, that test starts failing, which is exactly when
someone should look at it again.

## Who uses it

**Both mobile hosts.** Dispatch lives in `core_dispatch.rs` with no platform in
it, and each host is a thin adapter over the same function — Android via JNI,
iOS via `homerun_core_call`. See [`core-bridge.md`](./core-bridge.md).

These moved out of Kotlin entirely:

| Was | Now |
|---|---|
| `ServerJar.resolveVanilla` / `resolvePaper` | `minecraft::jar::resolve_version`, `vanilla`, `paper` |
| which staged Java runtime runs a jar, and on-disk comparison | `minecraft::jar::select_runtime`, `OnDisk::satisfies` |
| whether the jar already on disk can be kept | `minecraft::jar::cache_decision` |
| what to call a jar in the device-wide cache | `minecraft::jar::cache_key` |
| `WireProxy.render`'s string list | `tunnel::Config::render` |
| `WireProxy`'s handshake counter and threshold | `state::HandshakeWatch` |
| `JavaServerBackend`'s `DONE`/`JOINED`/`LEFT` regexes | `minecraft::console::is_ready`, `joined`, `left` |
| the exit-code-to-state rule | `state::exit_state` |
| `JavaServerBackend`'s `stopRequested`, `startingId`, `claimStart` | `lifecycle`, via `lifecycle.apply` / `lifecycle.query` |
| `BridgeRouter`'s active-id bookkeeping | `lifecycle::active_ids` |
| the order of `JavaServerBackend.launch`, written out longhand | `launch::plan` |
| `heapMb`, the `-Xmx`/`-Xms`/`nogui` args, `eula.txt` | `minecraft::jvm::heap_mb`, `heap_options`, `PROGRAM_ARGS`, `EULA_CONTENTS` |
| the stop escalation and its 30 s / 8 s waits | `minecraft::jvm::stop_ladder` |
| `START_TIMEOUT_MS`, `PREVIOUS_EXIT_WAIT_MS` | `minecraft::jvm::Limits` |
| six player-facing refusals, worded per host | `minecraft::jvm::Refusal` |
| — (Android wrote no config at all) | `minecraft::settings`, via `game.configFiles` |

That last row is the largest: every setting a player chose was silently
discarded before it existed. `ServerSettingsWriter.kt` now asks the core which
files to read, whose identity to fetch, and what to write — it knows no
property keys, no encoding, and no UUID derivation.

The two jar-cache rows are worth a note on shape, because they are the first
core call that answers *in two steps*. `cache_decision` can need a digest the
caller has not paid for — hashing 58 MB to settle a question a marker file
usually settles is the wrong default — so it replies `verify`, naming the
algorithm, and the host asks again with the answer. Same pattern as
`launch::plan`: the core decides, and what it decides includes *what it needs
to know next*. See
[`android-server-backend.md`](./android-server-backend.md#never-downloading-a-jar-this-device-already-has)
for what the host does with each verdict.

Verified end to end on an emulator against the dev backend after the swap: jar
resolved and downloaded, tunnel config rendered, handshake completed, `Done`
detected, and a Minecraft ping from the public internet answered. The settings
path is covered by tests including the exact JSON a host sends, but has not yet
been through a real logged-in server start.

Panics cannot cross either boundary — one unwinding through JNI aborts the VM
and one through the C ABI is undefined behaviour — so every call runs inside
`catch_unwind` and a panic becomes an ordinary error string. A `cfg(test)`
method panics on purpose to prove it.

## Still to do

**The desktop has not adopted anything**, so its TypeScript copies are still
the ones that ship, Paper bug included. That is deliberate: it works, and
rewriting a working supervisor is a well-known way to break a product. Start
with the pure pieces behind the existing TypeScript interfaces via napi-rs, and
leave `supervisor.js` owning processes. Adding Rust to the desktop build is
real CI work and a new way for a release to fail — worth doing deliberately,
not as a side effect.

**The console ring buffer is still Kotlin's.** `minecraft::console::Console`
exists and is tested, but the cursor is read on a hot path and moving it means
either a handle to free or a JSON round trip per line. Worth doing, not urgent.

**iOS reaches most of this now.** `Core.swift` and the C surface are in place,
and the host goes through them for tunnel rendering, handshake supervision,
lifecycle, launch order, server settings, backup decisions and the performance
graph — the last of which replaced the arithmetic half of `DeviceMetrics`.
`DeviceRegistrar` and `HomerunAPI` are the obvious next candidates, and Pumpkin
will need its own artifact resolution rather than borrowing `minecraft.jar.*`.

**Nothing here has run on arm64.** Every verification so far is x86_64 on an
emulator. That gap needs a physical device, not more code.

**Process supervision stays per-platform** and should. It cannot be shared with
iOS, and pretending otherwise is where this design would go wrong.

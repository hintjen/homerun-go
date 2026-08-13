# iOS Server Backend

## Overview

This is the half of the app that actually hosts Minecraft. The bridge layer
talks only to the `ServerBackend` protocol, so the twenty-odd
`native-server-*` channels are written once and would keep working against a
different engine; `PumpkinBackend` is the single iOS implementation.

The shape of everything here follows from one platform fact: **iOS cannot
spawn processes.** There is no child process, no pid, no stdio pipe, and no
JVM. The server is a Rust library call that blocks a thread for as long as the
world is up, running inside the same process as the UI — which means it shares
the app's memory budget, and when iOS decides the app is using too much, the
whole thing goes, not just the server.

## The server thread — `PumpkinBackend.startServerThread`

> **Load-bearing: the thread needs a 16 MB stack.**

```swift
let thread = Thread { ... }
thread.stackSize = 16 * 1024 * 1024
thread.start()
```

The 512 KB default overflows inside the engine and takes the app down **with
no panic report and no crash log** — the single most confusing failure this
code can produce, because there is nothing left to read afterwards. Neither
`Task` nor `Thread.detachNewThread` lets you set a stack size; only a
configured `Thread` does. This was rediscovered the hard way in the prototype;
do not rediscover it again.

`homerun_server_start` blocks on that thread for the server's entire lifetime.
Its *return* is the signal that the run ended — cleanly if a stop was
requested, as a crash otherwise.

## Starting, and why there is no timeout

`start(serverId:config:)` returns only once the server is accepting
connections. It polls `homerun_server_state()` until it reports running, and
there is **no deadline**: first boot generates a world and legitimately takes
minutes. The console is streaming the whole time, so the player can see it
working. The only things that end the wait are the server coming up or the run
ending.

This is the same rule as the bridge's no-call-timeout (PROTOCOL.md §5), for
the same reason, and it is why `native-server-start` must never be given one.

## Settings — what a player chose, and what the engine gets

A player's choices arrive as the API's `environment_variables`. **Pumpkin does
not read `server.properties`** — zero references in the whole fork — so this
host does not write one. The settings are applied in memory, by assignment onto
the config structs the FFI already mutates for the port.

Do **not** write the core's `ops.json` here either. Pumpkin's field is
`bypasses_player_limit` where vanilla's is `bypassesPlayerLimit`, and its
loader has no serde default, so a vanilla-shaped file **panics it at startup**.
Its lists live under `<serverDir>/data/`, not the server directory.

The path is: `BridgeRouter+Server` puts the API's env and game type on
`ServerConfig` → `PumpkinBackend.resolveSettings` looks up the names
`Core.requiredLookups` asks for → `StartRequest.encode` puts them on the wire →
Rust decides what every value means. Nothing on the Swift side interprets a
setting, which is what stops iOS and Android drifting.

An **offline** server costs no lookups at all: its UUIDs are a function of the
name and the core derives them itself. That is why the host asks
`requiredLookups` rather than resolving every name it sees — on a phone with no
signal the difference is a launch that costs nothing against one that costs a
ten-second timeout per operator.

**A settings failure never fails a launch.** A name Mojang will not resolve is
dropped and named on the console. This host's "defaults" are worse than
Android's, though, and it is worth knowing why: there, a failed write means
*vanilla's* defaults in a file we control. Here it means **Pumpkin's**, which
include `online_mode = true` — a server nobody with a cracked client can join.
So settings are applied even when every lookup failed.

### What is honoured, and what is not

| Setting | Where it lands |
|---|---|
| MOTD, max players, view and simulation distance, online mode | `networking.java` **and** `networking.bedrock` — one server, two listeners |
| PVP | `advanced.pvp.enabled` |
| Game mode, hardcore, whitelist on/off | `basic` |
| Seed | `basic.seed`, and **only when the player chose one** — `Seed::from("")` mints a fresh random seed, so assigning unconditionally gives a regenerated world a different world every launch. Read only when a level is created. |
| Ops, whitelist | Replaced wholesale in `data/ops.json` and `data/whitelist.json`, so a de-opped player actually loses it |
| Bans | **Appended** to `data/banned-players.json` — `/ban` in game writes the same file, and rewriting it would erase local bans |

Those three files are seeded in memory **and written back**, through Pumpkin's
own serde types so the shape is by construction the one its loader expects.
The write is not redundant: Pumpkin saves them only when someone runs `/op` or
`/ban` in game, and `PumpkinBackend.ops` reads `data/ops.json` to tell the
dashboard who the operators are — so seeding memory alone reported a server
with working operators as having none.
| Difficulty | **Not honoured.** `basic.default_difficulty` exists and nothing reads it; difficulty lives in `level.dat`. |
| World type, generate structures, spawn protection, spawn NPCs/animals/monsters, command blocks, cheats, allow flight | **Not honoured.** Pumpkin has no support for these at all. |
| Level name | **Deliberately not managed.** It decides which directory the world lives in, and `hasLocalWorld`, the restore selector and every existing device assume `world`. |
| `enforce-whitelist` | **Deliberately not managed.** The core has only `whitelist_enabled`, and no other host manages vanilla's enforce flag; deriving it would make iOS kick connected players where the others do not. |

Everything in that table's bottom half is reported on the console per launch —
one line naming what was ignored — because "my server ignored the difficulty I
picked" is otherwise a support conversation.

### Two behaviours worth knowing

**Online mode is now honoured**, where every iOS server previously ran
`online_mode = true` via Pumpkin's default. Flipping it changes every player's
UUID, so a world keyed by online UUIDs treats everyone as new.

**The bind address is `0.0.0.0`**, where `settings::properties` writes
`server-ip=127.0.0.1` for the other hosts — so anyone on the same Wi-Fi can
join an iOS server directly, bypassing the gateway. Pre-existing rather than
introduced with settings, and still open.

## FFI string ownership — `FFI/HomerunFFI.swift`

> **Load-bearing: every string the Rust side returns must be freed, including
> the ones from failed calls.**

Each function in the C surface returns a heap-allocated JSON document the
caller owns. `HomerunFFI.decode` is the only place that handles a reply, so
the `defer { homerun_free_string(raw) }` exists exactly once. Error paths are
the easy ones to forget — they are rare, and what they leak is the console,
which grows without bound over a long session.

A null return means the allocation itself failed, and is the one case with
nothing to free.

## The console — cursors and gaps

Logs are **polled by cursor**, not pushed:
`homerun_server_logs_since(cursor)` returns the lines after that point plus
the cursor to use next. The buffer is bounded at 2000 lines and evicts the
oldest, which matters because a backgrounded phone may not poll for minutes.

Two rules:

- **Never store a line count and use it as a cursor.** Cursors are monotonic
  sequence numbers and are never reused.
- **When the reply says `dropped`, say so.** `PumpkinBackend.drainLogs` emits
  an "… earlier output skipped …" marker. A console that quietly omits output
  sends someone hunting for a message that was never written.

Cursors do not survive a restart: the buffer clears but the sequence keeps
climbing, so a stale cursor reports `dropped` rather than silently replaying a
new run as a continuation of the old one.

## State, and what the UI is told

Internally there are five states. The contract's
`native-server-state-changed` event carries only three — `running`, `stopped`,
`crashed` — so `starting` and `stopping` are host-internal and the UI infers
them from its own pending call. `BridgeController` filters rather than
inventing a wire value.

A run that ends after a stop request is `stopped`. A run that ends on its own
is `crashed`, and carries the engine's message, which is already written for a
player.

## Metrics — `DeviceMetrics.swift` and `Core.Metrics`

The server is not a separate process, so there is no per-server figure to
report: memory is the whole app's physical footprint (the number iOS uses when
deciding what to jetsam) and CPU is the sum over the process's threads.

**This host reads counters and computes nothing.** `DeviceMetrics` returns
resident KiB and cumulative CPU seconds; `homerun-core::metrics` turns two of
those into a rate, decides how much history to keep, and decides when a number
cannot be trusted. That split is why the graph on a phone and the graph on a PC
of the same server now cover the same span — there were four answers to that
question and now there is one. See `docs/core-bridge.md`.

`DeviceMetrics.cpuSeconds` deallocates the thread array the kernel hands it.
Sampling every five seconds, leaking that would be a slow but real drain.

### What the graph covers

One point per **30 s**, 360 of them. When it fills it drops every other point
and doubles its own interval, up to 30 min — losing resolution rather than the
launch, because a session's first minutes are usually the interesting ones.
Before this it was 5 s × 720 and the launch scrolled off after an hour.

`PumpkinBackend` offers a reading every 5 s and lets the core keep what is due.
A dropped reading is still the anchor for the next rate, so the CPU number
describes the **last five seconds** before each point rather than the whole
thirty. That is a real difference from the desktop and both Android backends,
which average over the full interval, and it is the one place this migration
does not fully converge. If it ever matters, the fix is a sampler of its own on
the core's `intervalMs` — not arithmetic here.

Two consequences worth recognising rather than reporting as bugs:

- **The first point of every run has no CPU value.** A rate needs two readings,
  and inventing one would put a number on the graph that nothing measured.
- **A stopped server's graph is empty**, because `perfSamples` honours
  `serverId` like every sibling getter. It used to ignore it, so a stopped
  server kept drawing the last run's graph.

`cpuUsage` reads the graph's last point rather than sampling. It used to call
`CPUSampler`, which advanced its own baseline — so every poll of
`native-server-get-cpu-usage` stole the anchor from the next point, and the
Insights screen was changing the numbers it was reading. `CPUSampler` is gone.

`memUsedMb` is whole MiB (the core divides KiB by 1024), matching the UI's
`memUsedMb: number | null`.

### `memMaxMb` is the limit this app is killed for exceeding

Not the device's RAM, which is what it used to be. A phone with 16 GB does not
let one app have 16 GB, so "67 MB of 16,384 MB" told a player they had room
they were never going to get. It is now `os_proc_available_memory()` — what is
left before the dirty-memory limit — plus what is already used, which is the
same question Android answers with `largeMemoryClass`. The core has no opinion
on the ceiling; this is the hosts agreeing on their own.

**It is absent on the simulator.** A simulator process is a macOS process with
no jetsam limit, so the call answers zero and the UI shows the used figure
without an "of X". That is the honest rendering of "there is no cap here", and
it means **the number itself has only been verified on a simulator by its
absence** — on a device it should read the real limit, and nobody has looked
yet.

## Storage

Server files live in `Documents/servers/<id>/`, so a player can reach their
worlds through the Files app.

> **World directories are excluded from backup.** A Minecraft world is large
> and changes constantly; silently syncing one to iCloud burns the player's
> quota and their data. That is a bug, not a feature.

## How a player actually connects

Two ways, and only one of them works from outside the house.

**On the same Wi-Fi**, the host reports its LAN address and port and a friend
types it into Direct Connect. Appearing automatically in Minecraft's LAN-games
list is a different thing — it needs Pumpkin's multicast broadcast *and*
Apple's multicast entitlement, which is a request form with a real approval
delay. v1 deliberately shows the address instead.

**From anywhere else**, through the tunnel below. A phone on cellular sits
behind CGNAT, so there is no port-forwarding fallback the way there is on
desktop: without the tunnel, a server runs and nobody outside can join.

## The tunnel — `WireProxy.swift`, `go/wireproxy-ios/`

Traffic is **ingress**. The Homerun gateway is the client and the phone is the
server, which is the opposite of what "proxy" usually suggests:

```
player → <region>.gethomerun.app:<externalPort>   (link.forward_ports)
       → gateway DNAT
       → over WireGuard to <phone wg IP>:25565
       → wireproxy [TCPServerTunnel]
       → 127.0.0.1:<local Minecraft port>
```

> **No VPN permission is involved, and that is the most important property of
> this design.** wireproxy terminates WireGuard in its own userspace netstack.
> The interface address is virtual inside the process and is never registered
> with the OS — no `NEPacketTunnelProvider`, no VPN profile, no system prompt,
> and nothing for App Review to weigh. Android says the same about the same
> design. Do not "simplify" this into a Network Extension.

iOS cannot spawn the wireproxy binary that desktop and Android run, so the Go
code is bound with gomobile and runs in-process. `go/wireproxy-ios/` is a thin
wrapper — `Start`, `LastHandshakeUnix`, `Stop` — over the same
`hintjen/wireproxy-fork` Android builds from. It spawns the routines the
*config* declares rather than reimplementing forwarding, which is what keeps
the two ends of the gateway contract from drifting.

### The config is a shared contract

`WireProxy.render` produces byte-for-byte what `wireproxyConfig.ts` (desktop)
and `WireProxy.kt` (Android) produce. The same gateway is on the other end of
all three, so a difference is a bug by definition.

> **`ListenPort` is a fixed constant, not the local port.** The gateway
> unconditionally DNATs player traffic to 25565 on the WireGuard interface,
> whatever the server bound locally. Only `Target` follows the real port.
> `MTU = 1280` and `PersistentKeepalive = 30` are load-bearing for NAT
> traversal on an inbound tunnel.

iOS renders TCP only. Desktop and Android also emit UDP sections for Bedrock
(19132) and Simple Voice Chat (24454); neither can exist here, because a
Pumpkin host is vanilla-only and runs no plugin, and both are rejected before
a launch gets this far — see "What this host will not run" below.

### Credentials, and the staleness rule

The gateway provisions the WireGuard peer *asynchronously* after the server is
marked running, so at launch the credentials usually do not exist yet.
`HomerunAPI.awaitTunnel` polls `/api/server/<id>/` 20 times, 3 s apart.

The legacy provisioner mints a fresh keypair every session and nulls the config
on stop, so a polled config still equal to the pre-launch baseline is the
*dead previous set* and must be ignored. Gateway v2 reuses credentials
deliberately — **skipping the staleness check for v2 is required**, or a v2
link polls until timeout on every single start.

### `running` means reachable

The tunnel comes up *before* the backend reports `running`. Listening on
loopback is not the same as being joinable, and desktop learned this as "a
silently-rejected start masquerading as running".

**A tunnel failure stops the server.** Leaving it up would be a slower way to
fail, since nobody could join. Two kinds, both reported on
`native-server-network-error` **before** the stop — the stop itself goes
through the ordinary clean path, so without the event the UI cannot tell it
from the player pressing Stop:

| Kind | Means |
|---|---|
| `provisioning` | The gateway never handed over credentials, or the tunnel would not open |
| `handshake` | Credentials arrived but the peer never answered, or stopped answering |

A consequence worth knowing: **a start with no account now fails.** No token
means no credentials, which means no tunnel, which means no server.

### Detecting a dead tunnel

Desktop and Android count the string `"Handshake did not complete after 5
seconds"` ten times in wireproxy's stdout. In-process there is no child stdout,
and wireguard-go logs to fd 1 — which the Rust layer redirects into the
*player-visible* console. So iOS asks the device for its real
`last_handshake_time_sec` and treats 50 s without one as dead: the same
threshold, a real signal, and no tunnel noise in the console.

`get-native-local-network` reports enabled and `set-native-local-network`
accepts without doing anything, because on a phone there is no LAN toggle to
make — the device is on the player's Wi-Fi or it is not.

## What this host will not run

A linked engine cannot refuse a server it does not understand — it starts, and
what it starts is vanilla. That is the whole problem: a player who asks this
app to host their Forge pack waits through the download and the unpack, gets a
`running` server, joins it, and finds a fresh vanilla world. No error is raised
anywhere in that sequence, because from the engine's point of view nothing went
wrong.

So the refusal happens before the launch, in `native-server-start`, ahead of
even the backup-lease gate — everything after that point costs minutes.

| Server | Refused because |
|---|---|
| Bedrock (`bedrock`, `native-bedrock`) | no phone ships Bedrock Dedicated Server |
| Modded or plugin (`TYPE` is anything but vanilla) | Pumpkin's plugins are WASM/native; it loads no Bukkit plugin or Fabric mod |
| Crossplay (`native-crossplay`) | crossplay is Java plus Geyser, which is a plugin |

**The rule is in `homerun-core::minecraft::hosting`, not here.** Both apps ask
it and both show the sentence it returns, so the two cannot drift into refusing
different things or explaining the same refusal differently. `Core.hostingRefusal`
is the wrapper; nil means go ahead.

> **Crossplay is the one that catches people out.** Its `TYPE` is vanilla, so
> every loader-based check passes it. What makes it impossible is carried by
> the *game type*, not the settings — which is why this host passes `gameType`
> to the core verbatim rather than reducing it to java/bedrock first.

A core that cannot answer refuses too, matching the admission check above it:
a build mismatch is exactly when you least want to launch blind.

## Parameter shapes — `BridgeRouter+Server.swift`

> **Six metrics getters and `get-native-server-logs` take a bare string server
> id, not an object.** The desktop preload forwarded a single argument and the
> contract preserves that. Everything else takes `{ serverId: … }`. Reading
> the wrong shape yields nil and the screen renders empty, with no error
> anywhere.

## File map

| File | Role |
|---|---|
| `ios/HomerunHost/ServerBackend.swift` | The protocol the bridge talks to, and its types |
| `ios/HomerunHost/PumpkinBackend.swift` | The iOS implementation: server thread, pumps, state |
| `ios/HomerunHost/FFI/HomerunFFI.swift` | Typed FFI access; the one place strings are freed |
| `ios/HomerunHost/FFI/HomerunFFI.h` | Hand-written C declarations |
| `ios/HomerunHost/FFI/StartRequest.swift` | The start request's wire form. A leaf, so `ios/coretest` can check it against the real parser |
| `ios/HomerunHost/MojangDirectory.swift` | Name → UUID. The only outbound call here that is not to Homerun's API |
| `rust/homerun-core/src/minecraft/hosting.rs` | Which servers this host may run at all. Shared with Android, so neither app refuses alone |
| `rust/homerun-pumpkin-ffi/src/engine_settings.rs` | What a setting *means* to an engine. No Pumpkin, so it is in the fast test suite |
| `rust/homerun-pumpkin-ffi/src/pumpkin_settings.rs` | Assignment onto Pumpkin's own types |
| `ios/HomerunHost/DeviceMetrics.swift` | Process memory and CPU **counters**, from Mach. No arithmetic |
| `ios/HomerunHost/FFI/Core.swift` | The shared decisions, including `Core.Metrics` — the run's graph |
| `ios/HomerunHost/BridgeRouter+Server.swift` | The `native-server-*` channels |
| `ios/HomerunHost/WireProxy.swift` | Tunnel config, lifecycle, handshake watchdog |
| `ios/HomerunHost/HomerunAPI.swift` | Device registration; tunnel credential polling |
| `go/wireproxy-ios/` | gomobile binding over the wireproxy fork |
| `rust/homerun-pumpkin-ffi/examples/boot_engine.rs` | Boots the real engine host-native |

## Triage

**The app disappears the moment a server starts, with no crash log.** Stack
overflow on the server thread. Check `stackSize = 16 * 1024 * 1024` — this is
the classic symptom and there will be nothing else to go on.

**The console grows and the app is eventually jetsammed.** An FFI string is
not being freed somewhere — most likely on an error path. Everything should
go through `HomerunFFI.decode`.

**The console silently skips output.** The `dropped` flag is being ignored.

**The console replays an old run after a restart.** A cursor was carried
across runs and the `dropped` marker was ignored.

**`native-server-start` fails after a fixed interval.** Someone added a
timeout. There is none by design — world generation takes minutes.

**A metrics screen is blank with no error.** The handler read
`params["serverId"]` on a channel whose params are a bare string. See the
parameter-shapes note above.

**The Insights graph looks coarse, or stopped moving.** Expected: one point per
30 s, doubling to 60 s and beyond as a session runs long. The device log says
`metrics now keep one point per Ns` at each change.

**A graph reads 0% CPU rather than showing a gap.** The host sends counters and
omits what it could not read, so a zero on the graph is a measured zero. If a
missing reading is rendering as 0, suspect the `NSNull`/`NSNumber` decode in
`Core.Metrics` — `ios/coretest` has a check for exactly that.

**The server starts but no one can join.** Check the reported port against
what the engine actually bound (`homerun_server_stats()` reports it), and that
both devices are on the same Wi-Fi. iOS shows the address rather than
broadcasting to the LAN list.

**The server ignores a setting the dashboard shows.** Read the console: one
line per launch says what was applied, and another says what was ignored. If it
says the right thing and the server does not, the mapping in
`pumpkin_settings.rs` is wrong; if it says nothing was supplied, the settings
fetch failed — check the token and `HomerunAPI.serverSettings`.

**Every player is treated as new after an update.** Online mode changed. UUIDs
are keyed by it, so an offline server cannot recognise players a previously
online one knew.

**The operator list on the dashboard is empty on a server that has ops.** Two
causes, both fixed and both worth checking if it returns: `PumpkinBackend.ops`
reading the wrong path (the lists are under `<serverDir>/data/`), and
`apply_lists` seeding them in memory without writing them back. Compare the
file's mtime against the launch — if it is older, the write did not run.

**A world ends up in iCloud.** `isExcludedFromBackup` was not set on the
directory — note it must be set on the URL, and it is set at create time.

**The server starts and stops itself immediately, with a network error.** No
tunnel. If the kind is `provisioning` the gateway never issued credentials —
check the account is signed in and the server exists on the backend. If it is
`handshake`, credentials arrived but the peer never answered; check the
endpoint is reachable.

**Every start waits the full 60 s and then fails.** The staleness check is
rejecting a valid config. That is what happens when a gateway-v2 link is
treated as legacy — v2 reuses credentials, so its config *does* equal the
baseline and is supposed to.

**`cannot find type 'WireProxy' in scope`, or the framework is missing.** Run
`node scripts/build-wireproxy.js ios` before `xcodegen generate`; the project
references the staged xcframework, and new Swift files need a regenerate.

**`pkt.IsNil undefined` building the Go module.** gvisor was upgraded past the
fork's 2023 wireguard-go. The generated `go.work` pins it — and note that
`go work sync` *causes* this by writing resolved versions back into the fork's
own `go.mod`, which also dirties another repository.

**A modded server starts fine and the world is vanilla.** The refusal above did
not fire. Either the settings could not be read at all — nil settings are
deliberately not a refusal, so the launch proceeds on defaults — or the host is
passing a reduced `game_type`. Check the start returned `success: false` with a
sentence in `error`; if it returned success, the core was never asked.

**Every launch is refused with "could not work out whether this server can
start."** The core call itself threw, which means the linked library predates
`minecraft.hosting.refuse`. Rebuild the framework — `npm run rust:ios-sim` for
the simulator. This is the same class of failure as an unknown dispatch method,
and it fails closed on purpose.

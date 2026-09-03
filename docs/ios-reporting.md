# iOS reporting

## Overview

What this device tells the API about the server it runs: crashes, stats, player
presence, minigame results, and operator changes typed into the console.

**Read [`android-reporting.md`](./android-reporting.md) first.** It is the
reference, and this is the delta. Every decision — what to send, where, which
credential signs it, how often — is `homerun-core`'s and is already tested
there; both hosts call the same code through the same dispatcher, so this
document is only about the things iOS does differently and why.

There are three of them, and all three follow from two platform facts: **the
console buffer lives in Rust and outlives a run**, and **this app has no
background execution**.

A host that never reports looks fine from the inside. From everywhere else, a
crashed server has no explanation, the dashboard's graphs are empty, presence
is stale, and an `/op` typed into the console is taken back on the next launch
when `ops.json` is rewritten from the API.

## `Reporting.swift`

A `@MainActor enum` with static state, held for the life of the process the
same way `ScreenAwake` is. `AppDelegate` calls `attach(backend:)` at launch,
before the bridge exists.

The entry points and their order match `Reporting.kt` exactly:

| Called from | What |
|---|---|
| `BridgeRouter+Server.nativeServerStart` | `starting(serverId:settings:)` — arms the run |
| `HomerunAPI.awaitTunnel` | `gatewayAddressResolved(serverId:address:)` |
| `BridgeController` (`backend.onLog`) | `onLog(serverId:line:)` |
| `BridgeController` (`backend.onStateChanged`) | `onStateChanged(serverId:state:)` |
| `BridgeRouter+Server.nativeServerRcon` | `consoleCommand(serverId:command:)` |

Arming happens in the **start handler**, not on the transition to running. A
launch that dies on its way up is the one most worth explaining, and it needs a
context that already exists when it fails.

### There is no listener list to join

Android's `ServerHost` fans console lines and state changes out to any number
of listeners. iOS's `ServerBackend` offers **one closure per event**
(`onLog`, `onStateChanged`, `onPlayersChanged`, `onNetworkError`), and
`BridgeController` already owns all four.

So reporting is fed by forwarding from that one owner rather than by
subscribing. Everything involved is main-actor, so the call is free, and the
page is told first because the page is what the player is looking at. A
multiplexer would be a new abstraction for a second subscriber.

> If a third subscriber ever appears, that is the moment to build the fan-out —
> not before.

### The console tail that is not here

Android keeps its own 2000-line tail, because its backend stops answering
`logs()` the moment a run ends: the exit path clears the current server id
immediately after announcing state, so a `CRASHED` listener that then asks for
the console gets nothing.

**This host has no such problem, so it keeps no tail.** `PumpkinBackend.logs()`
is an unguarded passthrough to the engine's buffer; `finish()` drains once more
on the way out to catch the dying words; and the buffer is cleared by
`beginConsole()` at the top of the *next* launch. The crash path therefore reads
`backend.logs(serverId:since: 0)` and gets the same 2000 bounded lines a
duplicate tail would have held.

### What travels with a crash — the same two fields

The crash report is the console and a host context, exactly as on Android
(`android-reporting.md` § *What travels with a crash*). The delta is where the
log comes from: the FFI reads Android's logcat itself, and everywhere else it
asks the provider the host registered — here, the `OSLogStore` reader
`DeviceWebsocket.swift` installs for `get-app-logs`. A device that never
registered one sends the section as *This device has no logs to send.*, which
is what the reader should see rather than an empty log that reads as "nothing
happened".

`hostContext()` here names the hardware from `utsname` rather than
`UIDevice.model`, which only says "iPhone", and adds nothing UIKit. The armed
server id (`armedId`, set in `starting` and cleared with the cadence loop) is
also what `AppErrors.context()` reports as `serverId`, so an app error during
a launch and the crash report that follows can be joined on the API.

**Uncompiled**, like the rest of this file's Swift; the wrappers are on the
`coretest` target's list.

### The cadence is a timer, and suspension is the loop's teardown

`reporting.stats.schedule` owns every number — the 120 s interval, the 1 s
presence debounce, and the rule that a presence report resets the periodic
clock. The host holds the opaque state and supplies the clock, which is the
same arrangement `Core.Metrics` uses for the graph.

It is driven by a **single-shot `Timer`, rescheduled after each decision**,
like every other clock in this host (the instance heartbeat, the log pump, the
sampler, the tunnel watchdog). A presence nudge invalidates it and re-asks
immediately.

`plans/ios-reporting.md` originally called for tearing the loop down on suspend
and rebuilding it on resume, in the same place `native-server-state-changed` is
re-emitted. **Neither of those places exists**: `AppDelegate` implements two
methods, there are no scene hooks and no lifecycle observers anywhere in this
app. Nothing needed building, because a timer gives the required behaviour for
free:

- a suspended process's timers do not fire, so a backgrounded app reports
  nothing — which is correct, because the server has stopped too;
- the schedule is wall-clock, so the first tick after the app comes back is
  already overdue and reports at once.

That is exactly the plan's "wall-clock inside a session", with no machinery.

### The public IP is cached here, not in `HomerunAPI`

Android caches it in a `@Volatile` field on its API object. `HomerunAPI` here
is a nonisolated enum with no mutable state anywhere and no actors in the
codebase to borrow a pattern from, so the cache lives on the one caller, which
is already main-actor. `HomerunAPI.fetchPublicIPAddress()` is stateless.

## The rule this subsystem was built by

> **A core parser written against vanilla's console is suspect on Pumpkin, and
> it will not tell you it is wrong.**

Three were, and each one failed the same way: the number that comes from the
*engine* stayed correct, so nothing looked broken, and the thing that depended
on the *console* quietly stopped.

| What broke | Symptom | Why nobody noticed |
|---|---|---|
| `time query gametime` reply | `server_age` null on every report, 3 s timeout each time, and the reply leaked into the player's console | one null field among eight |
| `list uuids` reply | `players` empty on every report | the count beside it was right, because it came from the engine |
| join and leave lines | no presence report; a join reached the API up to 120 s late, and a shorter session never at all | the player count was still correct on the next beat |

So `Reporting.onLog` carries a **contradiction check**: a line that plainly says
`joined the game` while the core reports no join is logged verbatim as
`a presence line the core did not recognise: …`. It costs one string
comparison on lines already flowing through, and it is how the third of these
was found after two attempts at reconstructing the format from the engine's
source were both wrong.

Read the stream you actually consume. Pumpkin has **two** formatters, and the
one in `logs/latest.log` is not the one a linked host captures:

```text
[INFO] Kologgs joined the game                     # logs/latest.log
2026-08-13 10:36:00  INFO tokio-rt-worker ThreadId(120) pumpkin::world: Kologgs joined the game   # fd 1, what push_log sees
```

## What the poll costs on this platform, and what it did

`server.statsPoll` sends `list uuids` and `time query gametime` and recognises
each reply **by the core parsing it** — which is also what withholds it from
the player's console. A reply the core cannot read is therefore a reply the
player sees, every two minutes, plus a three-second timeout.

Pumpkin failed both, for one underlying reason: its console renderer resolves a
message through the **Bedrock** translation key in preference to the Java one,
and its translation lookup lowercases a key against a table that did not.

| Command | Pumpkin printed | Consequence |
|---|---|---|
| `time query gametime` | `Gametime is 12345` | age always null, 3 s timeout per report, and the line leaked into the console |
| `list uuids` | `There are 2 of a max of 20 players online: minecraft:commands.list.nameandid, …` | header parsed, so the **count was right and the roster was always empty** |

The second is the dangerous one: a plausible report, no error, no timeout,
nothing to notice.

Both were fixed in the core rather than here, because both hosts link it:

- `reporting::stats::parse_server_age` learned `Gametime is `, which fixes the
  age *and* stops the leak, since recognising the reply is what withholds it.
- `Engine::roster_is_authoritative` lets a linked engine hand over the roster
  it already holds, so this host never asks the console for it. See
  [`android-reporting.md`](./android-reporting.md#the-roster-does-not-come-from-the-console-when-it-does-not-have-to).

### Presence, and where the prefix rule ended up

`game.classify` reads a join out of the line the server prints, and its rule
was "the name is what follows the first `]: `" — vanilla's prefix. Pumpkin
writes no `]: ` at all, so `joined` and `left` answered `None` for every line
and the presence nudge never fired.

`console::after_log_prefix` now consumes either shape, and the reasoning is
worth keeping because the obvious fixes are both wrong:

- Consuming *any* bracketed run reads a chat author as a prefix, and then
  `[Griefer] Notch joined the game` typed in chat forges a join. A tag counts
  only if it announces a level or a time.
- Enumerating what sits between the level and the message does not survive
  configuration — the thread name and `ThreadId(120)` are there only because
  `logging.threads` is on, and the target only because `with_target(true)` is.
  What holds regardless is that the **first `": "` ends the prefix**: a
  timestamp's colons are followed by digits and a target's `::` by a letter.
  That is the same property vanilla's first-`]: ` rule leans on.

A chat author's `<…>` before the separator means the `": "` was typed rather
than logged, which is what keeps `hey: Notch joined the game` from forging a
join on a build with no target in its format.

> **The fork still prints those strings**, which is what a player typing
> `/list` sees today. Fixing `to_pretty_console` and `get_translation` in
> `hintjen/Pumpkin-homerun` is a follow-up; it needs a pin bump and a full static-lib
> rebuild, and reporting no longer depends on it.

## `loader` is always `vanilla`

`stats::pinned` prefixes a command with `minecraft:` for Paper, to get past a
Bukkit plugin that has shadowed it. Pumpkin registers bare command names and
rejects the namespaced form, so passing `paper` here would fail both commands
outright. This host is Pumpkin-only, so the value is a constant.

## Crash diagnosis mostly answers nil, and that is correct

`reporting::crash`'s patterns are JVM strings — `Invalid or corrupt jarfile`,
`FAILED TO BIND TO PORT`, `Could not reserve enough space for object heap`. A
Pumpkin crash produces none of them, so `diagnose` returns nil and the player
gets the API's own message rather than a wrong local one.

The consequence worth knowing: **iOS gets less local explanation than Android
does**, and adding Pumpkin's own failure signatures to the core is worth a
follow-up.

## File map

| File | Role |
|---|---|
| `ios/HomerunHost/Reporting.swift` | The listener, the cadence timer, the ops chain |
| `ios/HomerunHost/FFI/Core.swift` (§Reporting) | Wrappers over the core's decisions, including `Core.Request` and its `auth` |
| `ios/HomerunHost/HomerunAPI.swift` | `perform`, `serverBody`, `fetchPublicIPAddress`, and the PATCH verb |
| `ios/HomerunHost/PumpkinBackend.swift` | `note(serverId:line:)` — the host's own lines, badged once |
| `ios/HomerunHost/AppErrors.swift` | `context()` — shared between app errors and the crash report's host context |
| `ios/coretest/main.swift` | Every wrapper above, against the real core, with no simulator |
| `rust/homerun-core/src/reporting/` | The payloads, the cadence, the parsers |
| `rust/homerun-pumpkin-ffi/src/host_dispatch.rs` | The poll and the ping — the two calls with effects |

## Triage

**Nothing is reported at all.** Look for `reporting armed for <id>` at start:

```bash
xcrun simctl spawn booted log stream --predicate 'subsystem == "app.gethomerun.ios"'
```

Absent means the start handler never called `starting(...)`. Present, with no
`reported …` line 120 s later, means the run never reached `running` — or the
forwarding in `BridgeController` was removed, which nothing else would catch.

**`print` and `NSLog` are invisible here and worse than useless.** Once a
server starts, the Rust layer replaces fds 1 and 2 with the pipe that feeds the
*player's* console. Use `HostLog.reporting`.

**Every field is `?`.** Read the report line — it names each one:

| Field | `?` means |
|---|---|
| `players` | the engine reported no roster, which on this host means nothing is running — it does not go through the console |
| `age` | the `time query gametime` reply was not understood; check `parse_server_age` against what the console actually printed |
| `cpu` | expected on the **first** report of a run: a rate needs two readings |
| `ping` | the gateway has not assigned an external port yet, or it is unreachable from this device |

**A join is reported late, or `(periodic)` where `(presence)` was expected.**
The console line was not recognised — look for
`a presence line the core did not recognise:` in the log, which prints it
verbatim, and match that string in `console::after_log_prefix` rather than
inferring the format from the engine's source. The player *count* will still
be right on the next beat, because it comes from the engine, so nothing else
will look wrong.

**`ping` is null for a whole session.** Confirm the address the host logged is
reachable at all — `nc -z <host> <port>`. It is the gateway's public address
with the *external* port from `forward_ports`, and a name fronted by a CDN
answers 443 while refusing that one. The measurement failing is expected to
produce a hole rather than an error, so nothing else reports it.

**An operator change does not stick.** It needs a signed-in *user*, not the
device token — the API answers a device-signed settings change with 200 and
strips it. `nobody is signed in` in the log is the benign version. Every other
branch of `syncOps` logs what it decided, because the two most likely failures
are both silent by nature.

**`[Homerun] [Homerun] …` in the console.** Something double-badged.
`PumpkinBackend.note(serverId:line:)` adds the prefix only when it is missing,
because the core writes it into some lines already — `minecraft::ops` does, and
`reporting::crash` deliberately does not.

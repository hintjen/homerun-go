# Android reporting

## Overview

What this device tells the API about the server it is running: crashes, stats,
player presence, minigame results, and operator changes typed into the console.

None of it is needed to *run* a server, which is why it was missing for so
long — a host that never reports looks fine from the inside. What it cost was
specific:

- a crashed server gave the player no explanation and support no logs
- the dashboard's Insights graphs stayed empty for anything hosted on a phone
- a session shorter than the reporting interval was invisible to journeys,
  because no sample ever caught anybody online
- an operator granted with `/op` in the console was silently revoked by the
  next launch, which rewrites `ops.json` from the API

The desktop has done all of this for years (`nativeServerManager.ts`). This is
the port, with the decisions moved into `homerun-core` so there is one
implementation rather than three.

## The split

**Every decision is in the core. Every effect is here.**

The core answers with a `reporting::Request` — method, path, body, and *which
credential signs it*. This host performs it and nothing else: it picks no
path, builds no body, and never has to work out whether something is signed by
the device or by the person at the keyboard.

```text
  host: a line, a crash, a timer   →  homerun_core::reporting
  core:                            →  Request { method, path, body, auth }
  host: sign it, send it, forget it
```

That last part is a rule, not a style. Nothing here retries and nothing fails
loudly: a report that does not arrive is a gap in a graph, and a report that
interrupts hosting is a session lost.

### Which credential, and why it matters

| Report | Signed by |
|---|---|
| crash, stats, minigame | the **device** token |
| ops/ban sync | the **user** token |

Reporting is a fact about a machine. An ops change is an act by a person, and
the API judges it against them — a member's settings PATCH is stripped
server-side, matching their inability to change ops in the UI. Signing that
with the device token produces a silent success: HTTP 200, nothing changed.
So `Reporting.syncOps` deliberately **does not fall back** to the device token
when nobody is signed in; it logs and gives up.

### What kind of device this is

The device token above is issued by `POST /api/init/native/`, and that same
call is where this device says what it *is*. `DeviceRegistry` sends
`device_type` alongside the name:

| Host | Sends | API enum |
|---|---|---|
| Android | `mobile/android` | `DeviceType.MOBILE_ANDROID` |
| iOS | `mobile/ios` | `DeviceType.MOBILE_IOS` |
| Desktop, native path | `native_java` (or omits the field) | `DeviceType.NATIVE` |

The endpoint is still named `native` because that is what it means to the API:
*hosts a server as a plain process rather than a Docker Compose stack*. Phones
answer that question exactly the way desktop's native path does, and every
API-side behaviour keyed on it — no compose file with a state report, no ack of
a `stopping` transition, process output instead of container logs — applies
unchanged. Only the identity is new. On the API side that set is
`DeviceType.hosts_natively()`; test against it rather than `== NATIVE`.

Two things about this that are easy to get wrong:

- **The slash is load-bearing and is not a path.** The API matches the string
  exactly against its enum, and slash-namespaced type values are the house
  convention there (`game_type` has `minecraft/native`). `mobile-android`
  is a 400.
- **The field is a claim, and the API only half-trusts it.** A client may
  declare `native_java` or either mobile type; `wsl` and `both` are refused,
  because those are earned by completing a WSL install and the API awards
  them itself. Sending one gets a 400 rather than a silent downgrade.

Before this existed both phones registered as `native_java`, indistinguishable
from a desktop — which is why nothing could count phones. Devices that
registered then re-type themselves on their next `ensure()` call; there is no
backfill.

## `Reporting.kt`

Process-scoped, like `ServerHost` and for the same reason: a page reload must
not silence reporting for a server that is still running, and two activities
must not report the same server twice.

### The console tail

Kept here rather than read back on demand. A backend answers `logs()` only
while it owns a run — the exit path nulls its current server id immediately
after announcing the state — so a crash listener that reacts to `CRASHED` and
*then* asks for the console gets an empty list. 2000 lines, matching the
desktop's `SESSION_LOG_LIMIT`; the core caps what it actually sends, by lines
**and** by bytes.

### The cadence

`homerun_core::reporting::stats::Schedule` decides when. This file holds the
opaque state between polls and supplies the clock, the same arrangement as
`metrics::History`.

- every 120 s while running
- immediately when a run reaches running
- 1 s after a join or a leave, coalesced, and that resets the periodic clock

The loop sleeps on a `select` between the core's `waitMs` and a conflated
`nudge` channel, so a join wakes it early without a busy poll. Conflated
because a party arriving together should be one report, which is the same
coalescing the core's debounce expresses in time.

### Asking the server, without the player seeing it

`native-server-rcon` is fire-and-forget on this host — a command's reply
arrives as an ordinary console line, not as a response — so `list uuids` and
`time query gametime` are answered by watching the console. That watching used
to happen here in Kotlin; it happens in the supervisor now, for the reason
below.

### The poll is invisible to the player, and where that is enforced

Because the reply is an ordinary console line, a naive implementation shows
**every player two lines they did not type, every two minutes**:

```
[20:23:05] [Server thread/INFO]: There are 0 of a max of 20 players online:
[20:23:05] [Server thread/INFO]: The time is 32082
```

The desktop has no such problem — its replies come back over RCON, a channel
the console never sees.

**Filtering in the host does not work, and was tried.** The UI does not build
the console from the host's event stream; it pages it out of the supervisor's
own `LogBuffer` in Rust. A line skipped in Kotlin is still in the buffer when
the UI asks for it.

So the decision lives in the supervisor, at `push_log` — the single funnel
every console line goes through. `server::Ask` arms an expectation, and a line
that answers it is handed to the caller and **not** appended:

```rust
if let Some((ask, sender)) = inner.quiet.as_ref() {
    if ask.recognises(&line) { sender.try_send(line); return; }
}
inner.logs.push(line);
```

`recognises` is the core's own parser returning `Some` — so the shape of a
`list uuids` reply is written down once, and a reply this build cannot
understand stays on the console where somebody may make sense of it.

The host therefore makes **one** call, `server.statsPoll`, which sends both
commands and returns both answers. It blocks while the server replies, so it is
called off the main thread.

The cost: a player who runs `/list` in the same moment loses their own reply to
the poll. That is the trade for not showing everybody two machine-generated
lines every two minutes.

### The roster does not come from the console when it does not have to

All of the above is what a **child process** requires: a JVM behind a pipe can
be asked things only by typing at it. A *linked* engine is not that — it holds
the player list in memory — so `Engine::roster_is_authoritative` says which
kind this is, and `stats_poll` asks the engine directly when it can:

| Engine | Roster from | Why |
|---|---|---|
| `ProcessEngine` (the JVM) | `list uuids`, parsed | its own roster is names tracked from console lines, with no UUIDs, and a UUID is what makes a player a player downstream |
| `PumpkinEngine` (linked) | `Engine::players()` | exact, instant, no timeout, and nothing can shadow or reformat it |

This is not only an optimisation, and Android's JVM path is unaffected by it —
but Android's Pumpkin backend inherits the correctness. Pumpkin's console
renders each player through a translation key its own table cannot find, so the
reply names every player as the literal string
`minecraft:commands.list.nameandid`. The header still parses, so the count was
right, the line was still withheld as a recognised reply, and `players` was
empty on every report. Nothing about that failure was visible from either end,
which is the argument for not going through a console at all when the answer is
already in hand.

`time query gametime` has the same cause and no such escape — the world's age
is not on the `Engine` trait — so it stays a console round trip, and the core
learned Pumpkin's wording for the reply instead (`Gametime is 12345`, from the
Bedrock translation key). See `parse_server_age`.

### CPU is rescaled, and this is the easiest thing to get wrong

`backend.cpuUsage()` is percent **of one core** and legitimately exceeds 100.
`cpu_usage` on the endpoint is percent **of the machine**. The two are
identical on a single-core reading, so a host that forgets the conversion looks
correct in every test and reports a phone on fire in the field. Hence
`Core.cpuPercentOfDevice`, and hence this paragraph.

### The gateway address arrives late

`gateway_ping` measures the round trip to *where a player connects* — the
gateway's hostname and the external port it assigned — not to the WireGuard
endpoint, which would answer a different question.

That port does not exist when a launch begins. It is assigned while the
post-launch tunnel poll is waiting for it, so `HomerunApi.readLink` extracts it
there and hands it to `Reporting.gatewayAddressResolved`. Until then
`gateway_ping` is null, which is what the desktop sends in the same window.

## Ops sync happens on both consoles

An `op` typed in the app goes through `native-server-rcon` →
`Reporting.consoleCommand`. An `op` typed in the **web dashboard** never
touches Kotlin — it arrives as a `rcon` frame on the device websocket and is
executed inside Rust. Both mirror the change into the server's settings, and
both sign it as the person who typed it:

| Console | Path | Credential |
|---|---|---|
| the app's | `Reporting.syncOps` | `Session.userToken()` |
| the dashboard's | `device_ws::Connection::sync_ops` | the JWT that authenticated the socket |

The device-websocket half is the closer match to the desktop, which passes the
device-WS caller's token through for exactly this reason. Neither falls back to
the device token: the API would accept it and strip the change, which reads as
success.

## `Session.kt`

The signed-in user's token, read from the same preferences the bridge writes at
login. It exists because ops sync needs to act *as the user* from a console
watcher rather than from a bridge handler, which is where the token used to
live.

**The token must never reach a server process.** `ServerConfig.extra` is
forwarded into the child's environment; anything from this file that ended up
there would hand a Minecraft server, and every plugin in it, the user's
account.

## `net.gatewayPing` — the one call with a socket

Server List Ping is a small protocol that is easy to implement subtly wrong,
and the desktop did, twice: it cannot tell a malformed varint from an
incomplete one (so a peer that is not speaking SLP burns the whole timeout),
and its varint guard admits a sixth byte whose shift JavaScript masks to 3,
corrupting the value.

So the codec lives in `homerun_core::minecraft::slp` as a fed-bytes state
machine, and the socket around it lives in the FFI crate's `host_dispatch`,
reached through the same `nativeCall` every other core call uses. **No new
export, so no ABI change**, and iOS gets the whole thing without writing a
second socket loop in Swift.

`host_dispatch` enforces a real **deadline** rather than the desktop's idle
timeout — with an idle timeout, a peer dribbling one byte a second keeps the
measurement alive indefinitely, which on a phone is a wake lock held by a
stranger.

It has a neighbour now. `net.regionLatency` measures a *region's* gateway
before any server exists there, so it cannot speak SLP — there is nothing to
answer — and times a bare TCP handshake instead. It moved here from the two
hosts for the reason above, having been written twice and been wrong both
times: see [`region-latency.md`](./region-latency.md).

## Console forgery, which was real

Two lines of defence in the core, both added because the desktop's equivalents
can be forged by any player with a chat box:

1. **Fake joins.** `console::player_before` used to read the name after the
   *last* `]: ` in a line. Both characters are typeable, so
   `<Griefer> hey]: Notch joined the game` was read as a real join and entered
   the roster, the player-count graph and the presence-triggered report. It now
   reads from the first `]: `.
2. **Fake matches.** `[HOMERUN:STATS] ` was matched anywhere in a line, so a
   player could type a payload into chat and have it ingested as a finished
   match with whatever scores they liked. The core now requires it to sit
   directly after the log prefix, with only bracketed logger tags in between.

The desktop is still exposed to both. See `plans/android-parity.md`.

## File map

| File | Role |
|---|---|
| `Reporting.kt` | the listener, the cadence loop, the ops chain |
| `DeviceRegistry.kt` | registration, the device id/token, `device_type`, the heartbeat |
| `Session.kt` | the user token, read in one place |
| `Core.kt` (§Reporting) | wrappers over the core's decisions |
| `HomerunApi.kt` | `perform`, `serverBody`, `publicIpAddress`, PATCH |
| `homerun-core/src/reporting/` | crash, stats, minigame — payloads and cadence |
| `homerun-core/src/minecraft/ops.rs` | op/deop/ban/pardon, the list merge |
| `homerun-core/src/minecraft/slp.rs` | the ping codec |
| `homerun-pumpkin-ffi/src/host_dispatch.rs` | the ping socket, the deadline, `server.statsPoll` |
| `homerun-pumpkin-ffi/src/server.rs` | `Ask` — keeping a poll's answer off the console |
| `homerun-pumpkin-ffi/src/device_ws/mod.rs` | ops sync for the dashboard's console |

## Triage

**Nothing is reported at all.** Look for `reporting armed` at launch. If it is
missing, the bridge's start path did not call `Reporting.starting` — reporting
is armed there rather than on the running transition so that a crash *during*
startup still has a context.

**Reports go out but every field is `?`.** The log line names what it sent
(`players=… age=… cpu=… ping=…`) precisely so that this is visible; a line that
only said "reported" would make an all-null report look healthy. Then:

| Field null | Likely cause |
|---|---|
| `players`, `age` | the poll timed out (3 s), or a plugin shadowed `/list` — the core's `pinned` exists for this |
| `cpu` | expected on the **first** report of a run: a rate needs two readings |
| `ping` | the gateway has not assigned an external port yet, or the server is not reachable from the device |

**An ops change does not stick.** It needs a signed-in user. Check for
`nobody is signed in` in logcat. If the PATCH itself failed, `perform` logs the
method, the path and the API's own error text.

**A crash produced no report.** The tail is empty only if the run printed
nothing, which is itself worth investigating — look for
`crashed with an empty console`.

**The phone shows up as a desktop, or as "Native + WSL".** It registered before
`device_type` was sent, or it re-registered against an API that had not yet
shipped the mobile types. Registration is cached in SharedPreferences and
`ensure()` returns early when it is present, so nothing re-sends on its own —
clear the app's storage, or sign out and back in, to force one call.

**Registration is refused with a 400 naming `device_type`.** The string did not
match the API's enum. It is `mobile/android`, with a slash; check
`HomerunApi.DEVICE_TYPE` against `DeviceType` in `fractal_database/models.py`,
and confirm the API is new enough to know the value at all.

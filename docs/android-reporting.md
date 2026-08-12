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

### Scraping the console for RCON replies

`native-server-rcon` is fire-and-forget on this host — replies arrive as
ordinary console lines, not as a response — so `list uuids` and
`time query gametime` are answered by watching the console.

**The reply is recognised by the core parsing it**, not by matching text here.
`Core.parseRoster` returning non-null *is* the test. That is the same trick the
desktop uses for Bedrock, which has no RCON at all, and it means the shape of a
`list uuids` reply is written down once.

### Known issue: the poll is visible to the player

Because the reply is an ordinary console line, **every player watching their
own console sees two lines they did not type, every two minutes**:

```
[20:23:05] [Server thread/INFO]: There are 0 of a max of 20 players online:
[20:23:05] [Server thread/INFO]: The time is 32082
```

The desktop does not have this problem: its replies come back over RCON, a
separate channel the console never sees.

Filtering the host's event fan-out **does not fix it** — that was tried. The UI
does not build the console from these events; it pages it out of the
supervisor's own `LogBuffer` in Rust, so a line skipped in Kotlin is still
there when the UI asks. A real fix has to keep the line out of that buffer,
which means the supervisor has to know a command was issued on reporting's
behalf. Two candidates:

1. Mark the command as internal when it is dispatched, and have the supervisor
   withhold the next line that answers it from the buffer.
2. Stop polling: take the count from the console-built roster the supervisor
   already maintains, and lose the UUIDs — which journeys need to attribute a
   session to a player, so this is a real loss rather than a free win.

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
| `Reporting.kt` | the listener, the cadence loop, the scrapes, the ops chain |
| `Session.kt` | the user token, read in one place |
| `Core.kt` (§Reporting) | wrappers over the core's decisions |
| `HomerunApi.kt` | `perform`, `serverBody`, `publicIpAddress`, PATCH |
| `homerun-core/src/reporting/` | crash, stats, minigame — payloads and cadence |
| `homerun-core/src/minecraft/ops.rs` | op/deop/ban/pardon, the list merge |
| `homerun-core/src/minecraft/slp.rs` | the ping codec |
| `homerun-pumpkin-ffi/src/host_dispatch.rs` | the socket, and the deadline |

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
| `players`, `age` | the console scrape timed out (3 s), or a plugin shadowed `/list` — the core's `pinned` exists for this |
| `cpu` | expected on the **first** report of a run: a rate needs two readings |
| `ping` | the gateway has not assigned an external port yet, or the server is not reachable from the device |

**An ops change does not stick.** It needs a signed-in user. Check for
`nobody is signed in` in logcat. If the PATCH itself failed, `perform` logs the
method, the path and the API's own error text.

**A crash produced no report.** The tail is empty only if the run printed
nothing, which is itself worth investigating — look for
`crashed with an empty console`.

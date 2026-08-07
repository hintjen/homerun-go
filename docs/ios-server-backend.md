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

## Metrics — `DeviceMetrics.swift`

The server is not a separate process, so there is no per-server figure to
report: memory is the whole app's physical footprint (the number iOS uses when
deciding what to jetsam) and CPU is the sum over the process's threads.

CPU is a *rate*, so it only exists between two samples — `CPUSampler` reports
nothing on the first call rather than inventing a number. It can legitimately
exceed 100%: several cores, and the server uses them.

`DeviceMetrics.cpuSeconds` deallocates the thread array the kernel hands it.
Sampling every five seconds, leaking that would be a slow but real drain.

## Storage

Server files live in `Documents/servers/<id>/`, so a player can reach their
worlds through the Files app.

> **World directories are excluded from backup.** A Minecraft world is large
> and changes constantly; silently syncing one to iCloud burns the player's
> quota and their data. That is a bug, not a feature.

## How a player actually connects

The host reports its LAN address and port, and a friend on the same Wi-Fi
types it into Direct Connect. **Without that, nothing else here matters** — a
server nobody can join is not a feature.

Appearing automatically in Minecraft's LAN-games list is a different thing: it
needs Pumpkin's multicast broadcast *and* Apple's multicast entitlement, which
is a request form with a real approval delay. v1 deliberately shows the
address instead.

`get-native-local-network` reports enabled and `set-native-local-network`
accepts without doing anything, because on a phone there is no LAN toggle to
make — the device is on the player's Wi-Fi or it is not.

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
| `ios/HomerunHost/DeviceMetrics.swift` | Process memory and CPU sampling |
| `ios/HomerunHost/BridgeRouter+Server.swift` | The `native-server-*` channels |
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

**The server starts but no one can join.** Check the reported port against
what the engine actually bound (`homerun_server_stats()` reports it), and that
both devices are on the same Wi-Fi. iOS shows the address rather than
broadcasting to the LAN list.

**A world ends up in iCloud.** `isExcludedFromBackup` was not set on the
directory — note it must be set on the URL, and it is set at create time.

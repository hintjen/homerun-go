# Region latency

## Overview

One bridge channel — `measure-region-latency` — answers "how far is this
player from that gateway?". The UI ranks regions by the answer and launches a
server in the closest one, so a wrong number does not throw an error; it
quietly puts a player on the wrong continent.

It is one line of contract, and it was wrong on **both** mobile hosts from the
day each was written, in two different ways, for one reason: the argument is a
**hostname**, and both hosts treated it as a **URL**.

```
UI      lib/regionUtils.ts        measureLatency(r.domain)
        ↓  invoke("measure-region-latency", "us-east.gethomerun.app")
Host    one thin wrapper — no parsing, no socket
        ↓  net.regionLatency
Rust    homerun_core::region      split the address        (pure, tested)
        host_dispatch             resolve, connect, time   (the only socket)
        ↓
UI      bestRegionFromLatencies() → the region to launch in
```

## What the argument is

`domain` comes straight from `GET /api/server/regions/`, which the API builds
from `GatewayHostConfig.public_addr`:

> the gateway's public address — the SRV target players' clients resolve
> (e.g. `us-east.gethomerun.app`)

A bare hostname. No scheme, no path, and normally no port — the port lives in
the SRV record, not in this string.

**Nothing in the contract says so.** `shared/conformance/bridge-v1.json`
declares the channel and nothing about its payload, and the UI types it as
`{ params: string; result: number }`. `string` is exactly as much as a host is
told, which is why both hosts guessed, and why both guessed "URL".

## The bug this file exists to prevent

Android ran `URL(domain)`. `java.net.URL` requires a protocol, so every region
threw `MalformedURLException: no protocol`, was swallowed by a `runCatching`,
and returned the unreachable sentinel. **No packet was ever sent** — the
failure was at parse time, in microseconds.

iOS ran `URL(string: domain)`, which *succeeds*: a bare hostname is a valid
relative reference. It produced a URL with a nil scheme, sailed past the guard,
and failed one step later inside `URLSession` with `unsupportedURL`.

Same outcome on both: **every region reported 9999**, always, on every device,
with no error anywhere. The picker ranked a list of ties and took the first, so
players were placed in whichever region the API happened to list first.

The symptom is invisible from the inside. A region is always chosen, a server
always launches, and nothing is logged — the only tell is the UI's
`[Regions] Latency results:` line showing every region identical.

## Why none of this is in the hosts any more

Two platforms answering one question differently, and both answering it wrong,
is the case `homerun-core` exists for. The parsing is now
[`homerun_core::region`], with the post-mortem in its module docs and the rules
pinned by tests.

The socket moved too, and for the reason `host_dispatch`'s own header already
gave about `net.gatewayPing`: writing it once per platform means the
interesting half is shared and the half that can hang is not. `net.regionLatency`
sits beside it — same entry point, **no new export, so no ABI change**.

What is left in each host is a wrapper that passes a string down and turns
`null` into the sentinel. That is the whole of it, on both platforms.

## The measurement — `host_dispatch::connect_latency`

A bare TCP connect, timed. Not an HTTP request, and not Server List Ping:
`net.gatewayPing` needs something willing to answer as a Minecraft server,
whereas a region probe runs *before* any server exists there, so the handshake
is the only thing available to measure.

A **refusal is a measurement**, not a failure: the SYN reached the gateway and
a reset came back, which is the round trip being timed. Only two things count
as unreachable:

- the connect timed out — the SYN was dropped, nothing came back
- the name would not resolve

Resolution happens **before the clock starts**. The desktop folds it into its
figure, but a cold lookup is tens of milliseconds that vary per hostname, and
these numbers exist only to be ranked against each other. It still counts
against the deadline, because a name that takes five seconds to resolve is not
a region worth offering.

### The probe port has to be one the gateway serves

It is tempting to conclude from the refusal rule that the port does not matter
— that an unserved port still resets, so any port measures the same distance.
**On the internet that is false**, and it is the trap to avoid here.

A closed port on a public host is almost always firewalled, and a firewall
*drops* the SYN rather than rejecting it. Measured from a dev machine, port 81:

| Host | Result |
|---|---|
| `google.com` | timed out at 5s |
| `example.com` | timed out at 5s |
| `github.com` | timed out at 5s |
| `api.gethomerun.app` | timed out at 5s |

None refused. A closed port on *loopback* does refuse, which is exactly how the
assumption survives a local test and fails in the field.

So a probe aimed at a port the gateway does not serve does not read as slow —
it reads as unreachable, for every region, which is the **same symptom as the
bug this replaced**. The desktop's own comment on this ("Error means we got a
response") is true only for the reject case and is not justification for the
port choice.

`region::DEFAULT_PROBE_PORT` is **80** on all three hosts. The desktop chose it
first; the other two match so the numbers can be compared. Changing it is a
three-host decision.

### The refusal branch does nothing on Windows

`TcpStream::connect_timeout` on Windows reports even a refused *loopback*
connection as `ErrorKind::TimedOut`, with `raw_os_error` unset — Rust's own
deadline firing rather than the OS answering. Linux and Darwin, the two
platforms that ship, surface the refusal properly.

That is why `is_measurable` is a separate function with its own test: the rule
is checked on every platform, and the socket-level test for it is
`#[cfg(not(windows))]`. A dev machine cannot exercise that path through a real
connect, and a test that silently does nothing is worse than one that is
honestly skipped.

## The sentinel, and the UI check that never fires

`9999` is the unreachable value. It stays in the **hosts**, not the core: the
core answers `null` for "could not measure", and what the UI must receive
instead is a property of this bridge, not of the measurement.

It is a number rather than a thrown error because the UI ranks regions by it,
and a throw would cost the whole list to one bad host.

But the UI's own "nothing answered at all" test is:

```ts
return best[1] === Infinity ? null : best[0];
```

and `measureLatency` only produces `Infinity` when the invoke *throws*. A host
returning `9999` is a successful invoke, so **on mobile that branch is dead**:
the UI always picks a region, even when every one is unreachable, and will
render "9999 ms" to the player.

A host cannot fix this by sending `Infinity` — `JSON.stringify(Infinity)` is
`null`, so the value cannot cross the bridge. The fix belongs in
`homerun-app-ui`: treat `>= 9999` as unreachable.

## What the address actually resolves to

A region's `domain` is a **per-gateway domain** — `Domain.uri`, the SRV target
players resolve. In production those are names like `minecraft.gethomerun.app`
and `redstone.gethomerun.app`, one per gateway host, each an explicit
**DNS-only (grey cloud)** A record pointing at that gateway's VM.

They are grey-clouded deliberately, and the API docs say why:

> SRV *targets* (`link.domain.uri`, e.g. `minecraft.gethomerun.app`) must keep
> explicit **DNS-only (grey cloud)** records. A regional host without its own
> record silently falls into the proxied apex wildcard, and proxying breaks
> raw TCP.
>
> — `homerun/api/docs/server-share-page.md`

Verified, and the probe works:

| Region domain | Resolves to | Port 80 | min | avg |
|---|---|---|---|---|
| `minecraft.gethomerun.app` | `178.156.167.38` | open | 36.8 ms | **39.8 ms** |
| `redstone.gethomerun.app` | `46.224.146.18` | open | 143.0 ms | **144.5 ms** |

Eight samples each. A clean ~105 ms separation that is stable across runs —
exactly the signal the picker needs. **Port 80 is served by both gateways**, so
`DEFAULT_PROBE_PORT` is right, and the desktop's original choice was right.

Note the gateways *refuse* 25565 and 19132 rather than dropping them, so the
refusal branch is live against real infrastructure — it is not a theoretical
case.

### The trap: a region with no grey-cloud record

`*.gethomerun.app` is a **proxied Cloudflare wildcard**. Any name without an
explicit record falls into it and answers on Cloudflare's anycast edge instead
of a gateway. The API docs flag one already:

> `us-east.gethomerun.app` currently has no explicit record and resolves to
> Cloudflare IPs — verify before bringing a second region up.

That is the failure mode to watch, because of how it presents. A region in that
state does **not** look broken: port 80 is open at the edge, the probe returns
a fast plausible number, and the region sorts *well* — often first, because a
CDN edge is nearer than any real gateway. Measured from a machine near the DFW
edge, `us-east.gethomerun.app` and `eu-west.gethomerun.app` both returned about
16–17 ms, indistinguishable from each other and faster than either real
gateway, while being neither.

So a misconfigured region wins the ranking it should lose. **Two regions
reporting near-identical low numbers is the symptom**, and the check is one
lookup: if a region's `domain` resolves into `104.26.0.0/16` or
`172.67.0.0/16`, it is the wildcard, not a gateway.

This is infrastructure, not a host bug — no mobile change can detect a CDN edge
that answers correctly — but it is worth knowing before concluding the probe is
at fault.

## File map

| File | Role |
|---|---|
| `rust/homerun-core/src/region.rs` | `probe_target`, `DEFAULT_PROBE_PORT` — the parsing, and the post-mortem |
| `rust/homerun-pumpkin-ffi/src/host_dispatch.rs` | `net.regionLatency`, `connect_latency`, `is_measurable` — the only socket |
| `android/.../BridgeRouter.kt` | `measureLatency` — core call, `UNREACHABLE_MS` |
| `android/.../Core.kt` | `regionLatency` wrapper |
| `ios/HomerunHost/BridgeRouter+AppShell.swift` | `measureRegionLatency` — core call, `unreachableMs` |
| `ios/HomerunHost/FFI/Core.swift` | `regionLatency` wrapper |
| `homerun-app-ui/lib/regionUtils.ts` | the only caller; ranking and the `Infinity` check |
| `homerun/homerun-ui/src/electron/ipcHandler.ts` | the desktop implementation, still its own |
| `homerun/api/homerun/api/views.py` | `regions()` — where `domain` comes from |

## Triage

| Symptom | Cause | Fix |
|---|---|---|
| Every region reports 9999 | A host parsed the hostname as a URL — the original bug | It cannot any more; the host does no parsing. Check `net.regionLatency` is reached at all |
| Every region still reports 9999 | The gateway does not serve `DEFAULT_PROBE_PORT`, and the firewall drops rather than rejects | Probe a port the gateway listens on — one line, all three hosts |
| Two or more regions report near-identical low numbers | Those regions have no grey-cloud record and fall into the proxied `*.gethomerun.app` wildcard, so the probe reaches a CDN edge rather than a gateway | Infra: add an explicit DNS-only A record for that region. Check with a lookup — `104.26.x`/`172.67.x` is the wildcard |
| `the native core has no method "net.regionLatency"` | Kotlin built against a stale `.so` | `npm run rust:android-x86_64` — Gradle will not do it |
| The UI shows "9999 ms" instead of "unreachable" | The UI's dead `=== Infinity` branch | `homerun-app-ui`: treat `>= 9999` as unreachable |
| A refused port reads as unreachable on a dev machine | Windows `connect_timeout` cannot express a refusal | Expected; the phones are fine. See the section above |
| The bridge stalls while the picker is open | The core call blocks up to five seconds and was made on the main thread/actor | Android: `Dispatchers.IO`. iOS: `Task.detached` |

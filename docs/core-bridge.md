# The core bridge

How both mobile hosts reach `homerun-core`.

**Source**: `rust/homerun-pumpkin-ffi/src/core_dispatch.rs` (the dispatch, with
no platform in it), `core_bridge.rs` (the JNI adapter) and `lib.rs`'s
`homerun_core_call` (the C ABI); `android/.../Core.kt` and
`ios/HomerunHost/FFI/Core.swift` on the host side.

Dispatch is one function and both hosts wrap it:

```
Android  Java_…_nativeCall  ─┐
                             ├─►  core_dispatch::call
iOS      homerun_core_call  ─┘
```

So the platforms cannot drift in what a method *means*, only in how a string
crosses the boundary. That also makes dispatch testable on any machine — its
tests run under plain `cargo test`, no device and no emulator.

## What this is, and what it is not

It is **not a supervisor.** That distinction is the whole architecture, so it
is worth being precise about: a supervisor owns processes — spawning the JVM,
killing it, sampling its CPU, escalating a stop. None of that crosses this
bridge and none of it ever will, because **iOS cannot spawn a process at all**.
A shared supervisor could never have included iOS.

What crosses is everything a supervisor would have *decided*: which jar to run,
what the tunnel config says, when a handshake has failed for good, what a
console line means, which config files a server needs before it starts, whether
an exit was a crash. Those decisions were previously written once in the
desktop's TypeScript and again in each mobile app, and they had already
drifted — see
[`shared-core.md`](./shared-core.md) for the two divergences that prompted
this.

So: **the platform keeps the doing, the core keeps the deciding.**

## Why one entry point

There is a single native function:

```kotlin
private external fun nativeCall(method: String, args: String): String?   // Kotlin
```
```c
char *homerun_core_call(const char *method, const char *args);           // Swift
```

A dozen exported symbols would be faster by a few microseconds each and would
create a dozen places for two languages to disagree about argument order and
nullability. One entry point means each host has one parse path, adding a call
is one match arm plus one wrapper, and an unknown method is a runtime error
naming itself rather than a link error naming nothing.

The cost is a JSON round trip per call. The busiest caller is `game.classify`,
at a few hundred lines a second during world generation — nothing next to the
server producing them. If a genuinely hot path ever appears, give that one a
dedicated symbol; do not convert the rest on principle.

## The envelope

Arguments are a JSON object. Replies are always one of:

```json
{ "ok": true,  "value": <anything, may be null> }
{ "ok": false, "error": "a sentence" }
```

`Core.call` throws `Core.CoreException` on the failure arm, carrying the
`error` text unchanged.

**Errors are verdicts, not diagnostics.** "Homerun cannot host forge servers on
this device yet", "Minecraft 26.2 needs Java 25, and this version of Homerun
ships Java 21" — these are written to be shown to a player. When one reaches a
`ServerBackendException.Engine`, its wording survives to the console and the
UI. Do not reword them at the call site; fix them in the core so every platform
says the same thing.

A `null` reply from `nativeCall` means the JVM could not allocate the result
string. It is reported as a `CoreException` rather than an empty string, so
Kotlin never has two ways to fail.

## Method catalogue

Argument names are exactly as given. `?` marks optional.

Methods split two ways, and the split is the architecture:

- **game-neutral** — `game.*`, `tunnel.*`, `state.*`, `link.*`, `properties.*`.
  These take an optional `game` id (default `minecraft-java`) and route through
  the [`Game`](../rust/homerun-core/src/game.rs) trait. A host calling these
  never names a game mechanic.
- **game-specific** — `minecraft.*`. Anything with no honest cross-game
  signature lives here under its own namespace, and a second game would add its
  own rather than widening these.

An unknown `game` is an error, never a silent fallback to Minecraft: a host
asking about a game this build cannot host should hear so.

### The game surface

| Method | Arguments | Returns |
|---|---|---|
| `game.list` | — | the game ids this build can host |
| `game.classify` | `game?`, `line` | `{ ready, joined, left }` |
| `game.configInputs` | `game?`, `env` | `[{ path, encoding }]` — read these before building config |
| `game.requiredLookups` | `game?`, `env`, `gameType?` | `[{ name }]` — identities only the network can supply |
| `game.configFiles` | `game?`, `context` | `[{ path, contents, encoding }]` — write these before starting |

`encoding` is `utf8` or `latin1`, and it travels with each file so a host never
has to know that `server.properties` is latin-1. **It applies to reading as
well as writing:** decoding a latin-1 file as UTF-8 turns `§` into U+FFFD, and
writing that back turns it into `?` — a MOTD's colour codes destroyed by a
launch that changed nothing.

`requiredLookups` returns **only what the game cannot derive itself**. An
offline Minecraft server returns nothing and costs no requests, because its
UUIDs are a function of the name and the core derives them internally. A name
the host fails to resolve is simply left out of `context.resolved`, and the
game decides what that means — Minecraft skips it rather than writing an id
that can never match.

`context` is the `BuildContext`, and its field names are **snake_case where the
Rust struct is**, which is easy to "tidy up" by accident:

```json
{
  "env": { "MOTD": "…" },
  "game_type": "native-crossplay",
  "port": 25570,
  "bind_address": "127.0.0.1",
  "existing": { "server.properties": "…" },
  "resolved": [{ "name": "Notch", "id": "b50ad385-…" }],
  "now": "2026-08-10 14:03:22 +0000"
}
```

`existing`, `resolved` and `now` are optional. `now` is passed in because the
core has no clock — that is what keeps it deterministic and testable.

### The tunnel

| Method | Arguments | Returns |
|---|---|---|
| `tunnel.render` | `link`, and either `forwards` or (`game?`, `exposure?`, `port`, `geyserPort?`, `voiceChatPort?`) | the config INI |
| `link.fromServerBody` | `body` | `PolledLink` or `null` when the gateway has not provisioned yet |
| `link.isUsable` | `polled`, `before?` | bool — false when these are the dead credentials from last session |
| `deviceWs.fromLinkUpBody` | `body` | `DeviceLink` or `null` while the `link_up` task is still running |
| `deviceWs.tunnelConfig` | `link`, `httpsTarget`, `httpTarget?` | the config INI for the device websocket's own tunnel |

The two `deviceWs` methods carry the **device** link, not a server's. It arrives
flat from `POST`/`GET /api/device/<id>/link_up/` rather than nested under
`config.links[]`, which is why it has its own parser instead of a mode flag on
`link.fromServerBody`. `null` means the task has not finished — normal for the
first seconds, and not a failure to report. Omitting `httpTarget` drops the ACME
challenge forward, which is what a device serving without a certificate does.
`DeviceLink` also answers `can_serve_tls` and `expects_proxy_protocol`; see
[`plans/device-websocket.md`](../plans/device-websocket.md).

`tunnel.render` has two forms. Pass `forwards` — `[{ protocol, listen_port,
target_port }]` — and it renders exactly those, knowing nothing about any game.
Omit it and the game is asked, which is how both hosts call it today.

`exposure` is `java` (default), `bedrock` or `crossplay`. `port` is the local
port the server bound; `geyserPort` only applies to crossplay. Unknown values
error rather than defaulting, so a typo cannot silently produce a Java-only
config for a crossplay server — one that runs, and that no Bedrock player can
join.

Every `ListenPort` is fixed by what the gateway DNATs to, whatever local port
the server took. Only `Target` follows the local one.

`link` uses the **API's** field names, not Kotlin's:
`client_privkey`, `gateway_pubkey`, `link_address`, `address?`,
`allowed_ips?`. `WireProxy.Link.toJson` does that translation.

`PolledLink` is `{ link, is_gateway2 }`.

### Lifecycle

| Method | Arguments | Returns |
|---|---|---|
| `state.exit` | `intentional`, `code` | `"stopped"` or `"crashed"` |
| `state.handshake` | `watch?`, `line` | `{ watch, giveUp, recovered }` |

`state.handshake` is **stateless across the boundary**. The caller holds
`watch` as opaque JSON and hands it back with each line; the core returns the
updated one. That was chosen over a native handle so there is no allocation a
host has to remember to free, while the ten-failure threshold and the fact that
a success resets it still live in one tested place.

`giveUp` is returned **once** per watch, so a caller cannot stop a server twice.

### What a run is costing

| Method | Arguments | Returns |
|---|---|---|
| `metrics.record` | `history?`, `policy?`, `reading` | `{ history, appended, intervalMs }` |
| `metrics.query` | `history?`, `policy?`, `nowMs?` | `{ samples, intervalMs, due? }` |

**Stateless across the boundary**, like `state.handshake` above: `history` goes
in, a new one comes back, and there is nothing to free.

`reading` is `{ atMs, memUsedKb?, cpuSeconds?, playerCount? }` — **counters, and
never a percentage**. `cpuSeconds` is cumulative since the process started; a
host that pre-divides has taken a decision this module exists to own, and one
it cannot take correctly because it does not know when the previous reading
was. A missing counter means "the platform would not say", which reaches the
graph as a gap rather than as a zero nobody measured.

A `Sample` is `{ t, memUsedMb?, cpuPercent?, playerCount? }`. `cpuPercent` may
exceed 100 — a server uses more than one core, and clamping would hide exactly
the moment worth looking at. It is absent on the first sample of a run, because
a rate needs two readings.

`policy` is `{ intervalMs, maxIntervalMs, maxSamples }`, read **only when there
is no `history` to resume**, so the retention rule cannot change mid-session.
The default is the desktop's — 30 s, capped at 30 min, 360 points — so a
phone's graph of a server and a PC's graph of the same server cover the same
span.

Two things a host gets wrong otherwise:

- **Re-read `intervalMs` after every `record`.** It doubles when the buffer
  fills: a full graph drops every other point and halves its own resolution
  rather than sliding the window and forgetting the launch. A sampler still
  scheduling on the original value keeps paying to read `/proc` at a resolution
  the core has stopped keeping.
- **Offering more often than the interval is fine, and sometimes better.** A
  reading that is not due is dropped, but it is still kept as the anchor for the
  next rate — so a five-second pump feeding a thirty-second graph measures CPU
  over the last five seconds rather than averaging a spike away.

`due` is answered only when `nowMs` is given, so a host can ask whether a
reading is worth taking before paying to read it. Worth asking only where the
counter is the expensive half; where the history is (iOS holds it across the C
ABI by value) offering unconditionally is cheaper than asking.

One history per **run**, not per server: a graph covers a session, and a restart
starts a new one.

### Config files, generically

| Method | Arguments | Returns |
|---|---|---|
| `properties.merge` | `existing?`, `managed` | the merged `key=value` file |

`managed` is `[["key","value"], …]` — an ordered list rather than an object,
because keys the file does not already have are appended in exactly this order
and a JSON object would not promise it.

Comments, blank lines, and any key not in `managed` survive untouched and in
place; a managed key already present is rewritten where it sits, so the file
does not churn between launches. Most hosts never call this directly —
`game.configFiles` already applies it.

### Minecraft

Nothing here has an honest cross-game equivalent, which is why it is namespaced
rather than promoted.

| Method | Arguments | Returns |
|---|---|---|
| `minecraft.jar.resolveVersion` | `manifest`, `version?` | version string — absent, blank or `LATEST` all mean the latest **release**, never a snapshot |
| `minecraft.jar.metadataUrl` | `manifest`, `version` | the per-version metadata URL to fetch |
| `minecraft.jar.vanilla` | `metadata`, `version` | `Artifact` |
| `minecraft.jar.paper` | `builds`, `version`, `requiredJava?` (default 21) | `Artifact` |
| `minecraft.jar.parseLoader` | `type?` | `"vanilla"` or `"paper"`; anything needing an installer **errors by name** |
| `minecraft.jar.selectRuntime` | `artifact`, `bundled` (Java majors staged) | the major to launch — the **lowest** that satisfies the jar — or errors with a sentence for the player |
| `minecraft.jar.satisfies` | `onDisk`, `artifact` | bool — is the jar on disk exactly this artifact |
| `minecraft.jar.couldSatisfy` | `onDisk`, `version?`, `loader?` | bool — the looser offline fallback |
| `minecraft.settings.fromEnv` | `env`, `gameType?`, `loader?`, `fallbackMotd?` | resolved `Settings` |
| `minecraft.settings.properties` | `settings`, `runtime` | `[[key, value], …]` for `properties.merge` |
| `minecraft.settings.offlineUuid` | `name` | the UUID an offline server derives |
| `minecraft.settings.dashUuid` | `undashed` | Mojang's 32-char hex, dashed |
| `minecraft.settings.opsJson` | `players` | `ops.json` content |
| `minecraft.settings.whitelistJson` | `players` | `whitelist.json` content |
| `minecraft.settings.bannedMissing` | `existing?`, `banned` | names not already banned |
| `minecraft.settings.mergeBanned` | `existing?`, `additions`, `created` | the merged file, or `null` |

**Artifact resolution is deliberately not on the `Game` trait.** Minecraft
resolves a jar from Mojang's manifest; another game might resolve a Steam
depot, a container image, or nothing at all because it ships in the app. Those
have no honest common signature, and forcing one produces a method every
implementation ignores half the arguments of. `game.rs` says so, so nobody
"finishes" it later.

`Artifact` is `{ url, loader, version, checksum: { algorithm, hex } | null,
required_java, size_bytes }`. `algorithm` is Rust's variant name — `Sha1` or
`Sha256` — and `Artifact.fromCore` maps it to the JCA spelling
(`SHA-1` / `SHA-256`) that `MessageDigest.getInstance` demands. Passing the
Rust name straight to the JVM throws.

`onDisk` is `{ loader, version, checksum? }`, which is exactly what
`homerun-jar.json` holds.

Most of the `settings.*` methods are **unused by both hosts today** —
`game.configFiles` covers the whole path. They stay because a host that wants
RCON needs `settings.properties` with an `rcon` block, which `configFiles`
deliberately omits, and because they are the natural surface for a desktop
adoption.

## Adding a method

1. Put the logic in `homerun-core` with tests. **Break it on purpose and check
   a test fails** before moving on — see the mutation-testing note in
   `shared-core.md`.
2. Add a match arm in `core_dispatch.rs::dispatch`. Use the `field` / `text` /
   `optional_text` helpers so a missing argument produces a message naming the
   method and the argument. Namespace it: game-neutral methods take a `game`
   id and route through the trait; anything Minecraft-shaped goes under
   `minecraft.*`.
3. Add a test in `core_dispatch.rs` — dispatch is host-testable, so there is
   no excuse for the first run being on a device.
4. Add typed wrappers in **both** `Core.kt` and `Core.swift`. Keep them thin —
   parse the shape, return a native type. No decisions.
5. `npm run test:core && npm run test:core:lint && npm run test:rust`, then
   `npm run rust:android-x86_64` to rebuild the `.so`.

**Do not change the [`Game`](../rust/homerun-core/src/game.rs) trait's
signature.** It is frozen — three codebases build against it in parallel and a
change breaks them silently, because methods resolve by string at runtime
rather than by symbol at link time. `game.rs`'s `tests::Frozen` implements it
against the exact signatures, so a change fails to compile with a pointer back
to the rule. Additive methods with defaults are fine; new struct fields must
be `#[serde(default)]`.

**Rebuilding the Rust is not optional and Gradle will not do it for you.** A
Kotlin change referencing a new method compiles fine against a stale `.so` and
fails at runtime with `the native core has no method "…"`. That error means
exactly this, every time.

## Rules

**Panics must not cross.** A panic unwinding through JNI aborts the VM, and
through the C ABI is undefined behaviour — on a phone either is the whole app.
`core_dispatch::call` runs inside `catch_unwind`, so a panic becomes an
ordinary error naming the method. If you see
`the native core panicked handling "x"`, that is a bug in this crate, not bad
input.

This is proven rather than asserted: a `cfg(test)` method panics on purpose and
a test checks the envelope comes back. "We call `catch_unwind`" read from the
code is not evidence.

**No blocking work.** Nothing here opens a socket or touches a file, and it
should stay that way — calls are made from coroutines that assume they return
promptly. Transport belongs to the platform.

**Thread-safety comes free** because nothing is stateful. `state.handshake` is
the closest thing to state and it round-trips through the caller. Do not add a
global.

**Both sides of a shape change move together.** The Rust struct's serde names
are the contract; there is no version negotiation, because both halves ship in
one binary. Renaming a field means updating the Kotlin *and* Swift mappers in
the same commit — and the field names are pinned by tests in `game.rs` and
`core_dispatch.rs` precisely because a rename compiles cleanly and fails at
runtime on a device.

Watch the casing: `game_type` and `bind_address` are snake_case while `port`
and `env` sit beside them in the same object. That is the Rust struct's shape,
and "tidying" it to camelCase is a silent break — the host swallows the parse
failure and falls back to the server's own defaults, which looks exactly like
"settings not configured".

## Triage

**`the native core has no method "x"`** — the `.so` predates the Kotlin. Run
`npm run rust:android-x86_64` (or `rust:android` for a device).

**`UnsatisfiedLinkError` on `nativeCall`** — the library did not load at all.
`Core` and `NativeServer` both `System.loadLibrary("homerun_pumpkin_ffi")`; the
JVM dedupes, so this means the `.so` is missing for the device's ABI, not that
they conflict.

**A Swift symbol not found for `homerun_core_call`** — the header is not in the
bridging header, or the staticlib predates it. `ios/HomerunHost/FFI/HomerunFFI.h`
declares the whole C surface.

**`bad arguments: …`** — the args object was not valid JSON. Almost always a
Kotlin wrapper building a raw string instead of using `buildJsonObject`.

**A checksum error mentioning an algorithm the JVM does not know** — something
passed `Sha1`/`Sha256` through without mapping to `SHA-1`/`SHA-256`. That
translation happens in exactly two places, both in `ServerJar.Artifact`:
`toJson` going out and `fromCore` coming back. If you add a third crossing
point, route it through those rather than repeating the `when`.

**The Kotlin says one thing and the core another** — the core wins, and the
Kotlin copy should be deleted rather than reconciled. That duplication is the
problem this exists to solve.

## iOS

**Wired.** `homerun_core_call(method, args)` is the C-ABI sibling of the JNI
entry, and both call the *same* `core_dispatch::call` — so the platforms cannot
disagree about what a method means, only about how a string crosses the
boundary.

```c
char *homerun_core_call(const char *method, const char *args);
void  homerun_free_string(char *ptr);
```

Declared in `ios/HomerunHost/FFI/HomerunFFI.h`; add it to the bridging header.
`ios/HomerunHost/FFI/Core.swift` has typed wrappers mirroring `Core.kt` — prefer
those to calling the C function directly, and note that the reply must be freed
on every path, which `Core.call` does with a `defer`.

`WireProxy.swift` is the first adopter: it renders the tunnel config through
`Core.renderTunnel` instead of the hand-written copy it used to carry.

What is *not* done: Pumpkin needs its own artifact resolution rather than
borrowing `minecraft.jar.*` — see the note in `game.rs` about why artifact
resolution is deliberately outside the `Game` trait. `DeviceRegistrar.swift`
and `HomerunAPI.swift` also parallel Android's equivalents and are candidates
for the same treatment.

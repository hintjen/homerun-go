# The core bridge

How Kotlin reaches `homerun-core`.

**Source**: `rust/homerun-pumpkin-ffi/src/core_bridge.rs` (Rust side),
`android/.../Core.kt` (Kotlin side).

## What this is, and what it is not

It is **not a supervisor.** That distinction is the whole architecture, so it
is worth being precise about: a supervisor owns processes — spawning the JVM,
killing it, sampling its CPU, escalating a stop. None of that crosses this
bridge and none of it ever will, because **iOS cannot spawn a process at all**.
A shared supervisor could never have included iOS.

What crosses is everything a supervisor would have *decided*: which jar to run,
what the tunnel config says, when a handshake has failed for good, what a
console line means, whether an exit was a crash. Those decisions were
previously written once in the desktop's TypeScript and again in this app's
Kotlin, and they had already drifted — see
[`shared-core.md`](./shared-core.md) for the two divergences that prompted
this.

So: **the platform keeps the doing, the core keeps the deciding.**

## Why one entry point

There is a single native function:

```kotlin
private external fun nativeCall(method: String, args: String): String?
```

A dozen mangled `Java_…` symbols would be faster by a few microseconds each
and would create a dozen places for two languages to disagree about argument
order and nullability. One entry point means Kotlin has one parse path, adding
a call is one match arm plus one wrapper, and an unknown method is a runtime
error naming itself rather than an `UnsatisfiedLinkError` naming nothing.

The cost is a JSON round trip per call. The busiest caller is
`console.classify`, at a few hundred lines a second during world generation —
nothing next to the JVM producing them. If a genuinely hot path ever appears,
give that one a dedicated symbol; do not convert the rest on principle.

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

### Jars

| Method | Arguments | Returns |
|---|---|---|
| `jar.resolveVersion` | `manifest`, `version?` | version string — absent, blank or `LATEST` all mean the latest **release**, never a snapshot |
| `jar.metadataUrl` | `manifest`, `version` | the per-version metadata URL to fetch |
| `jar.vanilla` | `metadata`, `version` | `Artifact` |
| `jar.paper` | `builds`, `version`, `requiredJava?` (default 21) | `Artifact` |
| `jar.parseLoader` | `type?` | `"vanilla"` or `"paper"`; anything needing an installer **errors by name** |
| `jar.checkJava` | `artifact`, `bundledJava?` | `true`, or errors with a sentence for the player |
| `jar.satisfies` | `onDisk`, `artifact` | bool — is the jar on disk exactly this artifact |
| `jar.couldSatisfy` | `onDisk`, `version?`, `loader?` | bool — the looser offline fallback |

`Artifact` is `{ url, loader, version, checksum: { algorithm, hex } | null,
required_java, size_bytes }`. `algorithm` is Rust's variant name — `Sha1` or
`Sha256` — and `Artifact.fromCore` maps it to the JCA spelling
(`SHA-1` / `SHA-256`) that `MessageDigest.getInstance` demands. Passing the
Rust name straight to the JVM throws.

`onDisk` is `{ loader, version, checksum? }`, which is exactly what
`homerun-jar.json` holds.

### The tunnel

| Method | Arguments | Returns |
|---|---|---|
| `wireproxy.render` | `link`, `port?`, `exposure?`, `geyserPort?`, `voiceChatPort?` | the config INI |
| `link.fromServerBody` | `body` | `PolledLink` or `null` when the gateway has not provisioned yet |
| `link.isUsable` | `polled`, `before?` | bool — false when these are the dead credentials from last session |

`exposure` is `java` (default), `bedrock` or `crossplay`. `port` is the local
port the server bound; `geyserPort` only applies to crossplay. Unknown values
error rather than defaulting, so a typo cannot silently produce a Java-only
config for a crossplay server.

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
updated one. That was chosen over a native handle so there is no allocation
Kotlin has to remember to free, while the ten-failure threshold and the fact
that a success resets it still live in one tested place.

`giveUp` is returned **once** per watch, so a caller cannot stop a server
twice.

### Console

| Method | Arguments | Returns |
|---|---|---|
| `console.classify` | `line` | `{ ready, joined, left }` |

`joined` and `left` are player names or `null`. `ready` means the server
printed `Done (…)!` and is accepting connections — which is **not** the same as
the server being reported `running`, since that waits for the tunnel too.

## Adding a method

1. Put the logic in `homerun-core` with tests. **Break it on purpose and check
   a test fails** before moving on — see the mutation-testing note in
   `shared-core.md`.
2. Add a match arm in `core_bridge.rs::dispatch`. Use the `field` / `text` /
   `optional_text` helpers so a missing argument produces a message naming the
   method and the argument.
3. Add a typed wrapper in `Core.kt`. Keep it thin — parse the shape, return a
   Kotlin type. No decisions.
4. `npm run test:core && npm run test:core:lint`, then
   `npm run rust:android-x86_64` to rebuild the `.so`.

**Rebuilding the Rust is not optional and Gradle will not do it for you.** A
Kotlin change referencing a new method compiles fine against a stale `.so` and
fails at runtime with `the native core has no method "…"`. That error means
exactly this, every time.

## Rules

**Panics must not cross.** A panic unwinding through JNI aborts the VM, which
on a phone is the whole app. Every dispatch runs inside `catch_unwind`, and a
panic becomes an ordinary error naming the method. If you see
`the native core panicked handling "x"`, that is a bug in this crate, not bad
input.

**No blocking work.** Nothing here opens a socket or touches a file, and it
should stay that way — calls are made from coroutines that assume they return
promptly. Transport belongs to the platform.

**Thread-safety comes free** because nothing is stateful. `state.handshake` is
the closest thing to state and it round-trips through the caller. Do not add a
global.

**Both sides of a shape change move together.** The Rust struct's serde names
are the contract; there is no version negotiation, because both halves ship in
one APK. Renaming a field means updating the Kotlin mapper in the same commit.

## Triage

**`the native core has no method "x"`** — the `.so` predates the Kotlin. Run
`npm run rust:android-x86_64` (or `rust:android` for a device).

**`UnsatisfiedLinkError` on `nativeCall`** — the library did not load at all.
`Core` and `NativeServer` both `System.loadLibrary("homerun_pumpkin_ffi")`; the
JVM dedupes, so this means the `.so` is missing for the device's ABI, not that
they conflict.

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

Not wired, and it needs more than wiring.

`homerun-core` gets **compiled** into the staticlib iOS links, because the FFI
crate depends on it — but it exports no `#[no_mangle] extern "C"` functions, so
there is no C surface to call. `core_bridge` is the only way in and it is
`#[cfg(target_os = "android")]`, because it speaks JNI.

So iOS needs a sibling to `core_bridge`: the same dispatch behind
`extern "C" fn homerun_core_call(method, args) -> *mut c_char`, with the
existing `homerun_free_string` releasing the reply. The dispatch function
itself is platform-neutral and could be lifted out of `core_bridge.rs`
unchanged — it is only the JNI string marshalling around it that is Android's.

That is a small piece of work, but it is work, and none of it is worth doing
before iOS runs something real: it still uses `StubEngine`, and the Pumpkin
fork has never been executed through the FFI by anyone. Designing the iOS
surface around an engine nobody has run would be building on an assumption.

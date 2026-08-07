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
| handshake supervision and its threshold | CPU/memory sampling |
| console ring buffer, join/leave/ready parsing | bridge IPC |

## Modules

| Module | Reference implementation |
|---|---|
| `jar` | `src/electron/mod-installer.ts` |
| `wireproxy` | `src/electron/wireproxyConfig.ts` |
| `link` | `pollForNativeConfig` in `nativeServerManager.ts` |
| `state` | `onServerFullyRunning` + supervisor exit handling |
| `console` | `JavaServerBackend` (Android) + supervisor log handling |

The desktop is the reference for all of it, and each module names the file its
behaviour came from. Where this crate deliberately differs, it says so and why
— `jar::paper` is the clearest example.

## The tests are the deliverable

54 tests, and they are checked for teeth rather than counted. Three deliberate
regressions were introduced and all three were caught:

| Regression | Caught by |
|---|---|
| take Paper's last array element, as the desktop does | `paper_picks_the_newest_stable_not_the_last_element`, `paper_is_insensitive_to_array_order` |
| make `ListenPort` follow the local port | `a_nonstandard_local_port_moves_only_the_target` |
| drop the gateway-v2 staleness exception | `gateway_v2_accepts_an_unchanged_config` |

If you change behaviour here, do the same: break it on purpose first and check
something fails. A test that cannot fail is documentation with a runtime cost.

`the_desktop_expression_would_pick_an_alpha` is worth knowing about — it writes
out the desktop's algorithm and asserts we disagree with it. If PaperMC ever
flips the ordering back, that test starts failing, which is exactly when
someone should look at it again.

## Not done yet

The crate is built and tested; **nothing calls it in anger**. Android still has
its own Kotlin copies of all of this, and the desktop its TypeScript ones.
Adopting it is the next step, and the order matters:

1. **Android first**, because it is unreleased and the Kotlin was written from
   the same reference last week — replace `ServerJar`, `WireProxy.render`, the
   handshake watch and the console buffer with calls across the JNI bridge.
2. **Desktop last, and piecemeal.** It ships, it works, and rewriting a working
   supervisor is a well-known way to break a product. Start with the pure
   pieces (jar resolution, wireproxy config) behind the existing TypeScript
   interfaces via napi-rs. Leave `supervisor.js` owning processes.
3. **Process supervision stays per-platform.** It cannot be shared with iOS,
   and pretending otherwise is where this design would go wrong.

Adding Rust to the desktop build is real CI work and a new way for a release to
fail. Worth doing deliberately, not as a side effect.

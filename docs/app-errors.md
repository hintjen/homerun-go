# App error reporting

## Overview

Every unexpected failure in the mobile apps — a JavaScript throw, a Kotlin
exception, a Swift crash, a Rust panic, an API response the client could not
use — lands in one table on the API, attributed to a device, an app version and
an over-the-air bundle, **deduplicated and rate-limited before it leaves the
phone**.

Not to be confused with [`android-reporting.md`](./android-reporting.md) and
[`ios-reporting.md`](./ios-reporting.md), which are about what a device tells
the API concerning the *server it hosts* — stats, presence, console. This is
about the app failing.

## What was here before

Nothing that reported.

| Layer | What a failure left behind |
|---|---|
| Shared UI | A blank screen. No error boundary existed anywhere in the shipped tree. |
| Kotlin | A logcat tombstone nobody would read. |
| Swift | An `os.Logger` line on the device. |
| Rust | `crash-reports/*.txt`, which nothing ever opened. |
| HTTP | `{ error: string }` — `clientApi` flattened every failure and destroyed the status code, so a 404, a 403 and a 500 were the same string. |

Sentry is initialised in the bundle, but Sentry there is a **renderer**
integration: it sees the page's errors and cannot see a Kotlin stack, a Swift
stack or a Rust panic at all, and it does not know which OTA bundle was
running. It still runs; this is the source of truth.

## The shape: decisions in the core, transport in the hosts

`homerun-core` is a pure crate — serde, serde_json, ed25519-dalek and nothing
else. Its header states the rule: *decisions and shapes belong here, transport
and processes do not.* The reporter obeys it. It is a **decision module**, not a
transport: it returns a `reporting::Request { method, path, body, auth }` and
the host performs it, exactly as `reporting::crash` already worked.

**This is not a style preference.** `reqwest` sits behind the `device-ws`
feature, which `scripts/targets.js` enables for Android only. iOS compiles no
Rust HTTP client, and putting one in the core would link an HTTP stack into the
iOS static library to send an error report.

```
  five intakes  ──▶  core_dispatch "error.report"
                     │
  homerun-core       ├─ fingerprint ─ dedup ─ rate-limit ─ redact ─ truncate
                     │
                     └─▶ Request { POST /api/app-error/ }  │  or held, and counted
                                   │
  host               ────────────▶ sign it, send it, forget it
```

Everything platform-neutral lives in the core so the two phones cannot drift on
any of it — what counts as the same bug, what gets redacted, when to stay
quiet.

## The five intakes

| # | Source | Mechanism | Path |
|---|---|---|---|
| 1 | JS render throw | `ErrorBoundary` (two, at two depths) | live |
| 2 | JS uncaught / rejected | bubble-phase `error` + `unhandledrejection` | live |
| 3 | JS before the bundle boots | host document-start hook → `__host:jsError` | live |
| 4 | API failure | `clientApi`'s four primitives, 6 call sites | live |
| 5 | Kotlin / Swift / Rust death | uncaught handler, panic hook, and the OS | **stash → drain** |

Intakes 1–4 reach the host over the `report-error` bridge channel (see
[`ios-bridge.md`](./ios-bridge.md) / [`android-host.md`](./android-host.md)).
Intake 5 does not send at all — see below.

### Why 5 is different

`Thread.setDefaultUncaughtExceptionHandler` runs on the thread that is about to
be killed and hands straight to `KillApplicationHandler`. A coroutine launched
there never resumes and an HTTP request never completes. Swift's
`NSSetUncaughtExceptionHandler` is the same story with the process already
unwinding.

So a dying process **writes a file and returns**. The next launch drains it.
That is `AppErrors.drain()` on both hosts, and it is why a crash row's
`session` is the session of the process that *died*, not the one that sent it —
the stash carries its own context, and attributing a crash to the launch that
happened to deliver it would be wrong.

### `send`, not `invoke`

`report-error` is a fire-and-forget send. An unanswered invoke hangs a UI
promise for ever (PROTOCOL.md §5), and this repo has the scar — `share-content`
on iOS declared a capability with no handler and produced a share sheet that
never opened and never failed. The error boundary calls this during a React
commit; a reporter that can hang is worse than no reporter.

It also degrades correctly: Android logs and drops an unknown send when
`envelope.id == null`, so an OTA bundle landing on a pre-revision-12 host is
silent rather than broken.

## The pre-boot hook, and its handoff

Both hosts inject an `error` / `unhandledrejection` listener **at document
start**, before the bundle's first line runs. It exists for the one failure a
React error boundary can never see: a bundle that throws on its way up, leaving
a blank screen with no page to report from and no tree for a boundary to sit
in.

`__host:jsError` is a **protocol-level method**, handled ahead of the channel
table and deliberately absent from `channels.ts` — so it needs no contract
entry and no host revision bump.

Past boot it stands down, gated on `window.__homerunPageErrors`, which the
bundle sets in `installGlobalErrorReporting`. Without that gate every UI error
after boot arrives **twice**, and the hook's copy is the worse one: no stack,
no real error name, and filed for ever under `kind: "boot"`.

The gate degrades correctly in both directions. An older bundle never sets the
flag, so the hook keeps reporting — which is the only reporting that bundle
has. A newer bundle on an older host sets a flag nothing reads, and the host
keeps reporting alongside the page, which is what both hosts did before the
flag existed.

## Grouping — the fingerprint

`fingerprint = sha1(source ␟ kind ␟ signature)[..16]`, and the `signature` is
**sent too, human-readable**, so a reviewer can see *why* two things grouped
without reversing a hash.

| Input | Rule |
|---|---|
| `Source::Api` | The message is ignored outright. Signature is `METHOD path_shape status`, ids replaced with `{id}` — otherwise one fingerprint per server and dedup does nothing. |
| With a stack | The first 3 *meaningful* frames as `file:symbol`. **Line numbers excluded** — the bundle is minified and rebuilt weekly. |
| No stack | `location` plus the generalised message. |
| Message | Generalised before hashing, never before sending: digits → `#`, hex runs → `#`, UUIDs → `#`. So `(reading 'players')` and `(reading 'ops')` stay apart while two servers timing out collapse. |

**App version is deliberately not in the fingerprint.** Grouping across
versions is what answers "is this still happening after the fix"; the server
groups by `(fingerprint, app_version)` when it wants the other view.

### What counts as a meaningful frame

A frame is noise when it cannot distinguish one bug from another — every React
error passes through `react-dom`, every Android death through `RuntimeInit`.
The marker list spans JavaScript, the JVM, Apple, Rust and the Android natives.
If dropping leaves nothing, the top frame is kept and allowed to be noisy.

The JVM also **wraps**: an exception from a broadcast receiver arrives as a
`RuntimeException` whose own frames are all framework, with the real fault
under `Caused by:`. The **last** `Caused by:` is what gets fingerprinted.

> Both of these were found by a phone, not by a test. The first report off a
> Pixel grouped as `RuntimeInit$MethodAndArgsCaller` — the same string every
> Android crash would have produced.

### Things that broke grouping, and are now pinned by tests

- **Bundler content hashes.** Next.js names a chunk after its content, so
  `1e90c2ccc103585c.js` changes every build. A stem that is entirely a hash
  collapses to `chunk.js`. Measured before and after on a device: two
  fingerprints became one.
- **The no-stack path used `location` verbatim** — for a `window.onerror` that
  is the full chunk URL. It now normalises, but only when the location has a
  scheme: the other thing in that field is a route pattern (`/server/[id]`),
  and taking a basename of one would leave the literal `[id]`.
- **Native offsets.** A tombstone frame's `+164` moves whenever the library is
  recompiled, so it is stripped.

## Volume, and the four loops that had to be cut

An unguarded reporter is an outage amplifier. React commits at up to ~60 Hz and
calls `componentDidCatch` once per failed commit: **~3,600 events/min/device**,
36 M/min across 10,000 devices. Worse, a polling endpoint returning 500 means
every device POSTs *to the API that is already failing*.

Guarded, a 30-minute render loop costs **≤ 7 requests instead of 108,000**,
each carrying `occurrences: ~18000`. The signal is better, not merely smaller:
one row saying 18,000 is more legible than 18,000 rows.

| Constant | Value | Why |
|---|---|---|
| `COOLDOWN_MS` | 5 min, doubling to `MAX_COOLDOWN_MS` 60 min | First sighting always sends; a loop then reports at 5 → 10 → 20 → 40 → 60 |
| `BURST_WINDOW_MS` / `BURST_MAX` | 60 s / 5 | Caps *distinct* fingerprints arriving together — a cascade where every component throws differently |
| `SESSION_MAX` / `SESSION_HARD_MAX` | 20 / 30 per process | Past 20, only a never-seen `Fatal` gets through |
| `MAX_TRACKED` | 32 fingerprints, evict least-recently-seen | Bounds memory |

**Dropped reports are never queued and never retried — they are counted.**
Every body carries `occurrences` (sightings since this fingerprint last sent)
and `suppressed` (session total). Nothing here retries, and nothing here fails
loudly.

The four loops:

1. **UI → the report endpoint fails.** `noteApiFailure` returns immediately for
   any endpoint starting `/api/app-error`. Structurally it never touches
   `useClientApi` anyway — it goes over the bridge to a different code path.
2. **The host's send fails.** Both hosts swallow, and neither catches-and-
   reports. The comment says why, so nobody "improves" it into a recursive
   reporter.
3. **The core panics while deciding.** `catch_unwind` makes it an ordinary
   error envelope — but the panic hook records it and the next drain would
   report it, and a deterministic bug would re-panic. So `error.drain` **deletes
   each file before parsing it** and stops at `MAX_DRAIN = 5`.
4. **The reporter throws inside `componentDidCatch`.** React would rethrow past
   the boundary and white-screen the app *because of the reporter*. The whole
   `reportError` body is wrapped and the catch is empty on purpose.

**A failed report never surfaces to the player.** No toast, no modal, no
banner, anywhere in this path, ever.

## The ledger lives in the FFI crate

`errors.rs` holds `static LEDGER: OnceLock<Mutex<Ledger>>`. It is **not**
round-tripped through the host, unlike `lifecycle` and `metrics`.

The round-trip pattern exists because one host object owns that state and calls
in sequence. This ledger has four concurrent producers on different threads —
the JVM crash handler, the WebView bridge thread, the Rust panic hook, the
host's reporting coroutine. Round-tripping would give each its own copy, each
dropping the others' counts, and "20 sends per session" would quietly become
"20 per caller".

It is **session-scoped and never persisted**. A persisted ledger silences a
first-launch crash loop on the second launch — precisely when the report
matters most — and would mean file I/O on a path a panic can reach.

## An error during a launch names its server

`Context.server_id` had existed since the wire format was designed and
neither host filled it. Both do now: Android from `ServerHost.hostedServerId()`,
iOS from `Reporting.hostedServerId()`. Each is a lock-free read of state the
host already keeps — `AppErrors.context()` is called from a crash handler,
and taking a monitor there can turn a crash into a hang — so the answer may
be a state change stale, which is the right trade.

It is the join key. A Rust panic or a Kotlin exception during a launch used to
be a row in one table and the server's crash report a row in another, with
nothing but a timestamp in common. With `server` set, the API can put the two
beside each other; what it does with that is in
`homerun/api/docs/service-error-reporting.md`. The crash report itself
carries the app's log too — see [`android-reporting.md`](./android-reporting.md)
§ *What travels with a crash* — so the two reports describe the same minutes
from both sides.

## FFI surface

Four arms on the existing `core_dispatch::call`, beside `reporting.crash.report`:

| Arm | Does |
|---|---|
| `error.attach` | Points the app-level crash directory at host storage. Once, at launch. |
| `error.report` | Locks the ledger, calls `observe`, returns a `Request` or a hold. All live intakes converge here. |
| `error.stash` | Serialises straight to a file. No network. What the dying-thread handlers call. |
| `error.drain` | Reads last launch's files, deletes each **before** parsing, caps at 5, returns `[Request…]`. |

No new exported symbol, so **`FFI_ABI_VERSION` did not move**. `check-abi.js`
reads three files and none of them changed; that unchanged pass is the proof
the work stayed inside the existing entry point.

`crash::install_hook()` now runs at the top of every `core_dispatch::call`. It
is idempotent behind an `AtomicBool`, so the cost is one relaxed atomic load,
and it gets the hook live on the first core call at boot on **both** platforms.
Previously it was installed only from `server.rs`, so a device that never
hosted a server had no panic hook at all.

**The app crash directory is separate from the server's.** `set_crash_dir` was
being handed the server's data directory, which restic backs up as part of the
world — panics were riding into players' world backups.

## Deaths nothing of ours could report

A SIGSEGV in JNI runs no code on the way down. Neither does an ANR, a Swift
trap (`fatalError`, a force-unwrapped nil, an out-of-bounds subscript), or the
kernel reclaiming a process for memory — which for this app is not a corner
case, because it hosts a Minecraft server on a phone.

**No signal handler was written, and that is the design.** A signal handler
runs on a thread already in an undefined state, everything it calls must be
async-signal-safe — no malloc, so no JSON — and a mistake in it turns a crash
the OS would have recorded cleanly into a corrupted one. Both platforms already
collect this and hand it over later, on an ordinary thread.

| Platform | API | Covers |
|---|---|---|
| Android 11+ | `ApplicationExitInfo` (`ExitReasons.kt`) | native crash, ANR, low-memory kill, resource kill |
| iOS | MetricKit (`ExitDiagnostics.swift`) | signal crashes, hangs |

Both report as `source: native`, on the next launch — the shape `drain` already
had. Two known limits, both hit on real hardware:

- **Android tombstones are protobuf** (Android 12+), so those rows carry no
  stack. The collector checks for NUL bytes and logs that it is skipping,
  because "no frames" and "frames we threw away" look identical in a table.
- **iOS frames are not symbolicated** — binary names and byte offsets, because
  the dSYM is on the build machine.

Both therefore put the **signal in `kind`** (`native-crash (SIGSEGV)`), which
is hashed verbatim, giving a stable group where the stack cannot. Without it,
every native death in a process shared one fingerprint. `REASON_CRASH` is
excluded on Android: the uncaught handler already stashes those *with* a stack.

> iOS is **uncompiled**. See
> [`ios-error-reporting-runbook.md`](./ios-error-reporting-runbook.md).

## Redaction

Hand-written scanning, no regex, visible placeholders — the same reasoning
`scrub.rs` already carries, because this input is attacker-influenced. Applied
in order, since an earlier category can contain a later one:

1. **Tokens** — `Bearer …`, any `eyJ`-prefixed run ≥ 40 chars, `Authorization`
   values.
2. **URL query strings**, wholesale → `?[query redacted]`. Not per-parameter:
   the OAuth path carries `code`/`state`/`nonce` and registration carries an
   email. The host is kept — it is ours, and it is how staging is told from
   production.
3. **Emails** — claim and registration endpoints quote the address back.
4. **Home directories** — `C:\Users\<name>\`, `/Users/<name>/`, the iOS
   container UUID. Android's `/data/user/0/app.gethomerun.mobile/` carries no
   user and is kept; it is load-bearing for diagnosis.
5. **IP literals**, using the scanner lifted from `scrub`.

**Not redacted, deliberately**: UUIDs — device and server ids are already
fields and are what make a report actionable — and player names, consistent
with the judgement `scrub.rs` centralises.

## Truncation

| Field | Cap | Direction |
|---|---|---|
| `message` | 1 KiB | head |
| `stack` | 8 KiB | **head** — top frames matter, the opposite of `crash::tail_bytes` |
| `signature` | 300 B | head |
| `http.body` | 2 KiB | head |
| `location` | 256 B | head |
| `extra` | 2 KiB | **all or nothing** → `{"_dropped": true}`. A half-object is a lie. |
| whole body | 16 KiB | asserted after assembly |

Char-boundary discipline is shared in `reporting::truncate` — slicing a
`String` mid-char is one of the few panics safe Rust still allows, and this
feature has five truncation sites.

## Where the code is

| Piece | File |
|---|---|
| Decisions, ledger, `observe` | `rust/homerun-core/src/reporting/app_error/mod.rs` |
| Grouping | `…/app_error/fingerprint.rs` |
| Redaction | `…/app_error/redact.rs` |
| Head/tail truncation | `rust/homerun-core/src/reporting/truncate.rs` |
| Process-global ledger, stash/drain | `rust/homerun-pumpkin-ffi/src/errors.rs` |
| Dispatch arms, panic-hook install | `rust/homerun-pumpkin-ffi/src/core_dispatch.rs` |
| Android reporter | `android/…/AppErrors.kt` |
| Android OS-reported deaths | `android/…/ExitReasons.kt` |
| Android pre-boot hook | `android/…/MainActivity.kt` (bootstrap script), `BridgeRouter.kt` (`logJsError`) |
| iOS reporter | `ios/HomerunHost/AppErrors.swift` |
| iOS OS-reported deaths | `ios/HomerunHost/ExitDiagnostics.swift` |
| iOS pre-boot hook | `ios/HomerunHost/BridgeController.swift` |
| Deliberate failures, for verifying | `HomerunApplication.kt` + `MainActivity.kt` (broadcasts), `DebugTriggers.swift` (env var) |

The page half lives in `homerun-app-ui` — see `docs/error-reporting.md` there.
The endpoint lives in `hintjen/homerun` — see `api/docs/app-errors.md`.

## Verifying it

Error reporting is the one feature whose own failure is invisible: a reporter
that quietly sends nothing looks exactly like an app with no bugs. So both
hosts ship a way to fail on purpose in debug builds.

```bash
# Android — a Kotlin crash (stash → next launch), and a live send
adb shell am broadcast -a app.gethomerun.mobile.DEBUG_ERROR
adb shell am broadcast -a app.gethomerun.mobile.DEBUG_ERROR --es mode report

# Android — the pre-boot hook, and its handoff
adb shell am broadcast -a app.gethomerun.mobile.DEBUG_JS_ERROR
adb shell am broadcast -a app.gethomerun.mobile.DEBUG_JS_ERROR --es mode handoff

# Android — a real native crash, via a real tombstone
adb shell run-as app.gethomerun.mobile.debug kill -11 $(adb shell pidof app.gethomerun.mobile.debug)
```

iOS uses `HOMERUN_DEBUG_ERROR` in the scheme's environment; the full procedure
is in [`ios-error-reporting-runbook.md`](./ios-error-reporting-runbook.md).

Two traps that cost real time on Android and will cost it again:

- **An OTA bundle outranks the bundle you built.** Check the `bundle` column:
  `shipped` is the copy inside the app, anything else came over the air. The
  mobile repo pins `homerun-app-ui#main`, so a UI change on a branch is not in
  any build until it is merged.
- **Repeats are dropped on purpose.** Firing the same trigger twice and seeing
  one row is the rate limiter working. Change the message to change the
  fingerprint.

And when reading the table: **a row is a group of failures, not one failure.**
Sum `occurrences`; `count(*)` understates reality by orders of magnitude.

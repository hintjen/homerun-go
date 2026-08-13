# Homerun Mobile Documentation

## Overview

Technical documentation for the iOS and Android hosts. Each subsystem gets
one file, written **as it is built** — see
[`../plans/shared-milestones.md`](../plans/shared-milestones.md#documentation-is-part-of-the-milestone).

UI documentation lives in
[`homerun-app-ui`](https://github.com/hintjen/homerun-app-ui); desktop
main-process documentation lives in
[`homerun`](https://github.com/hintjen/homerun) under `homerun-ui/docs/`.

## Documentation Index

### 🔨 [Building](./building.md)

How to produce what Xcode and Gradle need: the shared UI bundle staged into
each platform's assets, and the Rust FFI compiled for its targets.

**Contains**:
- `npm run doctor` — what this machine can build and what is missing
- Staging the shared UI, and the env overrides for local checkouts
- Per-target Rust builds, triples, and where each artifact must land
- Why Android native libraries must live in `jniLibs`
- Typical loops, and a triage list for the usual toolchain failures

**Read this for**: Setting up a machine, wiring CI, or decoding a toolchain
error.

---

### 🦀 [Pumpkin FFI](./ffi.md)

The Rust library both hosts link — server lifecycle, console buffering,
port pre-flight, and crash reporting behind a C ABI.

**Contains**:
- Why the crate exists (so the Pumpkin fork can shrink to library patches)
- The C surface, JSON conventions, and string ownership
- Host integration rules: the 16 MB stack, no start timeout, log cursors
- The console ring buffer and cursor semantics
- Crash handling and why the last-panic slot is cleared per run
- One-server-at-a-time, and where it is enforced
- Wiring a real engine in behind the `Engine` trait

**Read this for**: Calling the server from Swift or Kotlin, wiring Pumpkin
in, or debugging a server that will not start.

---

### 🧩 [The shared core](./shared-core.md)

`homerun-core` — the decisions every Homerun app makes, in one tested place
instead of once per platform.

**Contains**:
- The two divergences that prompted it, both live before it existed
- Why it holds decisions and shapes but no transport and no processes
- The layout: a game-agnostic core with Minecraft as one implementation
- Why the `Game` trait is frozen, and how that is enforced rather than asked
- What is shared and what stays platform-specific, and why iOS forces that line
- The test suite, and the mutation check that proves it can fail
- Who has adopted it, and the order for who adopts next

**Read this for**: Deciding where a behaviour belongs, adding a game, or
before changing anything the desktop also does.

---

### 🔌 [The core bridge](./core-bridge.md)

How both mobile hosts reach the core: one dispatch, two thin adapters, JSON in
and out.

**Contains**:
- Why it is not a supervisor, and why that distinction is the architecture
- The envelope, and why errors are verdicts meant for a player
- The full method catalogue — game-neutral `game.*`/`tunnel.*` versus
  game-specific `minecraft.*`
- The `BuildContext` wire shape, including the casing that gets "tidied" and
  silently breaks settings
- Adding a method, including the rebuild that Gradle will not do for you
- The rules: no panics across either boundary, no blocking, no global state
- Triage for both hosts

**Read this for**: Calling the core from Kotlin or Swift, adding a method, or
decoding `the native core has no method "…"`.

---

### 📱 [iOS Host](./ios-host.md)

The app around the WebView: how the project is generated, how the shared UI
bundle is embedded and served, and how the device's capabilities reach the UI.

**Contains**:
- XcodeGen project generation, and why the `.xcodeproj` is not committed
- Linking the Rust static library, including the link flags nothing references
- Why the bundle is served over `homerun-app://` and never `file://`
- Path resolution, the traversal guard, and why a missing asset 404s
- Capability injection at document start, read from the vendored contract
- Why the shell is UIKit rather than SwiftUI

**Read this for**: Setting up the Xcode build, or debugging a blank screen.

---

### 🌉 [iOS Bridge](./ios-bridge.md)

The `bridge/v1` transport: invokes, sends and events between the shared UI and
the iOS host.

**Contains**:
- Transport in both directions, and the U+2028/9 escaping that keeps it working
- The weak message-handler proxy, and what leaks without it
- The event queue and the `__bridge:ready` handshake
- Content-process death recovery, and the generation counter behind it
- Why there is no call timeout
- The channel table the conformance checker reads, and how to keep it honest

**Read this for**: Adding a channel, or debugging a screen that hangs with no
error.

---

### 🎮 [iOS Server Backend](./ios-server-backend.md)

Hosting Minecraft on the phone: the server thread, the FFI, the console, and
how a friend actually joins.

**Contains**:
- Why the server thread needs a 16 MB stack, and what happens without one
- Why starting a server has no timeout
- FFI string ownership, including on error paths
- Console cursors, and admitting to dropped output
- Which states the UI is told about, and which are host-internal
- Memory and CPU sampling for a server that is not its own process
- What the Insights graph covers, and why the host computes none of it
- World storage, iCloud exclusion, and LAN connectivity

**Read this for**: Working on server lifecycle, or debugging a server that
will not start, will not stop, or cannot be joined.

---

### 💾 [iOS Backups](./ios-backups.md)

Backing a world up and putting one back, with an engine linked into the app
because this platform cannot spawn one.

**Contains**:
- The backup lease, its lack of a timeout, and the rule everything follows from
- The three moments: restore before launch, the lease gate, the snapshot on stop
- Why the world is moved aside rather than deleted before a restore
- Foreground execution, the background-task assertion, and what its five
  seconds are actually for
- The durable outbox, and the failure it exists to prevent
- Why the restore selector is resolved in Rust and not in Swift

**Read this for**: Working on backups, or diagnosing a world that reverted, a
launch that will not start, or a lease nobody is holding.

---

### 🤖 [Android host](./android-host.md)

The Android app shell — WebView, asset loader, capability injection, the safe
area, the icons, and the bridge router's threading.

**Contains**:
- Why the bundle is served over an `https://` virtual host, not `file://`
- The aapt asset filter that silently strips Next.js's entire `_next/` bundle
- Where capability injection lands relative to the first page script, and the
  `__homerunHost.postMessage` adapter the shared transport requires
- The safe area: why the *page* holds the bars' space and the host only hands
  it the numbers, and what happens when the host tries to hold it instead
- Reading the colour under the clock instead of guessing it from a theme name
- The Android 12+ splash, and how to make it draw no icon
- Both icons, generated from one master by `scripts/generate-icons.py`
- Thread discipline: which callback lands on a binder thread and what ANRs
- Render-process death, and why queued events are dropped rather than replayed
- Building, running on an emulator, and remote debugging

**Read this for**: Working on the Android host, or diagnosing a blank screen.

---

### 🎮 [Android server backend](./android-server-backend.md)

How `native-server-*` becomes a real server running inside the app — the JNI
adapter, the thread it needs, and the polling that stands in for callbacks.

**Contains**:
- Why a JNI layer exists at all, and why it calls the C ABI rather than bypassing it
- The 16 MB engine stack, and the crash you get without it
- Who owns a server and what order a launch runs in — both answered by the core
- Getting a server jar onto the device, and the three ways to avoid downloading
  one this device already has
- Why `start` polls for *running* instead of waiting on the call
- The log pump, the perf sampler, and one-server-at-a-time
- What each backend's memory and CPU numbers actually measure
- What the memory and CPU numbers actually measure
- Triage, symptom first

**Read this for**: Working on the server backend, or wiring Pumpkin in.

---

### 🔋 [Android lifecycle](./android-lifecycle.md)

How a server — and the backup that follows it — keeps running once the app is no
longer in front of the user.

**Contains**:
- Why there is no "run in the background" permission to request, and which one
  actually prompts
- The foreground service, and why the notification is the price rather than a
  feature
- Why a wake lock is not implied by the service, and what breaks without one
- `busy`: why "is a server running" is the wrong question, and the two extra
  terms that make it right
- Why `hostingRequested` must be paired inside the branch that requested it
- The notification that is attached but never posted, and what re-posts it
- Why the launcher icon cannot be a notification icon
- `specialUse` versus `dataSync`, and the six-hour cap that decides it
- What has been verified on device and what has not
- Triage, symptom first

**Read this for**: Working on backgrounding, or diagnosing a server that dies
after the app is dismissed, a missing notification, or a session that stalls
with the screen off.

---

### 📡 [Android reporting](./android-reporting.md)

What this device tells the API about the server it runs — crashes, stats,
presence, minigame results, and operator changes typed into the console.

**Contains**:
- Why a host that never reports looks fine from the inside, and what that cost
- Which credential signs what, and why the wrong one is a *silent* success
- Why the console tail is kept rather than read back when a run ends
- Scraping console replies, and why the core parsing one *is* the test
- The CPU rescale, and why forgetting it passes every test you would write
- Why the gateway address cannot be read at launch
- The two console forgeries the core refuses and the desktop still allows
- Triage: what each null field in a report means

**Read this for**: An empty graph, a crash with no explanation, an `/op` that
does not survive a restart, or before adding anything else the API is told.

---

### 📦 [Over-the-air UI bundles](./ota-bundles.md)

Replacing the shared web bundle without a store release: which one is served,
and what happens when a new one turns out to be fatal.

**Contains**:
- `BRIDGE_HOST_REVISION`, and the ledger check that makes bumping it mandatory
- Why capabilities need no revision and channels do
- The four directories, and the manifest a bundle directory must carry
- What activate and resolve do, in order, and why both run before a WebView
- Why the probation counter is on disk, written before the page can crash
- Why there is no per-request fallback to the shipped copy
- Testing the whole thing with `adb push`, and no CDN needed

**Read this for**: Shipping a UI fix without a store release, or an app that
came back on an older UI than the one it downloaded.

---

### 🚫 [Can iOS host in the background?](../plans/ios-background-execution.md)

Not a subsystem doc — the full sweep behind "no", so it does not get
re-researched every time someone notices Android can and iOS cannot.

**Contains**:
- The two hard walls: a 50 MB cap on every process type that runs
  indefinitely, and what the server actually needs
- All fourteen background modes, each with a verdict
- Why `BGContinuedProcessingTask` is not a duration problem, and the three
  properties of a game server that do disqualify it
- The one mode that works, the App Store app already doing it, and why we
  still should not
- Guided Access, which solves it outright and needs no code
- The handoff escape hatch this repo already owns
- What to build instead, and the specific triggers for revisiting

**Read this for**: Being asked why iOS cannot do what Android does, or
deciding how far to push it.

---

### 🌐 [The tunnel wrapper plan](../plans/tunnel-wrapper.md)

Not a subsystem doc yet — the spec for sharing one wireproxy implementation
between iOS and Android, and the fork patches it needs.

**Contains**:
- Why iOS forces the question (it cannot spawn a process at all)
- The gomobile binding's exported surface, and why the config stays an INI
- The three fork patches, all landed in `wireproxy-fork`
- What linking costs: fault isolation, and nothing else that is not just work

**Read this for**: Working on the tunnel on either platform.

---

### 🔌 [The device websocket on mobile](../plans/device-websocket.md)

Not a subsystem doc yet — how the desktop serves `wss://<device-fqdn>` for the
dashboard's console and RCON, and what it would take on a phone.

**Contains**:
- The four layers desktop brings up, and the order it tears them down
- The frame protocol, and why authentication and authorisation are separate
  questions answered by different parties
- The two liveness defences a tunnelled socket cannot do without
- What mobile already has (more than expected) and what is missing (less)
- What terminating TLS on a phone actually commits us to
- Why renewal, not issuance, is the risk — and why it gets its own milestone
- Why "a plugin in homerun-core" is right, and the two-dependency constraint
  that decides which half goes there

**Read this for**: Building it.

---

### 📦 [Shipping updates without the stores](../plans/ota-updates.md)

Not a subsystem doc yet — the plan for pushing the shared UI bundle over the
air, and the version negotiation it cannot ship without.

**Contains**:
- Why both stores explicitly allow this for a WebView host, quoted
- What can and cannot move, by layer and by size
- The one resolver function per platform that is the whole mechanism
- A walkthrough: the four directories, and what happens across the two launches
  between a release and a user seeing it
- Why applying an update needs no app restart — and could not have one on iOS
- Why a WebView swap is safe for a *running* server and not for a *starting*
  one
- Why an OTA'd UI against an older host turns this protocol's worst failure
  mode from impossible into likely, and the revision counter that prevents it
- The probation rule, without which a bad bundle bricks the app in a way no
  store update can fix
- Why the Play sentence that authorises this is the same one that governs
  downloading server jars
- Why to build it rather than adopt a framework

**Read this for**: Planning a release, or deciding whether something belongs in
the core or in the UI.

---

### 📲 [iOS handoff](../plans/ios-handoff.md)

Where the iOS side stands, what was changed without a Swift compiler, and the
open questions on backups.

**Contains**:
- Which Swift files changed and have never been compiled, ranked by risk
- The app-killing tunnel bug: what it was, how it was fixed, how to re-verify it
- The `go.work` setup that will bite you if the fork is not checked out beside
  this repo
- Backups: the decisions that exist, the API contract, and why the engine is
  still open
- The iOS background-execution question, which may shape the design more than
  the engine choice does

**Read this for**: Picking the iOS work back up.

---

### 🔁 [iOS — the server lifecycle into the core](../plans/ios-core-lifecycle.md)

The port that gives iOS the same answers Android already gets from
`homerun-core::lifecycle` and `launch`: who owns a server, what an exit meant,
and what order a launch runs in.

**Contains**:
- What iOS decides for itself today, mapped to the core call that replaces it
- Five phases, each verifiable on its own, with Android's code as the reference
- The two reorderings iOS is currently wrong about, and the regression that is
  easiest to introduce — `stopForNetworkError` losing the stop intent
- Two open decisions, and one thing this port deliberately does not fix

**Read this for**: Doing that port, or deciding whether to.

---

*Add an entry here whenever you add a doc. A doc nobody can find is not
written.*

<!--
Planned, one per milestone (see plans/shared-milestones.md):

  ios-lifecycle.md        iOS      M4
  android-bridge.md       Android  M1, extended at M2
-->

## House style

Match [`ffi.md`](./ffi.md) and the desktop repo's `homerun-ui/docs/`:

```markdown
# Subsystem Name

## Overview
What it is and why it exists. Lead with the problem it solves.

## <Component> — `path/to/File.swift`
Sections named after the file they document, so a reader can jump from a
stack trace to the right heading.

## File map
| File | Role |

## Triage
Symptom → cause → fix.
```

- Document the **why**, especially anything non-obvious about the platform.
- Mark load-bearing details that break silently when changed.
- End with triage, symptom-first — that is how docs actually get read.
- Update in the same commit as the behaviour change. A stale doc is worse
  than none, because people trust it.

---

**Maintained by**: Homerun Development Team

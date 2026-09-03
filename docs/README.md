# Homerun Go Documentation

## Overview

Technical documentation for the Homerun Go iOS and Android hosts. Each
subsystem gets one file, written **as it is built**.

UI documentation lives with the UI's source, and Homerun Desktop's
main-process documentation lives with the desktop app. Both repositories are
private; where a file here names one, it is to say which side of a boundary a
change falls on.

## Documentation Index

### 🔨 [Building](./building.md)

How to produce what Xcode and Gradle need: the shared UI bundle staged into
each platform's assets, and the Rust FFI compiled for its targets.

**Contains**:
- `npm run doctor` — what this machine can build and what is missing
- Staging the shared UI, and the env overrides for local checkouts
- Per-target Rust builds, triples, and where each artifact must land
- Why Android native libraries must live in `jniLibs`
- The two rules the Go and JRE binaries obey — cgo for DNS, 16 KB alignment —
  and why the emulator cannot catch either
- **Which backend a build talks to**: the two places that hold an API URL, the
  switch on each platform (`--api` / `HOMERUN_API_URL`), and why neither moves a
  device that has already run without clearing its data
- **Which UI a build runs**: why an over-the-air bundle outranks the one in the
  binary, the flag that turns that off for a development build
  (`--no-ota` / `HOMERUN_OTA_UPDATES=0`), and why a release cannot use it
- **Push credentials**: which of the four Firebase and APNs files goes where,
  which two are private keys, and why an APNs key is scoped to Apple's build
  environment rather than to a Firebase project
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

`homerun-core` — the decisions both Homerun Go hosts and Homerun Desktop make,
in one tested place instead of once per platform.

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
- Capability injection at document start, read from the vendored manifest
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
- Crossplay: Geyser and Floodgate as plugins, and why there is no second JVM
- Why `start` polls for *running* instead of waiting on the call
- The log pump, the perf sampler, and one-server-at-a-time
- What each backend's memory and CPU numbers actually measure
- What the memory and CPU numbers actually measure
- Triage, symptom first

**Read this for**: Working on the server backend, or wiring Pumpkin in.

---

### 🧊 [Bedrock on Android](./android-bedrock.md)

How a phone hosts a **Bedrock** server: PowerNukkitX, a Bedrock server written
in Java, on the JVM this host already stages. Closes a live hole — the wizard
has offered the Bedrock tile on Android all along and the launch was then
refused.

**Contains**:
- Why Mojang's Bedrock Dedicated Server cannot ship here at all
- The five PowerNukkitX facts that were nearly wrong, `pnx.yml` first
- The YAML merge, and the two rules that keep it from corrupting the file
- Where a seed can go, and why it is written exactly once
- The command line, and why every flag on it is load-bearing
- Updating the jar without a store release, and the pin that makes a bad
  release stoppable
- **The tunnel bug that predates this**: `java` for everything, and a Bedrock
  server behind a TCP tunnel that runs, goes green and cannot be joined
- Why Bedrock ignoring SRV reaches further than the tunnel
- What did not need changing — backups, the lease, the stop ladder, RCON
- Triage, symptom first

**Read this for**: Working on the Bedrock backend, or wondering why a server
that looks healthy has nobody in it.

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

### 📡 [iOS reporting](./ios-reporting.md)

The same subsystem on the other host, written as the delta: three deliberate
differences, and what a linked engine changes about asking a server anything.

**Contains**:
- Why there is no console tail here, when Android needs one
- Why the cadence is a timer, and why suspension needs no teardown at all
- Feeding a second subscriber when the backend offers one closure per event
- What Pumpkin actually prints for `list uuids` and `time query gametime`, and
  why the roster was silently empty on every report
- Why `loader` must never be `paper` on this host
- Triage: what each null field means when the engine, not a console, answered

**Read this for**: An empty Insights graph on iOS, a null `age` or `players`,
an `/op` that does not survive a restart, or before trusting a report this host
sent.

---

### 📍 [Region latency](./region-latency.md)

One channel, three hosts, and the reason a player can be put on the wrong
continent without a single error being logged.

**Contains**:
- What `domain` actually is — a bare SRV target, and why nothing in the
  contract says so
- The bug both mobile hosts shipped: a hostname parsed as a URL, every region
  reporting 9999, and no packet ever sent
- Why the parsing and the socket both left the hosts, and what is left behind
- Why the probe is a bare TCP connect rather than Server List Ping, and why a
  *refused* port is a valid measurement
- Why port 80 is a three-host decision, and why DNS is resolved off the clock
- Why the refusal rule is dead code on Windows, and how it is tested anyway
- The `9999` sentinel, why it stays in the hosts, and the UI's `=== Infinity`
  check that can never fire
- An unverified assumption about what these hostnames resolve to, and how to
  settle it

**Read this for**: A region picker that ranks everything equally, or before
changing how any host measures a region.

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
- **Why there is no update prompt**: a bundle applies as soon as it arrives,
  the two things that defer it, and the five triggers that re-ask
- Testing the whole thing with `adb push`, and no CDN needed

**Read this for**: Shipping a UI fix without a store release, an app that came
back on an older UI than the one it downloaded, or a bundle that downloaded and
did not go live.

---

### 🔑 [Sign in with Google and Apple](./social-login.md)

One bridge channel, two host implementations, and a browser — because Google
refuses to authenticate inside the WebView both hosts are built on.

**Contains**:
- Why the sign-in cannot happen in our WebView, and what that forces
- The split: why the host owns only "open a browser", and the OAuth lives in
  the shared UI
- `auth:web-session`, revision 7, and why `canceled` is not an error
- Android: Custom Tabs without the `androidx.browser` dependency, and why the
  callback must be claimed *before* the deep-link emit
- The 700 ms cancel grace, and why a dismissed Custom Tab is invisible
- iOS: the three lines that break silently — the ARC capture, the cookie jar,
  the file-scope anchor
- Why PKCE is hashed in JavaScript, and the iOS secure-context rule behind it
- The realm state the hosts assume and cannot check

**Read this for**: Working on social sign-in, adding a provider, or a sign-in
that spins for ever.

---

### 🧳 [Validating guest-server migration on iOS](./ios-guest-migration-validation.md)

A guest's servers should follow them onto the account they sign up or sign in
to. The API and the UI are shared and already proven on Android; this is how
to prove the one iOS-specific part.

**Contains**:
- Why a device row belongs to exactly one account, and the two endpoints that
  refuse it when it does not
- The run: guest, server, upsell, then either registering or signing in —
  both migrate
- What to check on the device (`registeredDeviceAccount` vs `currentAccount`),
  in the console, and on the backend
- Why "the servers moved" is *not* evidence the iOS fix works, and what is
- That the Swift has never been compiled

**Read this for**: Verifying migration on iOS, or a phone that migrates and
then says "running on another device".

---

### 🧯 [App error reporting](./app-errors.md)

Every unexpected failure — JavaScript, Kotlin, Swift, Rust, and an API
response the client could not use — in one table, deduplicated and
rate-limited before it leaves the phone. Not the same thing as
[`android-reporting.md`](./android-reporting.md), which is about the server a
device hosts.

**Contains**:
- Why the reporter is a *decision* module in the core and a transport in the
  hosts, and why `reqwest` being Android-only settles it
- The five intakes, and why native deaths stash to disk instead of sending
- Grouping: what goes into a fingerprint, what is deliberately left out, and
  the three things a real phone proved were breaking it
- The rate limiter's actual numbers, the volume they prevent, and the four
  feedback loops that had to be cut
- Why the ledger lives in the FFI crate rather than round-tripping through the
  host
- The four `error.*` dispatch arms, and why `FFI_ABI_VERSION` did not move
- `ApplicationExitInfo` and MetricKit — the deaths no code of ours could
  report, and why no signal handler was written
- How to make either host fail on purpose, and the two traps that cost real
  time

**Read this for**: Anything about how a failure becomes a row.

---

### 🧪 [Validating error reporting on iOS](./ios-error-reporting-runbook.md)

Every unexpected failure — JavaScript, Kotlin, Swift, Rust — lands in one
table. The core logic, the endpoint and the Android host are verified on
hardware; the Swift has never been compiled. This is how to prove it.

**Contains**:
- What is already proven and what is not, so no time goes on things that work
- The four native triggers (`HOMERUN_DEBUG_ERROR`) and the two JS ones you
  throw by hand from Safari's Web Inspector
- MetricKit: why a crash is not reportable for a day, and the Xcode menu item
  that gets you a payload now
- Why a native crash has offsets instead of function names, and why `kind`
  carries the signal because of it
- The three things that otherwise cost an hour: an OTA bundle outranking the
  one you built, the reporter dropping repeats on purpose, and one row being a
  *group* of failures

**Read this for**: Bringing the iOS half up on a Mac, or a report that did not
arrive.

---

### ⛏️ [The Minecraft account on mobile](./minecraft-account.md)

Which Minecraft player a phone belongs to. Stats are keyed on a Minecraft uuid
and every read of them takes one as input, so without this the Minigames Hub
could only ever show a signed-in user zero of their own numbers.

**Contains**:
- The two independent paths — an account linked on the desktop (no sign-in at
  all, and the one most users take) and a Microsoft sign-in on the phone
- Why `linkedAccount` is kept apart from `credentials`: an identity is not a
  credential and has nothing to sign out of
- Why device code rather than a redirect, and what the public Xbox client id
  buys us
- What differs between the two hosts, which is only storage, transport and how
  the approval page is opened — and why iOS uses `ASWebAuthenticationSession`
  for `auth:web-session` but not for this
- The two failure modes that are not failures: pending polls arriving as HTTP
  400, and Android cutting a backgrounded app's DNS mid-sign-in
- Why the sign-in completes when the user comes back, not when they approve
- Why no token crosses into the WebView, and what goes over instead
- Registering our own Azure app: the three settings that decide whether it works
  at all, how to frame the review, and what changes in the code if it lands

**Read this for**: A phone showing zero stats, a sign-in that never resolves, or
applying for Minecraft API access.

---

### 🔔 [Remote push on Android](./android-push.md)

How a message sent by the API lands in this phone's tray — FCM behind the
`remotePush` capability, bridge host revision 9.

**Contains**:
- The split that is the whole architecture: the host owns the OS permission
  and the token, the shared UI owns the API registration over the user's JWT
- Why background delivery involves no app code, and why the channel must
  exist before the process does
- The permission vocabulary, and why "was it ever asked" is a prefs flag
- The tap: `google.message_id` as the discriminator, and the ready-handshake
  queue it must ride
- `google-services.json` staged **by backend, not build type**, and the
  `SENDER_ID_MISMATCH` that punishes crossing them
- Triage, symptom first — including the two silences (denied permission,
  force-stopped app) that read exactly like non-delivery

**Read this for**: Working on push, a notification that never arrived, or a
token the API keeps deleting. The API documents its half.

---

### 🧱 [Mods and plugins on Android](./android-mods.md)

How a server gets the mods it is configured with — and the gap it closed:
**before this, Android installed no mods and no plugins at all**, on any
loader, while advertising that it could.

**Contains**:
- Why the logic is in `homerun-core` and not in Kotlin, and what that is worth
- The step machine, and why downloads are steps rather than a final pass
- Why a failed step is data rather than an exception
- Where the sync runs in a launch, and why it cannot fail one
- `.homerun-loader.json`'s two writers, and why every write is a merge
- The one rule the stale sweep must not break
- The shared fixtures, and what they are for
- Why a **Quilt** server resolves almost no mods, and why that is left alone
- What a real Modrinth run installed on a phone, and what it correctly skipped

**Read this for**: Touching mod resolution, or wondering why a plugin did not
install.

---

### 🌉 [Crossplay on Android](./crossplay.md)

How a Java server on a phone becomes one Bedrock players can join — Geyser and
Floodgate as plugins inside the server's own JVM, with no second process. **Was
offered and did not work**: the wizard sold it, the gateway forwarded a Bedrock
port, and nothing ever installed Geyser.

**Contains**:
- Why plugin mode rather than the desktop's Geyser Standalone
- Why Paper and not Fabric, decided on how many Minecraft versions each
  Geyser build spans
- Why Geyser is derived at launch and never stored, and what that buys
- Why Floodgate needs its own downloader when Geyser does not
- Why the config is a seed rather than a sync, and why there is no port probe
- What the gateway forwards, and why `geyser_enabled` is `False` on a working
  crossplay server
- A real device run: the log lines, the Floodgate UUID, and the protocol number
  that is **not** a cause of failure
- Triage from the jars outward, and one failure recorded as unexplained

**Read this for**: A Bedrock player who cannot join, or touching anything
between `MODRINTH_PROJECTS` and the UDP tunnel.

---

Some subsystems are documented only as planning notes in the private
repository that holds the release pipeline (`plans/…`): iOS background
execution, mod loaders, the tunnel wrapper, the device websocket, OTA
publishing, and the iOS handoff. Docs here cite those notes by name; the
decisions they settled are restated where they apply. Publishing to Play and
TestFlight is documented there too, beside the workflows that do it.

*Add an entry here whenever you add a doc. A doc nobody can find is not
written.*

<!--
Planned, one per milestone (see plans/shared-milestones.md):

  ios-lifecycle.md        iOS      M4
  ios-push.md             iOS      once M4 of plans/push-notifications.md verifies

(android-bridge.md landed as android-host.md.)
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

**Maintained by**: the Homerun Go team at Hintjen

<!-- CLA probe 2: throwaway PR, closed unmerged. -->

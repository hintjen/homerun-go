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

### 🤖 [Android host](./android-host.md)

The Android app shell — WebView, asset loader, capability injection, and the
bridge router's threading.

**Contains**:
- Why the bundle is served over an `https://` virtual host, not `file://`
- The aapt asset filter that silently strips Next.js's entire `_next/` bundle
- Where capability injection lands relative to the first page script, and the
  `__homerunHost.postMessage` adapter the shared transport requires
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
- Why `start` polls for *running* instead of waiting on the call
- The log pump, the perf sampler, and one-server-at-a-time
- What the memory and CPU numbers actually measure
- Triage, symptom first

**Read this for**: Working on the server backend, or wiring Pumpkin in.

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

*Add an entry here whenever you add a doc. A doc nobody can find is not
written.*

<!--
Planned, one per milestone (see plans/shared-milestones.md):

  ios-host.md             iOS      M0
  ios-bridge.md           iOS      M1, extended at M2
  ios-server-backend.md   iOS      M3
  ios-lifecycle.md        iOS      M4
  android-bridge.md       Android  M1, extended at M2
  android-lifecycle.md    Android  M4
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

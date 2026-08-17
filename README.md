# homerun-mobile

The iOS and Android hosts for Homerun — run a Minecraft server on your phone.

This repo is the **source of truth for mobile**. It contains no UI: every
screen comes from [`hintjen/homerun-app-ui`](https://github.com/hintjen/homerun-app-ui),
the same bundle the desktop app embeds. What lives here is the platform half —
the WebView host, the bridge implementation, and the server engines.

```
homerun-mobile/
├── ios/       Swift host — WKWebView, bridge, PumpkinBackend
├── android/   Kotlin host — WebView, bridge, Pumpkin + JVM backends
├── rust/      homerun-pumpkin-ffi — the C ABI both platforms link
├── shared/    the vendored bridge contract + conformance checker
└── scripts/   contract sync
```

## Architecture

```
        homerun-app-ui  (static web bundle, embedded in the app)
                │
                │  bridge/v1  — JSON over the platform's WebView channel
                ▼
   ┌─────────────────────────┐   ┌─────────────────────────┐
   │  iOS host (Swift)       │   │  Android host (Kotlin)  │
   │  WKWebView + router     │   │  WebView + router       │
   └───────────┬─────────────┘   └───────────┬─────────────┘
               │  ServerBackend              │  ServerBackend
               ▼                             ▼
        PumpkinBackend              PumpkinBackend │ JavaServerBackend
        (Rust, in-process)          (Rust, JNI)    │ (bundled JRE)
```

Two abstraction layers, and they do different jobs:

- **`bridge/v1`** separates the UI from every host. Frozen and versioned;
  spec in `shared/conformance/PROTOCOL.md`.
- **`ServerBackend`** separates the host from the engine. Per-platform
  (`ios/HomerunHost/ServerBackend.swift`,
  `android/.../ServerBackend.kt`) so the `native-server-*` channels are wired
  once no matter what is underneath.

## Platform constraints that shape everything

These are not preferences; they decide the design.

| Constraint | Consequence |
|---|---|
| **iOS cannot spawn processes** | iOS is Pumpkin-only, in-process via FFI. No pids, no stdio pipes anywhere in the interfaces. |
| **iOS forbids JIT** | wasmtime must run Pulley (AOT → interpreted bytecode). |
| **iOS has no background mode for a server** | Backgrounding suspends the server. v1 keeps the screen awake and says so plainly; `backgroundExecution: false`. This is a product limitation, not a TODO. |
| **Android API 29+ cannot exec from writable storage** | The bundled JRE ships in the APK as `jniLibs`. Server jars are data, so those may still download. |
| **Android 14+ foreground service types** | Hosting runs in a declared foreground service. Validate the type with an early internal-track submission. |
| **Phones jetsam** | The WebView content process can die while the server runs. The host must reload, re-queue events, and fail pending calls. |

## The bridge contract

`shared/conformance/` vendors two files from the UI repo:

- `PROTOCOL.md` — wire envelopes, transports, lifecycle, errors. **Read first.**
- `bridge-v1.json` — the generated manifest: every channel, and which
  profile requires it.

151 channels exist. **iOS must implement 64, Android 67** — the rest are
desktop-only (WSL, the Minecraft client launcher, the installer) and gated
off by capability, so the UI never calls them.

```bash
node scripts/sync-contract.js ../homerun-app-ui        # refresh the vendored copies
node shared/conformance/check-coverage.js ios     ios/HomerunHost/BridgeRouter.swift
node shared/conformance/check-coverage.js android android/app/src/main/java/app/gethomerun/mobile/BridgeRouter.kt
```

The checker reads each router's dispatch table (between
`BRIDGE-CHANNELS-BEGIN`/`END` markers) and fails the build on any required
channel without a handler — because an unanswered invoke hangs a UI promise
forever.

Wire both into CI. When the contract gains a channel, that is exactly how you
want to find out.

## Status

The Android app boots and renders the shared UI. No server runs yet.

| Piece | State |
|---|---|
| Bridge contract, vendored | done |
| Conformance checker | done |
| `ServerBackend` (Swift, Kotlin) | interfaces defined |
| `homerun-pumpkin-ffi` | **implemented and tested** except the engine itself — see [docs/ffi.md](docs/ffi.md) |
| Pumpkin engine | not wired (fork not yet pinned) — `StubEngine` stands in |
| iOS host | not started |
| Android host | **M0–M3 core done** — see [docs/android-host.md](docs/android-host.md) and [docs/android-server-backend.md](docs/android-server-backend.md) |

Android runs on an emulator today: the WebView serves the shared bundle,
capabilities are injected at document start, the bridge round-trips, the
post-login handshake routes through to the dashboard, `homerun://` deep links
arrive on both paths, and **a server actually starts** — the Rust engine runs
in-process over JNI, streams console output, and stops cleanly.

`npm run conformance:android` reports **36 of 46** required channels handled.
The remaining 10 are storage figures, the SAF document pickers, region
latency, and the Bedrock version lookup. That report is the work queue.

The dashboard renders at desktop proportions on a phone. Making the shared UI
responsive is the largest piece of remaining mobile work and belongs to
[homerun-app-ui](https://github.com/hintjen/homerun-app-ui), not here.

The FFI crate has 36 passing tests covering the state machine, console
cursors, port pre-flight, crash capture, and the whole C surface — none of
which need a device or Pumpkin. `Engine` is the seam; a `StubEngine` stands
in so the failure paths are exercised now rather than discovered on a phone.

The prototype this builds on lives in the `Pumpkin` fork under `ios/`. It
already solved the hard embedding problems — FFI lifecycle, panic
containment, log capture, no-JIT wasm, WebView-process recovery — and those
lessons are folded into the interfaces here. What it lacks is the product:
config, console, connectivity, backgrounding.

## Implementation plans

The two platform tracks are built in parallel by different developers.

| Plan | For |
|---|---|
| [`plans/shared-milestones.md`](plans/shared-milestones.md) | **Read first.** Ownership, the shared M0–M5 milestones, cross-cutting rules |
| [`plans/ios.md`](plans/ios.md) | Swift host, Pumpkin engine wiring |
| [`plans/android.md`](plans/android.md) | Kotlin host, JVM backend, foreground service |
| [`plans/android-mod-loaders.md`](plans/android-mod-loaders.md) | Fabric/NeoForge/Forge, and the desktop mod resolver ported into the core |

## Getting started

```bash
npm install        # fetches and builds the shared UI bundle
npm run doctor     # what this machine can build, and how to fix the gaps
```

Then, per platform:

```bash
npm run build:android      # stages the UI + builds the .so into jniLibs
npm run build:ios          # macOS only
npm run test:rust          # 36 FFI tests, no device needed
```

To actually run the Android app:

```bash
npm run android:emulator   # start the AVD and wait for boot
npm run android:run        # build, install, launch, follow logs
```

Debug builds are inspectable from `chrome://inspect` on the host machine.

`npm run doctor` names every missing prerequisite and the exact command to
install it. Full detail: [`docs/building.md`](docs/building.md).

**iOS builds require macOS.** Android builds from any host.

See `CLAUDE.md` for how the pieces fit and what to build next.

## Related repositories

- **[homerun-app-ui](https://github.com/hintjen/homerun-app-ui)** — the shared
  UI and the bridge contract
- **[homerun](https://github.com/hintjen/homerun)** — desktop app, API, services
- **Pumpkin** (fork) — the Rust Minecraft server this embeds

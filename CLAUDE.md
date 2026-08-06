# Homerun mobile — working in this repo

iOS and Android hosts that run a Minecraft server on a phone. **No UI here** —
every screen is the shared bundle from
[`hintjen/homerun-app-ui`](https://github.com/hintjen/homerun-app-ui). This
repo is the platform half: WebView host, bridge implementation, server engines.

Read `shared/conformance/PROTOCOL.md` before touching bridge code.

## Where a change goes

| Change | Repo |
|---|---|
| A screen, component, style, anything visual | `homerun-app-ui` |
| A new bridge channel's **contract** | `homerun-app-ui` (`lib/bridge/`) |
| A bridge channel's **iOS/Android implementation** | here |
| Server lifecycle, engines, platform APIs | here |
| WSL, the desktop installer, the client launcher | `homerun/homerun-ui` — and it is desktop-only, so mobile never implements it |

## The two interfaces

**`bridge/v1`** — UI ↔ host. Frozen, versioned, additive-only. The contract
lives in the UI repo; `shared/conformance/` vendors the generated manifest and
the spec so mobile CI needs no checkout of it.

**`ServerBackend`** — host ↔ engine. `ios/HomerunHost/ServerBackend.swift`,
`android/.../ServerBackend.kt`. Implement the `native-server-*` channels
against this, never against a specific engine.

- iOS: `PumpkinBackend` only. The platform cannot spawn processes.
- Android: `PumpkinBackend` (JNI) **and** `JavaServerBackend` (real JVM).

The `native-server-*` naming is desktop-legacy — it meant "not WSL". Treat it
as "the server this device hosts". Do not rename in v1; three repos depend on
the strings.

## Conformance is the gate

```bash
node scripts/sync-contract.js ../homerun-app-ui
node shared/conformance/check-coverage.js ios     ios/HomerunHost/BridgeRouter.swift
node shared/conformance/check-coverage.js android android/app/src/main/java/app/gethomerun/mobile/BridgeRouter.kt
```

Of 151 channels, iOS must implement 65 and Android 67 (44 and 45 of those
are handlers; the rest are events the host emits). The checker reads the
router's own dispatch table between `BRIDGE-CHANNELS-BEGIN`/`END` markers —
keep those markers around the real table, not a duplicate list.

If a channel is missing, the fix is a handler, not an exclusion. **An
unanswered invoke hangs a UI promise forever** — that is the worst failure
mode in this protocol, and it looks like a frozen screen with no error.

## Non-negotiables

Learned from the prototype and the platforms; violating any of these produces
bugs that are miserable to diagnose.

1. **Never let the engine abort the process.** A taken port must be an error,
   not `process::exit`. On a phone that is the whole app disappearing.
2. **Never let a Rust panic cross the FFI boundary** — undefined behaviour.
   Every `extern "C"` fn wraps its body in `catch_unwind`.
3. **Start the server thread with ≥16 MB of stack.** The default 512 KB
   overflows inside the engine and dies with no panic report.
4. **No blanket call timeout.** Server start and modpack import legitimately
   run for minutes. Clear pending calls when the *page* dies, not on a timer.
5. **Serve the bundle from a custom scheme**, never `file://`. Module scripts
   from an opaque origin fail silently and you get a blank page.
6. **Assume the WebView process dies.** Reload, re-queue events, wait for a
   fresh `ready`, fail the old page's pending calls. Keep no per-page state.
7. **Capabilities are injected at document start.** The UI reads them
   synchronously and cannot await the host.
8. **One server runs at a time.** Enforce in the host and the engine.
9. **Android: executables must come from `nativeLibraryDir`.** API 29+ blocks
   exec from writable storage — the JRE ships in the APK.
10. **Emit `native-server-state-changed` on resume.** After a suspend the UI's
    cached state is stale and nothing else will correct it.

## Building

The FFI crate builds and tests **host-native on any OS**, including Windows —
36 tests, no device, no Pumpkin. Do that before reaching for a simulator.

```bash
cd rust/homerun-pumpkin-ffi
cargo test
cargo clippy --all-targets
```

Cross-compiling: Android targets work from any host with `cargo-ndk`; iOS
targets require macOS.

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-linux-android
cargo install cargo-ndk
```

The Pumpkin dependency is commented out in `Cargo.toml` until the fork's
library-mode patches are pinned. That is deliberate: everything except the
engine is implemented and tested behind the `Engine` trait, so the FFI
surface can be reviewed and exercised first. See `docs/ffi.md`.

## What to build next

Roughly in dependency order:

1. **Wire Pumpkin into `Engine`** — the rest of `homerun-pumpkin-ffi` is done
   and tested (`docs/ffi.md`). Pin the fork, implement the trait, swap the
   default in `server::host()`. Add stdout/stderr redirect once a real engine
   writes to fd 1.
2. **iOS host** — `WKWebView` on a custom scheme, `BridgeRouter` with the
   marker block, `PumpkinBackend` over the FFI. Evolve the prototype's
   `BridgeController`; its weak-handler proxy, ready handshake, event queue,
   and process-death recovery are all correct.
3. **Android host** — `WebViewAssetLoader`, `addJavascriptInterface` router
   (remember: binder thread — hop to main before touching the WebView),
   foreground service, `PumpkinBackend` via JNI.
4. **`JavaServerBackend`** — bundled JRE in jniLibs, port the desktop
   `nativeServerManager` semantics (launch args, graceful console `stop`,
   RCON, log pipe → cursor).
5. **Parity** — wireproxy via gomobile for reachability, the device
   WebSocket, perf sampling, player identity reporting.

Platform plans: `plans/ios.md`, `plans/android.md`, and `plans/shared-milestones.md`
(read that one first — it says who owns what). Overall phasing:
`homerun/plans/mobile-apps.md`.

## Conventions

- Match the surrounding style. Comments explain *why*, not what.
- Error messages in bridge responses are shown to players. Write them for a
  player, not a log.
- Prefer fixing something in the shared layer over fixing it twice per
  platform — that is the entire reason the bridge exists.

# Homerun mobile — working in this repo

iOS and Android hosts that run a Minecraft server on a phone. **No UI here** —
every screen is the shared bundle from
[`hintjen/homerun-app-ui`](https://github.com/hintjen/homerun-app-ui). This
repo is the platform half: WebView host, bridge implementation, server engines.

Read `shared/conformance/PROTOCOL.md` before touching bridge code.

## Skills in this repo

Invoke these rather than re-deriving them; each exists because the same thing
was worked out from scratch more than once.

| Skill | When |
|---|---|
| `android-emulator` | testing on a device: build, install, tap by screenshot, logcat, verifying a flow end to end |
| `ffi-abi-change` | adding or changing a `homerun_*` C export — seven touchpoints, most failing silently |
| `tests-that-bite` | writing a test, or working out why a green suite missed a real bug |

**Improve them as you go.** A skill that was vague, incomplete, or wrong is a
bug in the skill — fix it in the same commit as the work that revealed it,
while you still remember what was actually confusing. Add the trap you fell
into, the command that did not do what it claimed, the step that was missing.
If you learn something a skill *should* have told you, it goes in.

If you find yourself working something out that a **new** skill would cover —
a task that took real effort and would take the same effort again next time —
say so and offer to write it. Do not create one unasked.

The test of a good skill is whether the next session avoids the mistake you
just made. Small, specific corrections beat rewrites.

## Where a change goes

| Change | Repo |
|---|---|
| A screen, component, style, anything visual | `homerun-app-ui` |
| A new bridge channel's **contract** | `homerun-app-ui` (`lib/bridge/`) |
| A bridge channel's **iOS/Android implementation** | here |
| Server lifecycle, engines, platform APIs | here |
| WSL, the desktop installer, the client launcher | `homerun/homerun-ui` — and it is desktop-only, so mobile never implements it |

## Decisions in Rust, effects in the host

`rust/homerun-core` holds the decisions every Homerun app makes — what a
console line means, how much heap is safe, what order a launch runs in, what
an exit meant. It has no sockets, no processes, no async runtime; its only
dependencies are `serde` and `serde_json`. Hosts supply the effects and the
things only they can know (how much RAM this device has, which launcher may be
exec'd).

**The rule: if two platforms could answer a question differently, the answer
belongs in the core.** Two divergences prompted it and both were live before
it existed. When you find yourself writing a rule in Kotlin or Swift, stop and
ask whether the other platform will need the same rule — and whether it will
get it right.

There is a second axis, and it points the other way: the core is native, so
changing it needs a store release, while the UI bundle can ship over the air.
The UI is shared across all three apps too, so "shared" does not settle it. A
threshold someone will want to tune after launch may belong in the UI even
when the core could hold it — decide that deliberately rather than by habit.
See [`plans/ota-updates.md`](./plans/ota-updates.md).

`homerun-pumpkin-ffi` is the other half: the **supervisor**. It owns the
running server — the state machine, the console buffer, the stop ladder, the
crash capture, the sampling — for a linked engine and a child process alike.
Hosts wire it up; they do not reimplement it. `docs/shared-core.md` and
`docs/ffi.md` are the references.

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

`npm run conformance:ios` and `conformance:android` wrap those. Both pass
today: iOS requires 43 handlers (52 declared), Android 44. The checker reads
the router's own dispatch table between `BRIDGE-CHANNELS-BEGIN`/`END` markers —
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

Both Rust crates build and test **host-native on any OS**, including Windows —
237 core tests and 100 FFI tests in seconds, no device and no Pumpkin. Do that
before reaching for a simulator or a phone.

```bash
npm test        # core + FFI (with process-engine) + the ABI check
```

Cross-compiling: Android targets work from any host with `cargo-ndk`; iOS
targets require macOS.

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-linux-android
cargo install cargo-ndk
```

Every heavy dependency sits behind a feature that is **off by default** —
`pumpkin-engine`, `backup-engine` (iOS only), `process-engine` (Android and
desktop, never iOS). That is what keeps the suite device-free and seconds
long; the platform builds turn on what they need, see `scripts/targets.js`.
Everything is written against the `Engine` trait, so the supervisor cannot
tell a linked engine from a child process. See `docs/ffi.md`.

## Where things stand

Both hosts exist and both pass conformance. Android runs a real JVM server
end to end — jar cache, settings, tunnel, graceful stop, on-stop backup,
Insights — and is the platform to test on, since it is the one that can be
driven from this machine.

Known gaps, so you do not rediscover them:

- **Android's Pumpkin path is compiled but never runs.** `ServerHost` picks
  `JavaServerBackend` whenever a JRE is present, which is always. That code
  has an `Engine` impl and a metrics path nothing exercises here.
- **The arm64 slice has never run on hardware.** Incremental builds usually
  refresh only the ABI you are emulating; assume the other one is stale.
- **iOS Swift changes have often been written without a compiler.**
  `plans/ios-handoff.md` tracks which, and what to check first.
- **The desktop has no `homerun-core` binding.** Every core module has exactly
  one consumer today, so "shared" is still aspirational in one direction.

Platform plans: `plans/ios.md`, `plans/android.md`, and
`plans/shared-milestones.md` (read that one first — it says who owns what).
Overall phasing: `homerun/plans/mobile-apps.md`.

## Documentation

**Write the doc with the code, not after.** Each subsystem gets one file in
`docs/`, indexed from `docs/README.md`, in the house style — `## Overview`,
sections named after the file they document, `## File map`, `## Triage`.
`docs/ffi.md` is the worked example.

Which doc belongs to which milestone is in
`plans/shared-milestones.md` ("Documentation is part of the milestone").

Most of what matters here cannot be inferred from the source: which thread a
callback arrives on, why a workaround exists, what the OS does under memory
pressure. Write that down.

## Conventions

- Match the surrounding style. Comments explain *why*, not what.
- Error messages in bridge responses are shown to players. Write them for a
  player, not a log.
- Prefer fixing something in the shared layer over fixing it twice per
  platform — that is the entire reason the bridge exists.

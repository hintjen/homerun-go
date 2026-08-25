# Homerun Go — working in this repo

iOS and Android hosts that run a Minecraft server on a phone. **No UI here** —
every screen is a compiled bundle that `npm run ui` fetches from the CDN and
verifies against the key pinned in this tree. Its source
(`hintjen/homerun-app-ui`) is private and shared with the desktop app. This
repo is the platform half: WebView host, bridge implementation, server engines.

Read `shared/conformance/PROTOCOL.md` before touching bridge code.

## Skills in this repo

Invoke these rather than re-deriving them; each exists because the same thing
was worked out from scratch more than once.

| Skill | When |
|---|---|
| `on-device-build` | getting a build onto a phone, either platform: what to stage before Gradle/Xcode touches anything, and pointing a build at a backend that is not production |
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

`homerun-app-ui` and `homerun/homerun-ui` are private. A change that belongs
there is one a maintainer has to carry over — say so in the issue or PR
rather than working around it here.

## Decisions in Rust, effects in the host

`rust/homerun-core` holds the decisions every Homerun app makes — what a
console line means, how much heap is safe, what order a launch runs in, what
an exit meant. It has no sockets, no processes, no async runtime; its
dependencies are `serde`, `serde_json` and `ed25519-dalek` — the last one
argued at length in `bundle.rs`, because an OTA manifest's signature is the one
place a hand-rolled implementation still accepts every honest input while
quietly accepting forged ones too. Hosts supply the effects and the things only
they can know (how much RAM this device has, which launcher may be exec'd).

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
See `docs/ota-bundles.md`.

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

- iOS: `PumpkinBackend` only, with the engine linked in. The platform cannot
  spawn processes.
- Android: `PumpkinBackend` **and** `JavaServerBackend`, both child processes
  through the same supervisor. Which one a launch uses is the core's answer to
  the server's game type, not a property of the device.

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
today: iOS requires 57 handlers (66 declared), Android 58 of 58. The checker
reads the router's own dispatch table between `BRIDGE-CHANNELS-BEGIN`/`END`
markers — keep those markers around the real table, not a duplicate list.

Three more gates run beside them, all five in
`.github/workflows/conformance.yml`: `npm run test:host-revision` (a channel
answered without a ledger entry is one an over-the-air bundle can hang on),
`npm run test:capabilities` (Android transcribes its capability record by hand
and had already drifted, and it now also checks the bundle public key both
hosts transcribe), and `npm run test:ui-bundle` (`scripts/ui-bundle.js` decides
whether to trust a manifest off the CDN, and every check in it fails *open* if
it is wrong).

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
573 core tests and 142 FFI tests in seconds, no device and no Pumpkin. Do that
before reaching for a simulator or a phone.

```bash
npm test        # core + FFI (with process-engine), then the ABI, host-revision
                # and capability checks
```

The FFI crate's `cargo test` resolves the still-private `hintjen/Pumpkin`
git dependency (Cargo fetches optional deps even with their feature off), so
`npm run test:rust` needs access to it until that fork opens. Everything
else — `test:core` and all the gates — runs for anyone.

Cross-compiling: Android targets work from any host with `cargo-ndk`; iOS
targets require macOS.

**A build talks to production unless you say otherwise, and saying otherwise is
not one switch.** Two places hold an API URL — the host's build config and the
page's `localStorage` — and the page seeds itself from the host *only when its
key is empty*, so rebuilding with a different backend changes nothing for the
page until the app's data is wiped: `npm run android:run:staging:fresh`, or a
delete-and-reinstall on iOS. The switch is `--api` / `-PapiUrl` on Android and
`HOMERUN_API_URL` on iOS. Never conclude which backend is in play from the build
log — read the value out of the running page. `docs/building.md` § *Which backend
a build talks to*, and the `on-device-build` skill.

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-linux-android
cargo install cargo-ndk
```

Every heavy dependency sits behind a feature that is **off by default** —
`pumpkin-engine` (iOS only now; Android spawns the server instead of linking
it), `backup-engine` (iOS only), `process-engine` (Android and desktop, never
iOS). That is what keeps the suite device-free and seconds
long; the platform builds turn on what they need, see `scripts/targets.js`.
Everything is written against the `Engine` trait, so the supervisor cannot
tell a linked engine from a child process. See `docs/ffi.md`.

## Where things stand

Both hosts exist and both pass conformance. Android runs a real JVM server
end to end — jar cache, settings, tunnel, graceful stop, on-stop backup,
Insights — and is the platform to test a *server* on.

On a Mac, iOS is drivable from a terminal too: build, install, launch and read
the log without opening Xcode, which is how the device websocket was verified.
The `on-device-build` skill has that loop and the traps in it.

Known gaps, so you do not rediscover them:

- **Android runs Pumpkin as a child process, not linked.** `pumpkin-engine` is
  off for both Android targets; the server ships as `libpumpkin.so` from
  `rust/homerun-pumpkin-bin` and is supervised by the same `ProcessEngine` the
  JVM backend uses. `ServerHost` holds both backends and routes per launch on
  the game type, which the core decides (`minecraft.hosting.serves`). iOS still
  links the engine, because it cannot spawn one.
- **Bedrock is served by PowerNukkitX, and it has hosted a player.** A phone
  hosts a Bedrock server through `native-powernukkitx` — a Bedrock server
  written in Java, on the JVM already staged here. Proven on a Pixel 9 Pro XL:
  world generated, clean stop, and a Bedrock client joined **through the
  gateway** and was kicked from the UI — so RakNet survives the tunnel, which
  the plan called the milestone most likely to surprise. JNA/OSHI survives
  bionic too; it degrades rather than throwing.

  What that run cost was four console defects, because the byte stream was new
  to this repo — `[main]` being eaten by the ANSI stripper, a bare timestamp, a
  thread-name tag, and an operator list whose case made `/deop` a no-op
  forever. All fixed; `docs/android-bedrock.md` § *The console* and
  § *Operators* are the write-up, and the lesson is that the console is the
  part no host-native test can retire.

  Still open: nothing displays the Bedrock version the server announces, and
  the wizard shows a view distance the core then clamps.
  `docs/android-bedrock.md`.

- **arm64 builds and installs; no server has been started on it.** The whole
  payload — FFI, launcher, `libpumpkin.so` at 65 MB, restic, wireproxy, a JRE —
  is staged and extracted to `nativeLibraryDir` on a Pixel 9 Pro XL, and the
  host reports `engines: jvm=true pumpkin=true` there. What has not happened on
  hardware is a launch: no world, no stop ladder, no metrics. All of that was
  verified on the x86_64 emulator only.

  Incremental builds still refresh only the ABI you are building; assume the
  other one is stale.
- **iOS Swift changes have often been written without a compiler.** Treat an
  untested Swift path as unproven until it has met one. The device websocket
  is no longer one of them: it builds, runs on a simulator, and its socket is
  proven by `ios/wsprobe/`. What it has not seen is an account, the gateway,
  or a physical device.
- **The desktop has no `homerun-core` binding.** Every core module has exactly
  one consumer today, so "shared" is still aspirational in one direction.

Planning notes (`plans/…`) live in the private repository that also holds
the release pipeline; docs here sometimes cite them by name for the history
of a decision. The decision itself is always restated where it applies.

## Documentation

**Write the doc with the code, not after.** Each subsystem gets one file in
`docs/`, indexed from `docs/README.md`, in the house style — `## Overview`,
sections named after the file they document, `## File map`, `## Triage`.
`docs/ffi.md` is the worked example.

Most of what matters here cannot be inferred from the source: which thread a
callback arrives on, why a workaround exists, what the OS does under memory
pressure. Write that down.

## Conventions

- Match the surrounding style. Comments explain *why*, not what.
- Error messages in bridge responses are shown to players. Write them for a
  player, not a log.
- Prefer fixing something in the shared layer over fixing it twice per
  platform — that is the entire reason the bridge exists.

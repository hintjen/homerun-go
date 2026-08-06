# Building

## Overview

Two things have to exist before Xcode or Gradle can build an app: the
**shared UI bundle** staged into the platform's asset directory, and the
**Rust FFI** compiled for that platform's targets. The npm scripts here do
both; the app build itself stays in Xcode and Gradle where it belongs.

Nothing here is committed — bundles and native libraries are build output.

```
npm run doctor            what can this machine build, and what is missing
npm run build:ios         stage the UI + build the iOS static library
npm run build:android     stage the UI + build the Android shared library
```

## Check the machine first — `scripts/doctor.js`

```bash
npm run doctor            # both platforms
node scripts/doctor.js android
```

Prints a pass/fail line per prerequisite and the exact command to fix each
gap. Run it before anything else: the native toolchains fail unhelpfully —
a missing Rust target surfaces as a linker error naming a file you never
mentioned, and a missing NDK as one about `cc`.

| Requirement | Platform | Install |
|---|---|---|
| Rust + `~/.cargo/bin` on PATH | both | <https://rustup.rs> |
| Shared UI bundle | both | `npm install` |
| Xcode + `xcode-select --install` | iOS | App Store |
| XcodeGen | iOS | `brew install xcodegen` |
| Android NDK + `ANDROID_NDK_HOME` | Android | Android Studio → SDK Manager → NDK |
| `cargo-ndk` | Android | `cargo install cargo-ndk` |
| Rust targets | both | `rustup target add <triple>` |

**iOS can only be built on macOS.** Xcode's linker and SDKs have no
cross-platform equivalent. Android builds fine from any host.

## The shared UI — `scripts/build-ui.js`

```bash
npm run ui                # both platforms
npm run ui:ios
```

Re-resolves `homerun-app-ui`, lets npm rebuild the bundle via its `prepare`
hook, and copies the result into the platform's asset directory:

| Platform | Destination |
|---|---|
| iOS | `ios/HomerunHost/web/` |
| Android | `android/app/src/main/assets/web/` |

**A build ships whatever the UI is currently at.** The lockfile is rewritten
with the commit that was resolved, so a release is still traceable — commit
`package-lock.json` with it.

| Env | Effect |
|---|---|
| `HOMERUN_UI_DIR=<path to out/>` | Stage that build; no refresh. Working against a local UI checkout. |
| `HOMERUN_UI_NO_UPDATE=1` | Keep the pinned commit. Offline, or reproducing an old build. |

The destination is **replaced, not merged** — a stale file from an older UI
version would otherwise linger and get served.

> Editing this script? `npm update` does **not** refetch a git branch
> dependency; npm treats it as already satisfied. Re-installing the spec is
> what re-resolves the ref.

## The Rust FFI — `scripts/build-rust.js`

```bash
node scripts/build-rust.js ios
node scripts/build-rust.js android --debug
node scripts/build-rust.js host       # tests only, no artifact
```

| Target | Triple | Artifact lands in |
|---|---|---|
| `ios` | `aarch64-apple-ios` | `ios/HomerunHost/lib/` |
| `ios-sim` | `aarch64-apple-ios-sim` | `ios/HomerunHost/lib/sim/` |
| `android` | `aarch64-linux-android` | `android/app/src/main/jniLibs/arm64-v8a/` |
| `android-x86_64` | `x86_64-linux-android` | `android/app/src/main/jniLibs/x86_64/` |
| `host` | this machine | — (for `cargo test`) |

`scripts/targets.js` is the single table those paths come from, so the
scripts, the doctor, and this document cannot disagree.

**jniLibs is not a convention, it is a requirement.** Since API 29 Android
refuses to load or execute anything from writable storage, so native
libraries — and later the bundled JRE — must ship inside the APK and land in
`nativeLibraryDir`.

Release is the default. `--debug` exists for symbols, but debug builds are
enormous once a real engine is linked (~1.8 GB for the prototype); the
script says so rather than letting you sideload one by accident.

## Typical loops

**Change Rust, test it** — no device needed, and this is most of the FFI:

```bash
npm run test:rust     # 36 tests, host-native
```

**Change the UI, see it in the app:**

```bash
npm run ui:ios        # or ui:android
```
then rebuild in Xcode/Gradle. For a live loop, run `npm run dev` in the UI
repo and point the host at `http://localhost:3000`.

**Fresh clone:**

```bash
npm install
npm run doctor
npm run build:android      # or build:ios on a Mac
```

## Conformance

Independent of building, and worth wiring into CI early:

```bash
npm run conformance:ios
npm run conformance:android
```

Fails when a router lacks a handler for a channel its profile requires. An
unanswered invoke hangs a UI promise forever, so this is a gate, not a lint.

## File map

| File | Role |
|---|---|
| `scripts/targets.js` | Triples, artifact names, output paths — the single source |
| `scripts/build-ui.js` | Refresh + stage the shared UI bundle |
| `scripts/build-rust.js` | Per-target cargo builds, prerequisite checks, staging |
| `scripts/doctor.js` | What this machine can build |
| `scripts/sync-contract.js` | Refresh the vendored bridge contract |
| `package.json` | The npm entry points |

## Triage

**`cargo not found on PATH`** — Rust installs to `~/.cargo/bin`, which some
shells do not pick up. Add it, or open a new terminal after installing.

**`iOS device can only be built on macOS`** — expected on Windows/Linux.
Build Android locally and leave iOS to a Mac or CI.

**`Rust target … is not installed`** — the message includes the exact
`rustup target add` line.

**`cargo-ndk is not installed`** — `cargo install cargo-ndk`. If it is
installed and the build still fails on a missing linker, `ANDROID_NDK_HOME`
is probably unset; the script warns about that separately.

**`Build reported success but lib….a is not at …`** — the crate's
`crate-type` no longer matches the target. iOS needs `staticlib`, Android
needs `cdylib`; both are declared in `rust/homerun-pumpkin-ffi/Cargo.toml`.

**`No built UI bundle at …`** — run `npm install`, or the path in
`HOMERUN_UI_DIR` has no `index.html` (the marker for a real export). An
empty directory would otherwise stage silently and the app would show a
blank screen.

**App shows a blank screen** — the bundle is missing, or being served over
`file://`. See the host docs; module scripts from an opaque origin fail
silently.

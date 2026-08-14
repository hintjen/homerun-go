# Building

## Overview

Two things have to exist before Xcode or Gradle can build an app: the
**shared UI bundle** staged into the platform's asset directory, and the
**Rust FFI** compiled for that platform's targets. The npm scripts here do
both; the app build itself stays in Xcode and Gradle where it belongs.

Nothing here is committed — bundles and native libraries are build output.

```
npm run doctor              what can this machine build, and what is missing
npm run build:ios           stage the UI + build the iOS static library
npm run build:android       stage the UI + build the Android shared library
npm run build:android:release   everything an installable release needs
```

`build:android` stages what the *debug* loop needs and no more. A release
needs four more pieces, and every one of them fails open — the build
succeeds and the app is broken on a phone instead. See
[Building a release](#building-a-release).

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
| Go 1.26+ | both | `brew install go` (the wireproxy fork needs it) |
| gomobile | iOS | `go install golang.org/x/mobile/cmd/gomobile@latest && gomobile init` |
| `wireproxy-fork` checkout | both | clone as a sibling, or `HOMERUN_WIREPROXY_SRC` |
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

### The engine feature

Device builds pass `--features pumpkin-engine`, which links the real server.
The host build deliberately does not — that is what keeps `cargo test` at
about two seconds with no Pumpkin, no wasmtime and no device, and it is the
reason the whole FFI surface could be tested before the engine existed.

```bash
node scripts/build-rust.js ios            # with the engine
node scripts/build-rust.js ios --stub     # without it
```

`--stub` is for checking that the FFI surface still cross-compiles for a
target without waiting for the engine to build. The first build with the
engine pulls the pinned Pumpkin fork from GitHub and takes a few minutes.

## The tunnel — `scripts/build-wireproxy.js`

```bash
node scripts/build-wireproxy.js ios            # gomobile → xcframework
node scripts/build-wireproxy.js android        # go build → jniLibs
```

Android gets an executable staged into `jniLibs`; iOS gets
`WireproxyIOS.xcframework` (device + simulator slices, ~32 MB) staged into
`ios/HomerunHost/lib/`, because the platform cannot spawn a binary and the
tunnel has to run in-process.

**Run it before `xcodegen generate`** — the project references the staged
framework.

The Go binding lives in `go/wireproxy-ios/` and reaches the fork through a
`go.work` the script regenerates each build. That workspace is generated
rather than committed because the fork's location is configurable, and it
pins gvisor: the fork's wireguard-go is on a 2023 upstream commit whose
netstack does not compile against anything newer.

> Do not run `go work sync` there. It writes the resolved versions back into
> the fork's own `go.mod` files — a change to a different repository, and
> precisely the gvisor upgrade that breaks the build.

## Two rules the Go and JRE binaries have to obey

Both were learned by shipping a build that installed, launched, and could not
host. Neither is visible from the code.

### cgo is mandatory on every Android target

`build-restic.js` and `build-wireproxy.js` set `cgo: true` for **both** ABIs,
so both need the NDK. That is not about linking. It is about **DNS**.

Android ships no `/etc/resolv.conf`. With cgo off, Go uses its own resolver,
finds no nameservers, and falls back to `127.0.0.1:53` — where nothing is
listening. Every hostname lookup fails with `connection refused`, so restic
cannot reach the backup repository and wireproxy cannot reach the gateway.

On a device that does not look like an error. A server start restores the world
from backup before launching the JVM, so restic retries with backoff for ever:
the jar downloads, the card says *Starting up…*, and nothing further happens.
No crash, no exception, no report — `crash::report` reads a server's console
output, and there is no server.

**The emulator cannot catch this.** `android-x86_64` has always needed cgo for
an unrelated reason — Go cannot link android/amd64 internally — so the only
configuration ever exercised was the one that never ships, while arm64, which
does ship, was never tested. If you are ever tempted to set cgo off on arm64
because "Go links it internally and needs no NDK": that is true, and it is
exactly how this happened.

### Everything must be 16 KB page aligned

New 64-bit devices run 16 KB pages and their linker refuses a library aligned
more coarsely. Play has required support for targetSdk 35+ since 1 November
2025 (extended to 31 May 2026), so this blocks a release as well as a device.

The NDK emits 16 KB by default and Go's own linker emits 64 KB, so this is
mostly free — with one trap. **Turning cgo on hands linking to the NDK's `ld`,
whose default is 4 KB**, which is why both Go scripts pass
`-extldflags=-Wl,-z,max-page-size=16384`. The two settings are one change; a
future edit that keeps cgo and drops the flag re-breaks this silently.

Three checks enforce it, and each has caught something real:

| Where | Covers |
|---|---|
| `stage-jre.py` | every `.so` staged into `assets/jre/`; refuses to stage otherwise |
| `build-restic.js`, `build-wireproxy.js` | the binary just built |
| `third_party/libandroid-spawn/` | Termux publishes only a 4 KB build of a library `libjvm.so` has a hard `DT_NEEDED` on, so it is compiled from source here |

> Before blaming page alignment for a device failure, run
> `adb shell getconf PAGE_SIZE`. Most phones still answer `4096`, and a 4 KB
> library loads perfectly well on them — the alignment work is a Play
> requirement and a future-device fix, not an explanation for today's bug.

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

## Building a release

`build:android` is the debug loop's staging step. It covers the UI bundle,
the FFI library and wireproxy — and stops there, which is right for the
emulator and wrong for anything you would upload.

```bash
npm run build:android:release
```

That adds the four pieces a release needs and a debug build does not:

| Piece | Without it |
|---|---|
| `rust:java-launcher` (`libjavabin.so`) | `JavaRuntime.isAvailable` is false, so `ServerHost` picks a backend that cannot start a Java server |
| `restic:android` (`librestic.so`) | backups silently no-op while `HostCapabilities` still advertises them — the app offers a feature that does nothing |
| `jre:android` | the app installs and can never host anything |
| — and it stages the **arm64** JRE, not the emulator's | an APK that runs on no phone |

Every one of those fails open: the build succeeds and the failure surfaces
on a device, a long way from the cause. That is why this is a separate
script rather than a note in a checklist.

The Gradle build then wants the ABI named explicitly, because the staged
JRE is architecture-specific and a release must ship exactly one:

```bash
./gradlew :app:bundleRelease -Pabi=arm64-v8a -PversionCode=N -PversionName=X.Y.Z
```

`bundleRelease` produces the `.aab` Play wants; `assembleRelease` produces an
APK, which is useful for sideloading and is not uploadable for a new app. Play
rejects a `versionCode` it has already seen, so bump it every upload — and a
`versionName` must be numeric and period-separated, since Play rejects
`0.2.0-beta` even though Android accepts it.

**A release build needs the NDK, including for arm64.** That used to be false
and the note here used to say so; see the cgo rule above for why it changed.

Signing comes from `android/keystore.properties` (gitignored). Without it the
build still completes and the artifact is **unsigned** — deliberate, so CI
smoke builds and local audit builds work without a copy of the key, but it
means "unsigned" has to be caught somewhere. `verifyReleaseConfig` warns, and
`publish-android.yml` runs `jarsigner -verify` before it uploads.

Prove what you built before uploading it, rather than after Play rejects it:

```bash
jarsigner -verify app/build/outputs/bundle/release/app-release.aab
unzip -l app/build/outputs/bundle/release/app-release.aab | grep base/lib/
```

The second should list exactly one ABI. Two means the `-Pabi` flag was lost,
and half the payload will not match the one JRE in `assets/`.

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

**A server downloads its jar and then nothing happens, on a device** — almost
certainly DNS. Check with the app running:

```bash
adb -s <serial> logcat --pid=$(adb -s <serial> shell pidof app.gethomerun.mobile)
```

`lookup … on [::1]:53: connection refused` from `HomerunBackup` means a Go
binary was built without cgo — see the cgo rule above. Note the tag: filtering
logcat on `HomerunJava` or `HomerunHost` misses this entirely, because the line
comes from restic.

A single component can be tested without reinstalling anything. The staged
binaries are executable, so push one and run it:

```bash
adb push android/app/src/main/jniLibs/arm64-v8a/librestic.so /data/local/tmp/restic
adb shell "chmod 755 /data/local/tmp/restic && RESTIC_PASSWORD=x \
  /data/local/tmp/restic cat config -r rest:https://backups.gethomerun.app/nope/"
```

`401 Unauthorized` is the healthy answer — DNS resolved, TLS completed, the
server replied. A `lookup` error is the broken one.

**`No aarch64-linux-android26-clang in the NDK`** — every Android target needs
the NDK now, not just the emulator's. Set `ANDROID_NDK_HOME`, or install it
from Android Studio → SDK Manager → NDK.

**`N staged libraries cannot load on a 16 KB-page device`** — `stage-jre.py`
refusing to stage a runtime that would fail to load. The message names the
files; rebuild them with `-Wl,-z,max-page-size=16384`. This is a real refusal,
not a warning to work around.

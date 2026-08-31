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
| `wireproxy-fork` checkout | both | clone as a sibling, or `HOMERUN_WIREPROXY_SRC`; must be at the revision in `scripts/wireproxy.rev` (`npm run doctor` says which) |
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
| `HOMERUN_UI_BUNDLE=<id>` | Stage that published bundle from the CDN. |
| `HOMERUN_UI_CHANNEL=<channel>` | Which channel's newest to take. `stable`. |
| `HOMERUN_UI_NO_UPDATE=1` | Keep the pinned commit. Offline, or reproducing an old build. |

The destination is **replaced, not merged** — a stale file from an older UI
version would otherwise linger and get served.

### Four sources, first match wins

| Set | Source |
|---|---|
| `HOMERUN_UI_DIR` | that directory |
| `HOMERUN_UI_BUNDLE` | that published bundle, from the CDN |
| `homerun-app-ui` in `package.json` | `npm install`, then its `out/` — checkouts that add the private UI dependency |
| otherwise | the newest published bundle for the channel — **this repo** |

This repo carries no UI dependency: a checkout builds against the *compiled*
bundle. Every published bundle is a public CloudFront object, and its manifest
is signed, so a build can prove what it downloaded with no credential at all.
The release pipeline adds the private `homerun-app-ui` dependency and builds
the UI from source instead — the third row.

Nothing from the CDN is trusted on its face. `scripts/ui-bundle.js` verifies
the manifest's Ed25519 signature against `scripts/bundle-key.js`, refuses a
`minHost` above this checkout's own host revision, checks the archive against
the manifest's `sha256`, and unpacks with the same ceilings and Zip Slip guard
`BundleUpdater` applies on a device. `npm run test:ui-bundle` exercises each of
those against an input that should trip it. `docs/ota-bundles.md`
§ *Building against a published bundle*.

**A CDN build stages someone else's analytics.** A stable bundle has the
production PostHog key compiled into it at `next build`, so a build pointed at
staging still reports to the production project. Cosmetic, and it cannot be
fixed from this side — the key is inside the compiled artefact. See *Which
backend a build talks to* below for the rest of that story.

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

### The device websocket feature needs `cmake`

Both phone targets build with `device-ws`, which pulls in `aws-lc-rs` for the
crypto provider rustls and the ACME client share. `aws-lc-sys` compiles C, and
its build script needs **cmake on the machine doing the build** — `brew install
cmake` on a Mac. Without it the failure is a build-script error naming
`aws-lc-sys` rather than anything in this repo, which reads like a broken
dependency instead of a missing tool.

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
rather than committed because the fork's location is configurable. wireguard-go
and gvisor come from the module proxy at whichever version is highest between
the fork's `go.mod` and the binding's — the fork stopped vendoring wireguard-go
when it became a real fork of upstream (its `FORK.md` has the history).

> Do not run `go work sync` there. It writes the resolved versions back into
> the fork's own `go.mod` — a change to a different repository.

**The fork checkout must be at the revision in `scripts/wireproxy.rev`**, on
both platforms. The fork's `main` merges upstream on a schedule, so it moves
without any commit here; the pin is what keeps a store build from shipping
whatever landed overnight. `build-wireproxy.js` refuses any other revision
(`HOMERUN_WIREPROXY_ALLOW_UNPINNED=1` for iterating on the fork itself), and
bumping it is a reviewed commit here, the same as the Pumpkin rev — with a
run on a phone if wireguard-go or gvisor moved.

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
| `stage-jre.py` | every `.so` staged into each `jre-<major>/`, checked per runtime; refuses to stage otherwise. On-demand majors stage into their feature module (`android/jre21/`), not the app — see `ON_DEMAND_JAVA` |
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

## Which backend a build talks to

Production, unless you say otherwise. Two places decide, and they are not the
same place — which is the whole reason this section exists.

| | Where the value lives | Default |
|---|---|---|
| **The host** | `BuildConfig.API_URL` (Android) / `HostStore.apiURL` (iOS) | `https://api.gethomerun.app` on Android; **nothing** on iOS |
| **The page** | `localStorage.apiUrl` | seeded from the host on first run, else the compiled-in production default |

The page's copy is the one that matters for almost everything — registration,
login, server settings, every `clientApi` call. The host's copy is for what the
host does itself: the device registration, the report tokens, the OTA bundle
check.

### Android

```bash
node scripts/android-app.js run --api https://api.fractalnetworks.co --fresh
npm run android:run:staging          # the same, without the wipe
npm run android:run:staging:fresh
```

`--api` is shorthand for `-PapiUrl=…`, which sets `BuildConfig.API_URL`.

**`--fresh` is not optional the first time.** The page seeds `localStorage.apiUrl`
from the host's value **only when the key is empty** (`pages/index.tsx`), so a
hand-picked backend survives every remount. A rebuild with a different `--api`
therefore changes nothing for the page: the build succeeds and the app keeps
talking to the old backend. `--fresh` wipes the app's data so the seed runs
again. The script prints what it built for, and warns when the device has
something else stored.

### iOS

`HOMERUN_API_URL` is the build setting, defaulted in `ios/project.yml` and
overridable per build:

```bash
xcodebuild -project ios/Homerun.xcodeproj -scheme Homerun \
  HOMERUN_API_URL=https://api.fractalnetworks.co
```

In Xcode itself, set it on the scheme or edit the value in `project.yml` before
`xcodegen generate`.

It reaches the app as the `HomerunAPIURL` key in `Info.plist`, and
`HostStore.apiURL` returns it **only when the page has not stored one** — so a
backend picked inside the app outranks the build, which is what somebody pointing
the app at a laptop expects across a relaunch.

The same first-run rule as Android therefore applies: moving a device that has
already run means clearing its data, or the stored value wins. Delete and
reinstall the app, or clear the `apiUrl` key in Safari's Web Inspector.

Two values are treated as "nothing was set", so a missing setting degrades to the
page's own default rather than to a URL nothing answers: an empty string, which is
what Xcode substitutes for an undefined setting, and a literal unsubstituted
`$(…)`.

**This was the one real asymmetry with Android and it is now closed.** Before it,
`HostStore.apiURL` was written only by the page, so a fresh install had nothing,
`get-initial-config` omitted `apiUrl` entirely, the page logged
`Initial API URL not provided by main process`, and iOS always fell back to
production — reachable only by editing `localStorage` by hand.

### Reading the value the page actually holds

Do not infer it. Both times this has gone wrong, the inference was the problem.
On Android, over the WebView debugger (debug builds enable it):

```bash
PID=$(adb shell pidof app.gethomerun.mobile.debug | tr -d '\r')
adb forward tcp:9222 localabstract:webview_devtools_remote_$PID
curl -s http://localhost:9222/json | grep webSocketDebuggerUrl
```

then `Runtime.evaluate` `localStorage.getItem('apiUrl')` over that socket. On iOS
the same answer comes from Safari → Develop → the device → the WebView.

To ask the *host* instead, invoke `get-initial-config` across the bridge and read
its `apiUrl`. When the two disagree, the page wins.

### Two bugs that made this harder than it should have been

Both fixed, and both worth knowing because they explain the shape above:

- **`clientApi.getApiUrl()` used to persist its own default.** It is called at
  startup by the token refresh, before the seeding effect runs, so production
  landed in `localStorage` and the seed was skipped — silently, because the
  "not provided by main process" warning lives inside the same `if`. A reader now
  returns the fallback without writing it.
- **`handleFailedRefresh()` called `localStorage.clear()`.** On any fresh install
  the refresh fails, so this ran and wiped the seeded `apiUrl` — and because the
  seed only fires on an empty key, that made it unrecoverable for the session.
  Signing out now forgets credentials and keeps settings.

Either one alone made `--api` useless on every device, which is why the flag
looked broken rather than the page.

## Which UI a build runs, and turning updates off

A build does not necessarily run the UI it was built with. `BundleStore` serves
`files/ui/current` — a bundle fetched over the air — in preference to the
`assets/web` staged into the binary, deliberately: that is the whole point of
`docs/ota-bundles.md`. Since 2026-08-20 a bundle that arrives also applies
**immediately** rather than at the next launch, so a build can move onto a
different UI mid-session.

That is right for a user and wrong for a session whose purpose is the UI you
just staged. One flag per platform turns it off:

```bash
npm run android:run -- --no-ota      # shorthand for -PotaUpdates=off
xcodebuild … HOMERUN_OTA_UPDATES=0   # iOS, defaulted in ios/project.yml
```

Off means **ignore over-the-air bundles entirely**, not merely "do not fetch":
nothing is downloaded, and a bundle already on disk is neither promoted nor
served. So there is nothing to delete first, and nothing is deleted — a later
build without the flag carries on exactly where it left off. Both hosts say
`over-the-air updates are off in this build` once per launch, on
`HomerunBundle` / `HostLog.bundle`.

**On by default, including for debug.** The update path is only ever exercised
on a debug build, so defaulting it off in development would mean nobody sees it
work until a release. And a *release* built with it off would look completely
healthy while silently never updating again — every shared-UI fix would need
another store release — so Gradle's `verifyReleaseConfig` refuses one. iOS has
no equivalent gate; the setting lives in `project.yml` and is not something a
release build should be passing.

There is no "off" spelled as an empty signing key, though both hosts do treat a
blank `BUNDLE_PUBLIC_KEY` that way. `prop()` falls back to the compiled-in
default for a blank `-P` override and the hex `require` rejects an empty one, so
`-PbundlePublicKey=` never disabled anything — it just looked like it had.

To ask a running device which UI it is on rather than guessing:

```bash
adb logcat -d -s HomerunBundle:* | tail -3
#  serving the shipped bundle   <- the one in the APK
#  serving bundle 2026-08-13.2  <- one from the CDN
```

## Push credentials

Push is Firebase Cloud Messaging on both platforms. Every file it needs is a
**per-environment build input, and none of them are committed** — the same
shape as the section above, and for the same reason: staging and production
are two separate Firebase projects, and a staging build wired to the
production project is a real user's phone buzzing with test data.

The projects are `homerun-go-staging` and `homerun-go-prod`. Each registers
the same two apps, `app.gethomerun.mobile` and `app.gethomerun.ios`.

| File | Where it goes | Secret? |
|---|---|---|
| `google-services.json` | `android/app/`, per build type | No — but per environment |
| `GoogleService-Info.plist` | `ios/HomerunHost/`, per configuration | No — but per environment |
| `AuthKey_<KeyID>.p8` | uploaded to Firebase; **never into this repo** | Yes |
| `*-firebase-adminsdk-*.json` | the API's secret store; **never into this repo** | Yes |

`.gitignore` covers all four patterns. The two marked secret are private keys:
the service-account JSON signs sends *as the backend*, so holding one is
holding the ability to push to every user of that project, and Apple will not
re-issue a `.p8` — it downloads once and never again.

The service-account JSON is a **backend** credential and does not belong in
this repo at all. It reaches the API as `FCM_SERVICE_ACCOUNT_JSON`.

### The `.p8` is not per Firebase project

The axis that trips people is the APNs key's environment, which is **Apple's
build environment** — a debug build on a device is sandbox; TestFlight and the
App Store are production — and it has nothing to do with which backend or
which Firebase project a build talks to.

So one sandbox key and one production key, and **both go into both projects**:

```
homerun-go-staging   development slot: sandbox key
                     production  slot: production key
homerun-go-prod      development slot: sandbox key
                     production  slot: production key
```

Firebase Console → Project settings → Cloud Messaging → Apple app
configuration → APNs authentication key, which has a slot for each. Each
upload wants the `.p8`, its Key ID (the part after `AuthKey_` in the
filename), and the team id `35DS8JGY4Y`.

A key uploaded into the wrong slot fails at send time as `BadDeviceToken`,
which reads like a bad *token* rather than a bad key — budget an afternoon if
this is got wrong. The environment is chosen when the key is created in
Apple's portal and **cannot be changed afterwards**.

### How the Android build consumes them

`stageGoogleServices` (app/build.gradle.kts) copies the right JSON into
`app/google-services.json` **by backend, not by build type** — it follows
`-PapiUrl` exactly as the API URL does, because the pairing that matters is
app Firebase project ↔ backend FCM credential: a debug build against the
production API with a staging `google-services.json` mints tokens the prod
backend can never send to (`SENDER_ID_MISMATCH`).

**Each Firebase project must register BOTH Android package names** —
`app.gethomerun.mobile` *and* `app.gethomerun.mobile.debug` (the debug build
type appends `.debug`). The google-services plugin fails the debug build with
`No matching client found` when the second one is missing; re-download the
JSON after adding it, and the one file then covers both build types.

The host side is [`PushMessaging.kt`] plus the three `push:*` handlers in the
router (bridge host revision 9); the token→API registration lives in the
shared UI (`lib/push.ts` in `homerun-app-ui`).

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

**The last row used to fail open in the debug loop too.** `verifyJavaRuntime`
compares the staged runtime's own `OS_ARCH` against the ABI being built and
refuses a mismatch — but only when `-Pabi` is passed, and `scripts/android-app.js`
never passed it. So `npm run android:install` after
`npm run jre:android-x86_64` produced an APK that installed on a phone, launched,
showed every screen, and could not host a thing; the first sign was a `dlopen`
failure reading `is for EM_X86_64 (62) instead of EM_AARCH64 (183)`, deep in a
server log. The script already knew the device's ABI — it resolves one to decide
which native libraries to rebuild — so it now passes the same answer to Gradle,
and the check fires in the loop where emulator and phone actually alternate.

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
the release pipeline runs `jarsigner -verify` before it uploads.

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

**`failed to authenticate when downloading repository`, naming no repository
you recognise** — cargo's built-in git client could not reach a git
dependency. Every one of them is public now (the Pumpkin, rustic and wireproxy
forks all went public for the repo split), so this should not happen on a
fresh machine any more; if it does, `[net] git-fetch-with-cli = true` in
`~/.cargo/config.toml` hands the fetch to the system git and whatever
credentials it holds.

**`Undefined symbols … ___chkstk_darwin` linking for iOS** — the deployment
target is unset, so rustc linked against iOS 10 while the SDK compiled
`aws-lc-sys`'s C for the current one. `scripts/build-rust.js` sets
`IPHONEOS_DEPLOYMENT_TARGET` from `targets.js`, so this only appears when
calling `cargo build --target aarch64-apple-ios` by hand. Use the script, or
export it yourself.

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

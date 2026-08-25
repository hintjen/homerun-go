# Homerun Go

The iOS and Android hosts for [Homerun](https://gethomerun.app) — run a
Minecraft server on your phone.

This repo is the **platform half** of the mobile app: the WebView host, the
bridge implementation, the Rust core and supervisor, and the server engines.
It contains no UI. Every screen is a compiled web bundle, fetched at build
time from Homerun's CDN and verified against a signing key pinned in this
tree — see *The UI bundle* below.

**Licence:** the code in this repository is [GPL-3.0-only](LICENSE). The UI
bundle is a separate, proprietary work; the GPL covers the host, not the
bundle it downloads. Contributions are accepted under the [CLA](CLA.md),
which a bot will ask you to sign on your first pull request.

**Releases** (App Store, Google Play, and over-the-air UI updates) are built
from tags of this repository by a private release pipeline that holds the
store credentials and signing keys. Nothing in this repo can publish
anything. References to `plans/…` you may find in the docs point at internal
planning notes that live alongside that pipeline, not here.

```
homerun-go/
├── ios/       Swift host — WKWebView, bridge router, PumpkinBackend
├── android/   Kotlin host — WebView, bridge router, Pumpkin + JVM backends
├── rust/      homerun-core (decisions) + homerun-pumpkin-ffi (the supervisor)
├── go/        wireproxy-ios — the tunnel, built as a library for iOS
├── shared/    the vendored bridge contract + conformance checker
├── scripts/   the build system — staging the UI, cross-compiling, the gates
└── docs/      one file per subsystem, indexed from docs/README.md
```

## Architecture

```
        UI bundle  (static web bundle, embedded in the app, OTA-updatable)
                │
                │  bridge/v1  — JSON over the platform's WebView channel
                ▼
   ┌─────────────────────────┐   ┌─────────────────────────┐
   │  iOS host (Swift)       │   │  Android host (Kotlin)  │
   │  WKWebView + router     │   │  WebView + router       │
   └───────────┬─────────────┘   └───────────┬─────────────┘
               │  ServerBackend              │  ServerBackend
               ▼                             ▼
        PumpkinBackend             PumpkinBackend │ JavaServerBackend
        (Rust, linked in)          (child process) │ (bundled JRE)
```

Two abstraction layers, and they do different jobs:

- **`bridge/v1`** separates the UI from every host. Frozen, versioned,
  additive-only; spec in `shared/conformance/PROTOCOL.md` — read it before
  touching bridge code.
- **`ServerBackend`** separates the host from the engine
  (`ios/HomerunHost/ServerBackend.swift`, `android/.../ServerBackend.kt`), so
  the `native-server-*` channels are wired once no matter what runs
  underneath. iOS links the Pumpkin engine in-process (it cannot spawn
  processes); Android supervises Pumpkin and JVM servers as child processes
  through the same Rust supervisor.

`rust/homerun-core` holds the decisions every host makes — what a console
line means, how much heap is safe, what an exit meant — with no sockets, no
processes, no async runtime. Hosts supply the effects. `docs/shared-core.md`
and `docs/ffi.md` are the references.

## The UI bundle

`npm run ui` fetches the current bundle's signed manifest from
`cdn.gethomerun.app`, verifies its Ed25519 signature against the public key
pinned in this tree, checks the archive's digest, and stages it into both
hosts — the same verification a device applies to an over-the-air update.
Pin a specific bundle with `HOMERUN_UI_BUNDLE=<id>`, or point at a local
tree with `HOMERUN_UI_DIR`. `scripts/check-ui-bundle.js` proves every one of
those guards still refuses a bad input; it runs in CI on every push.

The bundle's *source* is not in this repository and is not open source. The
bridge contract it speaks is vendored in `shared/conformance/`, so the
conformance gates run with no UI checkout at all.

## Building and testing

```bash
npm install        # no private dependencies; installs nothing of note
npm run doctor     # what this machine can build, and how to fix the gaps
npm test           # the Rust suites + the ABI, revision, capability and bundle gates
```

Both Rust crates build and test host-native on any OS, including Windows —
hundreds of tests in seconds, no device needed. Two of the git dependencies
are **private for now** while their forks are prepared for release:
`hintjen/Pumpkin` (the server engine) and `hintjen/wireproxy-fork` (the
tunnel). Until they open, `npm run test:rust` and full platform builds need
access to them; `npm run test:core` and everything else runs for anyone.

Per platform:

```bash
npm run build:android      # stages the UI + builds the native pieces into jniLibs
npm run build:ios          # macOS only
npm run android:emulator   # start the AVD and wait for boot
npm run android:run        # build, install, launch, follow logs
```

Debug builds are inspectable from `chrome://inspect`. Full detail:
[`docs/building.md`](docs/building.md) — including *Which backend a build
talks to*, which is worth reading before concluding anything from a device.

## Conformance is the gate

```bash
npm run conformance:ios
npm run conformance:android
```

The checker reads each router's dispatch table and fails on any required
channel without a handler, because **an unanswered invoke hangs a UI promise
forever** — on a phone that looks like a frozen screen with no error. Both
hosts pass today: iOS answers 57 required channels (66 declared), Android 58
of 58. Beside coverage, CI checks the host revision ledger, capability
parity, the pinned bundle key, and the UI-bundle guards — see
`.github/workflows/conformance.yml` for why each exists.

## Status

Both hosts exist and both pass conformance. Android runs a real JVM server
end to end on hardware — world, tunnel, graceful stop, on-stop backup — and
hosts Bedrock through PowerNukkitX; a Bedrock client has joined through the
gateway. iOS links the Pumpkin engine and is drivable from a terminal on a
Mac. `docs/README.md` indexes the write-ups, including the gaps.

## Related repositories

- **[homerun](https://github.com/hintjen/homerun)** — desktop app, API, services
- **hintjen/Pumpkin** — the Rust Minecraft server this embeds (private for now)
- **hintjen/wireproxy-fork** — the tunnel (private for now)

See `CLAUDE.md` for how the pieces fit and the house rules for working here.

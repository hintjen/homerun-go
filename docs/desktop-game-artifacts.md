# Desktop engine artifacts

## Overview

Homerun Desktop downloads a descriptor runner at launch and a core Node addon
at build time. This repository builds them; the monorepo owns signing and S3
publication. `scripts/publish-desktop-artifacts.js` prepares manifests and
prints commands. It never uploads anything.

## `scripts/targets.js`: Windows builds

Run on Windows x64 with MSVC, Rust and Node installed:

```text
npm run rust:game-runner
npm run rust:core-node
npm run test:core-node
```

Both build release binaries for `x86_64-pc-windows-msvc` into `dist/desktop/`.
The runner is `homerun-game.exe`, built with the static CRT and without linking
Pumpkin. The addon is `homerun_core.node`. Its existing target is unchanged
apart from source identity injection. Keep the addon smoke test in the Windows
release job: a Rust compilation alone does not prove Node can load it.

## `scripts/publish-desktop-artifacts.js`: sign, hash, publish

Sign the final binaries **before** preparing manifests. Any signing or other
byte change after preparation invalidates the digest and build ID. For each
artifact, `build` is the first twelve lowercase hex characters of SHA-256 of
the final file; `sha256` is the full digest; `size` is its byte length; `url`
points to an immutable name. The runner's protocol `ready.build` uses the same
rule on its own executable.

```text
node scripts/publish-desktop-artifacts.js --only game-runner
node scripts/publish-desktop-artifacts.js --only core-node
```

With no arguments, preparation requires all three artifacts including Pumpkin.
Each invocation validates all its inputs before writing any manifests. Upload
all immutable binaries before changing the corresponding `latest.json` files.

S3 base: `s3://fractal-homerun/homerun-desktop`. Public base:
`https://fractal-homerun.s3.amazonaws.com/homerun-desktop`.

| Artifact | Immutable key, relative to base | Manifest key | Local manifest |
|---|---|---|---|
| Runner | `game-runner/homerun-game-<build>.exe` | `game-runner/latest.json` | `game-runner-latest.json` |
| Core addon | `core/homerun-core-<build>.node` | `core/latest.json` | `core-latest.json` |
| Pumpkin | `pumpkin/homerun-desktop-minecraft-pumpkin-<build>.exe` | `pumpkin/latest.json` | `pumpkin-latest.json` |

The runner adds `version` (Cargo package version) and `protocol: 1`. The addon
adds `version`, `abi`, and `sourceBuild`, read from the actual locally built
addon. Pumpkin retains its existing `rev` field and URL layout. The script
also prints a compatibility copy to `assets/homerun_core.node` for older
desktop build scripts. New clients should resolve the immutable addon URL and
verify its SHA-256, rather than treating that mutable alias as a pinned input.

## `rust/homerun-core-node`: identity exports

The addon exports `coreVersion()`, `coreAbiVersion()`, and `coreBuildId()`.
The first is Cargo's package version; the second starts at **1** and describes
the Node addon contract, independently of the mobile FFI ABI version. Bump it
when the Node contract changes incompatibly.

`coreBuildId()` is a source identity, not the binary digest: embedding a file's
own hash would be recursive. `build-rust.js` supplies the Git commit, with
`-dirty` for local edits. CI can supply `HOMERUN_CORE_BUILD_ID` explicitly; it
must be nonempty and contain only ASCII letters, digits, dots, underscores or
hyphens. Direct Cargo builds fall back to Git HEAD or `unknown`. The manifest's
`sourceBuild` records this value; its separate `build` identifies signed bytes.

## Remaining monorepo integration

`publish-desktop-artifacts.yml` must build the runner and addon from the selected
engine revision, sign them, run the addon smoke test, prepare the manifests,
upload immutable files, then publish the manifests with `Cache-Control:
no-cache`. Retain existing Pumpkin publication. No release or upload is done
by these engine changes.

`download-assets.js` must consume the addon manifest and verify its SHA-256
before staging it. The descriptor runner host already expects
`game-runner/latest.json`. Test that downloader and the Electron lifecycle with
the real Windows runner before calling the integration complete. Vendor-game
onboarding, licence acceptance and full save/gateway verification are separate
remaining work; see [the runner's limitations](./game-runner.md).

## Triage

**Missing addon identity exports:** an older `.node` is staged. Rebuild the
addon from this branch; do not fill in guessed metadata in the manifest.

**Download hash or ready.build mismatch:** regenerate manifests after signing,
and confirm publication did not mix binaries from different builds.

**Source identity ends in -dirty:** the local source tree had changes. Release
from a committed checkout and record the chosen engine revision in CI.

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
Pumpkin. The addon is `homerun_core.node`, also built with the static CRT as on
main. Both builds verify their Windows imports. Keep the addon smoke test in the Windows
release job: a Rust compilation alone does not prove Node can load it.

## `scripts/publish-desktop-artifacts.js`: sign, hash, publish

Sign the final binaries **before** preparing manifests. Any signing or other
byte change after preparation invalidates the digest and build ID. For each
artifact, `build` is the first twelve lowercase hex characters of SHA-256 of
the final file; `sha256` is the full digest; `size` is its byte length; `url`
points to an immutable name. The runner's protocol `ready.build` uses the same
rule on its own executable.

```text
node scripts/publish-desktop-artifacts.js --channel dev --only game-runner
node scripts/publish-desktop-artifacts.js --channel prod --only core-node
```

With no arguments, preparation requires Pumpkin and the addon, preserving the
existing release workflow. It includes the runner only when `homerun-game.exe`
exists. A workflow building the runner should use `--only game-runner` as an
explicit gate: that selection fails if the runner is missing. Use a clean
staging directory to avoid including a stale runner from a previous build.
Each invocation validates all its inputs before writing any manifests. Upload
all immutable binaries before changing the corresponding `latest.json` files.

### `--verify`

Nothing in the upload path notices a manifest that was hashed before signing,
and the desktop refuses a checksum mismatch permanently rather than retrying
it. `--verify` re-hashes each selected file against the manifest already on
disk beside it -- digest, build ID, byte length and the channel the `url`
belongs to -- and exits nonzero on any disagreement, reporting all of them
rather than the first.

```text
npm run verify:desktop
node scripts/publish-desktop-artifacts.js --channel prod --verify
```

Run it after signing and after re-preparing, immediately before uploading. It
writes nothing, so it is safe to run at any point. It does not replace
preparing manifests from signed bytes; it checks that somebody did.

## Channels: dev and prod

Everything a publish touches hangs off one prefix: the immutable object keys,
each `latest.json`, the absolute `url` written inside it, the printed `aws s3
cp` commands, and the mutable `assets/homerun_core.node` alias. `--channel`
picks it, `HOMERUN_ARTIFACT_CHANNEL` is the environment equivalent, and an
unrecognised name is refused rather than resolved.

| Channel | S3 base | Public base |
|---|---|---|
| `dev` (default) | `s3://fractal-homerun/homerun-desktop-dev` | `https://fractal-homerun.s3.amazonaws.com/homerun-desktop-dev` |
| `prod` | `s3://fractal-homerun/homerun-desktop` | `https://fractal-homerun.s3.amazonaws.com/homerun-desktop` |

**`dev` is the default because `prod` is what installed desktops read on every
server launch.** A publish to `prod` reaches every player within one launch, so
it has to be something somebody asked for by name; a forgotten argument
publishes somewhere nobody is listening instead of shipping an engine. `prod`
reproduces the previous layout exactly -- same keys, same absolute URLs -- so
moving an existing job onto it is a no-op.

The dev prefix is a sibling of `homerun-desktop`, not a directory inside it, so
that an IAM statement or bucket policy scoped to `homerun-desktop/*` does not
silently cover it. Whether the `ELECTRON_UPDATER_*` key can write
`homerun-desktop-dev/` at all is a permissions question, not a code one: if a
dev publish fails with `AccessDenied`, that policy needs the prefix added. It
is also why the tests look for `homerun-desktop/` **with the trailing slash**:
without it, the production prefix is a substring of the dev one and matches
both.

### Pointing a dev desktop build at `dev`

The desktop resolves the base through one helper
(`homerun-ui/src/electron/artifactChannel.ts` in `hintjen/homerun`) and honours
an override **only when `!app.isPackaged`** -- a packaged app always reads
`prod`, because an environment variable that redirects where an app downloads
executables from is a local code-execution path. For an unpackaged build:

```text
set HOMERUN_ARTIFACT_CHANNEL=dev
npm run dev
```

`homerun-ui/scripts/download-assets.js` reads the same variable at build time,
where it is a build-machine input rather than something a player can set.

| Artifact | Immutable key, relative to the channel base | Manifest key | Local manifest |
|---|---|---|---|
| Runner | `game-runner/homerun-game-<build>.exe` | `game-runner/latest.json` | `game-runner-latest.json` |
| Core addon | `core/homerun-core-<build>.node` | `core/latest.json` | `core-latest.json` |
| Pumpkin | `pumpkin/homerun-desktop-minecraft-pumpkin-<build>.exe` | `pumpkin/latest.json` | `pumpkin-latest.json` |

The runner adds `version` (Cargo package version) and `protocol: 1`. The addon
adds `version`, `abi`, and `sourceBuild`, read from the actual locally built
addon. Pumpkin retains its existing `rev` field and URL layout, plus
`minecraftVersion` and `protocol` queried from the built engine using
`--minecraft-version`. The script
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

That workflow takes its own `channel` input and passes it through
`HOMERUN_ARTIFACT_CHANNEL`, so its S3 base and read-back URLs come from the
channel rather than a literal. It greps the checked-out copy of
`publish-desktop-artifacts.js` for that variable name and refuses a dev publish
when the revision it checked out predates channels -- otherwise a dev publish
against an older engine ref would land in production. Renaming the variable is
therefore a change in both repositories.

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

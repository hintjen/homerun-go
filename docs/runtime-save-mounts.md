# Runtime working directories and server-owned saves

## Overview

Some dedicated servers load assets relative to their working directory and
also save relative to it. The Rust pilot reported that running from the server
folder could not load `Bundles/`, while running from the runtime would put
saves outside the folder that Homerun Desktop backs up, moves and deletes.
This is a layout problem the runner must handle before real-game validation.

Prefer an absolute data-path argument using `{serverDir}` when the game supports
one. That needs no links. For a game with fixed cwd-relative saves, use a runtime
working directory and declare directory mounts into the server folder:

```json
{
  "launch": { "exe": "DedicatedServer", "cwdBase": "runtime", "cwd": "." },
  "saves": {
    "paths": ["world"],
    "mounts": [{ "runtime": "server", "server": "world" }]
  }
}
```

The launch object is under the selected `platforms[host]`. The game sees
`<runtime>/server`; its files physically live at `<serverDir>/world`. Save paths,
configuration files and licence acceptance files remain server-relative.
Mounts redirect directories, not individual files. No game-specific behavior
is inferred from the game ID.

## Descriptor and contract additions

- `platforms[host].launch.cwdBase`: `server` (default) or `runtime`. `cwd` stays
  a fixed relative path under that base. Existing descriptors keep their
  original behavior. Do not put `{runtimeDir}` into `cwd`; select the base.
- `{runtimeDir}`: a host-only, single-pass placeholder in arguments, environment
  and configuration values. It is refused in a public join URL. A caller using
  it must supply the binding; `engine.invocation` accepts `runtimeDir` and
  returns the additive `cwdBase` field.
- `saves.mounts`: an optional array of `{runtime, server}` fixed relative paths.
  Both ends reject traversal, absolute paths, alternate separators, templates,
  Windows device names, streams and trailing-dot/space aliases. Mounts cannot
  overlap or cover the executable or runtime cwd. Parent links are refused
  before filesystem changes; actual runtime and server roots must be separate.
- Doctor warns for runtime cwd without mounts, asking for proof that an
  absolute data-path argument keeps saves under `serverDir`. Declaring an
  argument does not prove the vendor honors it.

`rust/homerun-core/schema/game.v0.json` is regenerated. The monorepo must update
descriptor-contract section 1 and re-pin `games/schema/game.v0.json` from the
merged main commit. Deploy a runner with this support before onboarding a
descriptor that uses these fields: older runners do not understand their
semantics. No new NDJSON command or error code is introduced by the mount work.

Two earlier commits on this branch are separate additive contract changes:
`engine.doctor` can return an optional `licence` refusal and the CLI emits it
as `licence_not_accepted`; Steam fetch plans carry `verify`, and observed
`Present` state can carry `suspect`.

## `runtime.rs`: ownership, installation and recovery

The runner holds an OS file lock at `<runtimeRoot>/.homerun-runtime-<game>.lock`
from before fetch through game exit and mount cleanup. A second runner using
that runtime receives `busy`; independent runner processes cannot replace each
other's mounts. Lock files persist but locks are released by the OS on process
death. This protects all runner-mediated updates and launches. An external
steamcmd or manual updater bypassing the runner is outside that protection.

Before any fetch, recover `<runtimeRoot>/.homerun-mounts-<game>.json`. This
record is outside the directory steamcmd updates. It lists mount locations
from the previous launch, so cleanup works even when a new descriptor has
removed or renamed its mounts. Recovery removes links only, never their target
directories or a real directory someone put in their place. An unreadable,
unsafe or obstructed recovery record blocks the operation rather than handing
the updater a potentially live save link.

After fetching and validating the runtime, prepare configuration and arguments,
record the planned mounts durably, create links and verify their actual targets.
Windows uses directory junctions via `FSCTL_SET_REPARSE_POINT`; Unix uses
directory symlinks. No Windows symbolic-link privilege or Developer Mode is
required for the junction operation.

Normal stop, EOF, game crash, failed spawn and preparation failure remove
created links while retaining save bytes. If cleanup fails, the record remains
and the next operation must recover it before fetching. Cleanup diagnostics
go to stderr.

**An abruptly killed runner cannot execute cleanup code.** Its inherited Job
Object ownership reaps the game on Windows, while the junction and recovery
record may remain. The next runner unlinks them under the runtime lock before
any fetch or start, including a start for a different server. Immediate junction
removal at the instant of runner death is not claimed. No independent cleanup
daemon is installed.

An existing real directory at a requested mount location is always refused,
even if empty. Nothing migrates or deletes it automatically. A person must
back up and move any existing saves into the correct server folder first.
An unexplained link without a recovery record is also refused; the runner
does not guess its ownership. Runtime/server directories and recovery state
are trusted app-managed locations, not a sandbox against a user concurrently
rewriting their own filesystem.

One shared runtime can have only one active mount layout. Supporting two
simultaneous servers of the same game needs separate runtime roots or a
per-server runtime view; deleting the lock is not a concurrency solution.

## Steam update policy and doctor licence codes

An unpinned Steam runtime still runs `app_update` on every start. It no longer
adds `validate` when the runtime has a completion stamp and its executable
exists. First install or a missing executable verifies; a host can also mark
the observed runtime `suspect` to request verification. No CLI repair command
or automatic corruption classifier sets `suspect` yet. Never infer corruption
from every failed launch; many failures are configuration errors.

The standalone doctor distinguishes a licence refusal from an unmet machine
requirement, including when both occur together. It neither accepts terms nor
downloads a game.

## Continuation boundary

Recovered from the PowerShell `Core-Worker` session
`580e5d25-8d88-4dc5-b1e1-ac493c360e80`. It had committed the doctor licence
change (`3e1f4dc`) and Steam verification policy (`fbb95a0`), then stopped with
partial descriptor/template fields and platform junction primitives uncommitted.
This continuation retained those commits, completed validation and schema,
connected cwd and mounts to the runner lifecycle, added runtime locking and
write-ahead recovery, hardened junction handling, and added the fixture tests.
It also corrected old-plan verification defaults and suspect pinned runtimes.

## Validation and limits

The complete Windows `npm test` run passed: 954 core tests, 262 supervisor tests
(with two ignored), eight protocol tests, and 27 lifecycle entries (one is the
fake-game fixture, so 26 substantive tests). The ABI, revision, capability, UI
bundle, 22 native addon checks and 18 artifact tests also passed. Mutation checks
proved that disabling cleanup, pre-fetch recovery, runtime exclusion or mount
validation, or changing the legacy Steam verification default, fails its test;
mutations were restored. Build checks for the final commit are recorded in the PR.

The fake executable reads `Bundles/asset` from its runtime cwd and writes a
cwd-relative save. Windows tests verify the actual junction, persistence under
the server folder, normal/crash/failed-spawn cleanup, stale-link recovery after
hard kill, two-runner exclusion, refusal of real directories and refusal of
redirected parents at either end. Core tests cover path rules, host-only
templating, schema correspondence and doctor warning inputs. Platform tests
exercise junction creation/removal on real files and preserve target contents.

These are fixtures, not RustDedicated, Steam validation, Electron integration,
restic backup or Move Installation tests. The real-game pilot must still prove
asset loading, actual save layout, restart persistence, backup/restore and the
absence of other writes outside the server folder. This continuation never
accepted vendor terms, downloaded a vendor game, signed/published artifacts or
dispatched release workflows.

## Proposed paragraph for monorepo `games/PLATFORM.md`

> A game may run with its working directory under its shared runtime when it
> requires cwd-relative assets. All persistent server data must still reside
> under that server's own folder. Prefer an absolute data-path argument; where
> unavailable, declare `saves.mounts` mapping fixed runtime-relative directories
> to server-relative directories. The runner owns the runtime exclusively,
> removes recovered links before updates, and creates links only after updates.
> A real directory is never silently replaced. Onboarding must verify the
> physical save location, restart persistence and backup/restore; a successful
> launch is insufficient. One shared runtime cannot serve two mounted layouts
> concurrently.

## Triage

**Runtime is busy:** another runner owns it or the lock cannot be opened.
Finish that lifecycle; deleting the file is not a safe way to steal ownership.

**Save mount location already exists:** inspect and back up the existing data.
The runner deliberately refuses automatic migration or unrecorded links.

**Recovery record cannot be read:** stop all users of the runtime and inspect
the record and its link locations. Preserve the server directories. Do not
run steamcmd over unresolved links to make the error disappear.

**Assets load but the world is missing from backups:** check the declared
mount targets and real writes, not only `saves.paths`. A vendor may have
additional persistent paths requiring explicit configuration or more mounts.

# Mods and plugins on Android

How a server gets the mods it is configured with. Read
[`android-server-backend.md`](./android-server-backend.md) first for how a
server starts at all, and
[`plans/android-mod-loaders.md`](../plans/android-mod-loaders.md) for where
this sits in the loader work.

> **The spec is `native-mod-support.md`** in the `homerun` repo. It documents
> the behaviour — why a client-only mod is dropped, why some of them are kept
> anyway, why version resolution does not filter to `release`. This document
> covers how that behaviour is *arranged* on Android, and does not restate it.

## What this replaced

Nothing. Before M4 the Android app installed **no mods and no plugins at all**,
on any loader. A Paper server configured with plugins on the web dashboard
started on a phone as bare Paper, silently, while `HostCapabilities` advertised
`moddedServers: true`. There was no error and no log line — the configuration
simply had nowhere to go.

So this is not a Fabric feature. Fabric made it visible; Paper had the same gap
from the day the JVM backend shipped.

## Where the logic lives, and why

`homerun_core::minecraft::mods`. Every decision — which version of a mod wins,
whether it is client-only, which dependencies it drags in, which jar in the
directory is stale — is Rust. `ModInstaller` makes HTTP requests and moves
bytes, and is deliberately incapable of deciding any of it.

That split is worth more here than anywhere else in this codebase.
`native-mod-support.md` states the problem outright: the desktop carries **two**
hand-maintained copies of this pipeline, `nativeServerManager.ts` and
`mod-installer.ts`, kept in parity by hand, where "a logic fix must be applied
to **both**". A Kotlin port would have made three. This is meant to end up
being one.

## The step machine

The core is pure — no I/O, no async, no runtime — and installing mods is three
phases of interleaved HTTP with a graph search in the middle. It cannot be one
function, so it is a driver:

```
Core.modsBegin(inputs)            -> { kind: "steps", steps, state }
Core.modsAdvance(state, replies)  -> { kind: "steps", steps, state }
                                   | { kind: "done",  outcome }
```

The host performs the steps and reports what happened. The state is opaque
JSON it holds and hands back, so the core stays a pure function of its
arguments — which is what lets the whole pipeline be tested without a network.

**Downloads are steps, not something the host does at the end.** That is not
incidental: a mod whose download fails must not pull in its dependencies. The
desktop gets that for free by downloading inside the loop, and resolving
everything up front would have quietly installed dependencies for mods that
never arrived.

**A failed step is data, not an exception.** The host reports the failure and
the core decides what it meant — a mod that will not resolve keeps its old
record rather than being deleted; a `server_side` lookup that fails makes the
whole exclusion pass fail open, because without the dependency graph, dropping
a client-only mod could strip a hard dependency of one being kept.

## What the projects list is, and what it is not

`MODRINTH_PROJECTS` is the player's list, and it is **not** the whole input.
`crossplay::merge_projects` folds in what the game type implies before the
resolver ever sees it, which is how a crossplay server gets Geyser without
anything having been written into its environment. A slug the player already
pinned is left alone. See [crossplay.md](./crossplay.md).

## What runs when

`ModInstaller.sync` runs **after settings are written and before the spawn**,
which is where the desktop's `startServer` puts it. It is deliberately outside
the block that prepares the jar and runtime: a mod that cannot be fetched must
not stop a server starting, and `sync` never throws for a mod-shaped reason. A
server whose mods could not be resolved starts with whatever is already in
`mods/`, and says so on the console.

Every loader goes through it. A Paper server's plugins resolve through exactly
the same code as a Fabric server's mods — `mods` or `plugins` is one answer
from the core (`mods::sub_dir`), and Spigot and Bukkit resolve against Paper's
Modrinth facet because that is what their plugins are published under.

### Quilt resolves against Quilt's facet, and that costs it

Quilt is hosted now (see
[`android-server-backend.md`](./android-server-backend.md#where-the-four-installers-differ))
and its mods go through this pipeline unchanged: `sub_dir("quilt")` is `mods` and
`modrinth_facet` passes `quilt` straight through.

**Expect a Quilt server to resolve very few mods.** Modrinth's loader tags are
author-declared, and most Fabric mods are not tagged `quilt` even though Quilt's
compatibility layer runs them — Fabric API for 1.21.11 has **eleven**
`fabric`-tagged versions and **zero** `quilt`-tagged. So a Quilt server asks for
Fabric API and is told `incompatible`.

That is exact parity with the desktop, which maps only `spigot` and `bukkit` onto
another facet and queries `quilt` directly, and it is left alone deliberately:
widening the query to Fabric's facet would install jars Modrinth never claimed
were compatible, and the point of resolving against a facet is that everything
which arrives is installable by construction. Worth revisiting as its own
decision rather than as a side effect of adding Quilt.

## Verified against real Modrinth, on a phone

The fake-host tests cover the pipeline exhaustively; what they cannot cover is
Modrinth's actual answers. A Fabric server on Minecraft 26.2, configured with
four mods from the app's own browser and started on a Pixel 9 Pro XL:

| Configured | Outcome |
|---|---|
| Fabric API | installed — `fabric-api-0.157.0+26.2.jar` |
| FerriteCore | installed — `ferritecore-9.0.0-fabric.jar` |
| Lithium | installed — `lithium-fabric-0.25.3+mc26.2.jar` |
| **Sodium** | **skipped, silently** — client-only |

The loader then logged `Loading 43 mods` with all three present, and
`.homerun-loader.json` recorded a `versionId` for each and **no record for
Sodium**. So the client-only exclusion, the version resolution and the marker
write are all confirmed against the live API rather than against a fixture.

One host behaviour worth knowing, because it is the UI's and not this file's: a
server must be **stopped** before mods can be added to it. The picker lists a
running server under "Incompatible Servers" with that reason.

## The marker

`.homerun-loader.json` — the desktop's name and shape, so a directory restored
from a desktop backup is understood rather than reinstalled. Two writers own
different halves of it:

| Field | Owner |
|---|---|
| `loader`, `mcVersion`, `loaderVersion` | `ServerLoader`, or `JavaServerBackend` for a downloaded-jar server |
| `mods` | `ModInstaller` |

So **every write is a merge** (`LoaderMarker`): each writer replaces what it
owns and preserves everything else. They run at different times — the desktop's
`setupServerLoader` writes the marker before `downloadMods` has resolved
anything — and a round-trip through a narrower type would silently drop a mod
record on the first restart after a restore.

A vanilla or Paper server never runs an installer, so `JavaServerBackend` is
the only place it gets a marker at all. The desktop reaches the same state by
running `setupServerLoader` for every loader, vanilla included.

`Core.loaderFilesToClean` includes the marker, so a loader or Minecraft version
change wipes it along with the install. That is intended: mod records describe
files that no longer exist once the loader has been torn down.

## The sweep, and the one rule it must not break

Stale jars are deleted, and the scope is narrow on purpose: **only files a
previous Homerun run installed are candidates.** A jar the player dropped into
`mods/` by hand has no record naming it and no modpack claiming it, so it is
not managed and is never touched.

Getting that wrong deletes somebody's mods, which is the worst thing this
pipeline can do. It has a fixture of its own
(`hand-added-jars-survive-the-sweep.json`) for exactly that reason.

The same rule is what makes it safe for another installer to put a jar in the
same directory. `CrossplayInstaller`'s Floodgate jar and `PluginInstaller`'s
minigame jars have no record here, so the sweep never considers them — see
[crossplay.md](./crossplay.md).

## Testing

Two layers, and they answer different questions.

`minecraft::mods::tests` drives the whole pipeline through a fake host — 28
cases covering version fallback, client-only skipping, the dependency closure,
cycles, batching, the sweep, and failing well. None of them touch a network.

[`shared/fixtures/mods/`](../shared/fixtures/mods/README.md) is the part meant
to outlive this implementation. Each file is a complete run — inputs, canned
Modrinth responses, and the outcome that must come out — recording a behaviour
that was learned the hard way in one of the desktop's copies. The Rust tests
run all of them today; pointing the desktop's Jest suite at the same files is
what closes the loop, and the format is fixed so that it can be.

A tampered fixture fails with the `why` field in the message, so the failure
says which behaviour is at stake rather than just which numbers differ.

## Modpacks

A `.mrpack` is a zip: `modrinth.index.json` naming mods to fetch by URL, plus
an `overrides/` tree copied verbatim. `ModpackInstaller` fetches and reads it;
`homerun_core::minecraft::modpack` decides what goes in.

**A pack outranks the server's own settings.** The manifest decides the loader,
the Minecraft version *and* the loader build, not `TYPE` and `VERSION`. That is
the desktop's order too, and the pin is not advisory: a pack built against
Forge `47.2.17` and run on `47.4.20` dies at boot with a mixin
`InjectionError`, because version-sensitive mixins target the exact patched
classes of one revision.

### Deciding what a pack installs

The question is *which of these mods must not be installed on a dedicated
server*, and it is harder than it sounds. Three sources of evidence, in order:

1. **Modrinth's project-level `server_side`**, reached by hashing every mod and asking `/version_files` — one request for the whole pack. The manifest's own per-file `env.server` is **not** used: it is author-supplied, and kitchen-sink packs routinely export every mod as `required` even when many are client-only.
2. **The jar's own metadata**, for the ones Modrinth has never seen — packs ship CurseForge builds whose bytes match no Modrinth file. `homerun_core::minecraft::modjar` reads `fabric.mod.json`, or **both** `neoforge.mods.toml` and `mods.toml` (a jar ships a minimal legacy one beside the real one, and NeoForge reads the latter).
3. **A strict name search**, for jars that declare `side = "BOTH"` — which authors leave on genuinely client-only mods. Three guards stop false positives: the hit must be a *mod*, its slug must normalise-equal the mod id, and only then does its `server_side` count.

Then the part that makes it correct rather than merely aggressive: **a
dependency closure**. Dropping every client-only mod breaks servers a different
way, because kept mods hard-depend on client-only libraries — `chipped` needs
`athena`, which Modrinth marks unsupported. Only client-only mods that *no kept
mod requires* are excluded.

And after assembly, **reconciliation**: Modrinth's per-version dependency data
drifts from what a jar's own metadata says, and the loader enforces the jar. A
rescued client-only mod whose hard dependency did not survive is dropped, and
the drop cascades. A *server*-installable mod missing one is left alone — that
is a real modpack error and the loader should report it.

**Fail-safe, not fail-open.** If the hash lookup fails there is no dependency
graph, so excluding anything could strip a hard dependency — and the pack is
installed exactly as its author shipped it.

### Two manual escape hatches

Modrinth marks some genuinely client-only mods `optional` rather than
`unsupported`, and the closure keeps those. `native-mod-support.md` records two
static-detection attempts that were tried and **removed** for over-removing
working mods, so these stay manual:

| | |
|---|---|
| `MODRINTH_EXCLUDE_FILES` | partial filenames — `rubidium-extra` drops `rubidium-extra-0.4.18.jar` |
| `MODRINTH_OVERRIDES_EXCLUSIONS` | ant globs against **every** override path, not just mods |

### Mobile-specific

- **The archive is cached by version id**, in `filesDir/modpacks/` rather than `cacheDir` — Android may delete `cacheDir` under a running app, and here that costs a several-hundred-megabyte download.
- **Free space is checked before any mod is fetched.** The manifest states every mod's size, so this is arithmetic rather than a guess, and it refuses with both numbers. Failing at 90% having filled the device is the worst available outcome — a phone with no free space misbehaves in ways that have nothing to do with Minecraft.
- **Override extraction refuses path traversal.** The archive came off the internet, and a `../` entry would write outside the server directory.

### What a pack failure does

`ModpackInstaller` throws and the launch fails, where [`ModInstaller`] never
does. That difference is deliberate: a missing mod leaves a server that still
runs, and a missing *modpack* leaves one that is not the server the player
asked for.

## Not here yet

- **Naive mode** (`disableAutoFix`) — exists for the desktop's Dockerised KnownError reproducer, to make a crash surface rather than be fixed. An app has no use for it.
- **File import.** `fileImport` is false and stays false; a `.mrpack` arrives by URL from the dashboard.
- **Resumable pack downloads.** `ServerJar` resumes a partial jar and this does not yet; a pack is bigger and deserves it more.

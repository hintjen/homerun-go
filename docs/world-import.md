# Importing a player's world

## Overview

A player who already has a world can start a new server with it instead of a
fresh one: a Terraria `.wld`, a Palworld world downloaded from another host's
dedicated server, a Hytale universe from another server or a single-player
save, or a Valheim world from 1.0 or before it. Importing is something a **game implements**, not
something a host knows how to do. A game that supports it has an
**importer**; a game without one can't import.

It comes in the engine's usual two halves:

- **The decisions,** in `homerun-core::engine::import`: which world is in
  what the player picked, which files go where, which settings follow from
  the world, and what is refused. Pure, like the rest of the crate.
- **The effects,** in `homerun-supervisor::world_import` and the runner's
  `src/import.rs`: listing a folder or zip, reading the start of a file,
  copying files, and applying a staged world at the server's first start.

The host picks the source, keeps the staging folder, and says when the world
lands. It never looks inside a world.

## Two steps

1. **At create, `homerun-game import` stages the world.**

   ```
   homerun-game import <game.json or slug> --source <path> --stage <folder> [--world <id>] [--runtime-version <v>] [--json]
   ```

   It prints one of `import-candidates` (several worlds and no `--world`:
   nothing staged), `import-staged` (the world's name and facts, the settings
   it decides, warnings) or `import-refused` (a code and a sentence for a
   player), with `import-progress` while it copies. Exit code 0 after staged or
   candidates, 2 after refused.

2. **At the first start, `start.import` applies it.** The staging folder's
   path, on the `start` command (or `--import` on `homerun-game launch`).
   Before the game's config is written, the runner:
   - does nothing if any of the staged `present` patterns already match in
     the server's folder, so a backup restore (which the desktop runs before
     `start`) wins, and passing the same folder on every start is harmless;
   - otherwise copies `files/` in, never over a file that is already there,
     then sets the staged INI keys with the same merge as a game's config;
   - fails the start with `import_failed` if it can't, rather than starting
     an empty world that would then block the import for good.

Features: `world-import` (the mechanism), and `world-import:<game>` per
importer. A host offers import only when both are present, and sends
`start.import` only to a runner with `world-import`: an older runner ignores
the field and would start an empty world.

## The conversation with an importer

The runner lists the source (a folder walk that skips symbolic links, a zip's
entries, or one file) and hands the list to the importer. The importer answers
`Read` (show me the start of these files), `Choose` (several worlds),
`Plan` or `Refuse`. `engine::import::plan` checks every plan before the runner
sees it: each file is in the source, each destination is a plain relative path
(no `..`, drive, `\`, device name or trailing dot), INI edits can't add a line
or a section, and the copy fits in 4 GB.

The staging folder holds `files/`, laid out as the files land in the server's
folder, and `import.json` (`Staged`, format 1): the world's name and facts,
the `present` patterns, and the INI and JSON edits (`json`, a dotted key and
a text value, applied with `json_config::merge`; absent in a world staged
before Hytale). A world with no name a player would recognise has an empty
`name`, and its facts say which it is.

## The importers

### Terraria

A `.wld` is the whole world; characters stay on each player's computer. The
importer reads the header (version, name, seed, size, mode), stages the file
as `data/Worlds/world.wld` with its `.bak`, and returns `worldSize`,
`difficulty` and `worldName` for the host to create the server with.

- A world from a newer Terraria is refused when `--runtime-version` names a
  release in `terraria::RELEASES`. Without it, a world newer than any tested
  release is a warning. This matters because the server, given such a world,
  prints "File is from a later version of Terraria" and **exits 0**.
- A tModLoader world (a `.twld` beside it) and a custom size are refused.
- A Journey world warns that only Journey characters can join.

`RELEASES` is the world version each server release writes, found by
generating a world with each one: 1.4.5.0 and 1.4.5.1 write 315, 1.4.5.6
writes 319, 1.4.5.8 writes 326. Add a row when a new release is pinned.

A community report (PassiveLemon/terraria-docker#20) describes a 1.4.5 bug
where `-autocreate` with the wrong size corrupts an existing world. On 1.4.5.8 it did not reproduce: a medium world
loaded with `-autocreate 1` and saved changed only its header's save counter.
Matching the size setting to the world is still what the importer does.

### Palworld

A world from a **dedicated server**: the world folder, `SaveGames/0`, or a
whole `Pal/Saved`, as a zip or a folder. The importer copies the world folder
to `Saved/SaveGames/0/<name>/` keeping its name, and sets
`DedicatedServerName` in `Saved/Config/WindowsServer/GameUserSettings.ini`.

- `WorldOption.sav` is left out: it overrides `PalWorldSettings.ini`, so the
  server would ignore Homerun's settings. Players choose a respawn point again.
- The world's `backup/` folder (the old server's snapshots) is left out.
- An included `PalWorldSettings.ini` (Windows or Linux) is staged under
  `Config/WindowsServer/`; the game's managed keys are written into it at
  start, so the old server's other choices stay.
- With several worlds, the one an included `GameUserSettings.ini` names is
  chosen; otherwise the player picks.
- A co-op or single-player world (its host is
  `Players/00000000000000000000000000000001.sav`) is refused: on a dedicated
  server the host would start again with nothing, and moving them means
  rewriting `Level.sav`, which this does not do.

Players keep their characters because a dedicated server names each player's
file after their own account. Tested 2026-10-12 by moving one world between
two servers and back, with a Steam player and a Game Pass player: both kept
their characters, items and guild, and the second server reused their files.

### Hytale

A universe from another server, or a single-player save
(`%APPDATA%\Hytale\UserData\Saves\<save>\`), which has the same layout
because single-player runs the same server inside the game. A universe is a
folder holding `worlds/<world>/config.json`. The importer copies it to
`universe/` without the `.bak` copies the server keeps, brings `mods/`,
`permissions.json`, `whitelist.json` and `bans.json` from beside it, and sets
`Defaults.World` in the server's `config.json`: the world the old server's
config named, else `default`, else the first.

- **Never copied, never read:** `auth.enc`, `auth.key` (the server's sign-in,
  which only decrypts on the machine that wrote it), any other `.key`, `.keys`
  or `.enc` file, `account.dat`, `keyutil_*`, and the client's
  `client_metadata`. `hytale::never_copied` matches them on every part of a
  path, so a download that nests them anywhere is covered.
- Warned about: a world config `Version` above 4 (a newer Hytale; worlds
  upgrade one way), `DeleteOnUniverseStart: true`, and `RequiredPlugins`.
- Players are keyed by account UUID (`universe/players/<uuid>.json`) in a save
  and on a server alike, so characters carry over. Observed on a Homerun
  server 2026-10-12; a single-player save's player file is unverified.

### Valheim

A world from Valheim's `worlds_local` (or the older `worlds`) folder, a
server's save folder, or the whole `IronGate/Valheim` folder, as a folder or a
zip. The descriptor runs the server with `-savedir save` and `-world
{setting:worldName}`, and `-world` picks a world by its **folder or file
name**, so the importer places the world under a name and returns that name as
`worldName`, a create-only setting.

- **1.0 worlds** are folders: `_main.<n>.fwl2`, `.db2`, `.chunks` and `.ok`
  for the newest save generation `n`, plus `*.chunk` files. The generation's
  files and every chunk land in `save/worlds_local/<name>/`. A generation
  with no `.ok` is a save that didn't finish and is skipped; a folder with
  only `_main.0.fwl2` was generated and never saved, and is refused
  (`read_failed`).
- **Pre-1.0 worlds** are a `<name>.fwl` and `<name>.db`, staged as
  `save/worlds_local/<name>.fwl|.db`. The 1.0 server converts them the first
  time it loads them: one way, silent, and minutes long for a large world,
  which the import warns about. A `.fwl` without its `.db` is refused.
- **Backups** (`<name>_backup_<yyyymmdd-hhmmss>.fwl/.db`) are offered, with
  a label saying so, only when the world they back up isn't in the source:
  after 1.0 converted a world, a backup is often the only copy left. One lands
  under the world's own name, without the suffix, when the suffix is exactly
  that date. `.old` files and `_backup_auto-*` copies are never offered.
- **Never read or copied:** characters (`*.fch`, `characters*`), the
  permission lists, `prefs`, logs, the map texture caches and
  `steam_autocloud.vdf`. `valheim::never_read` matches them on every part of
  a path.
- A Game Pass save (`containers.index`) is refused: it is a container of
  unnamed blobs, not world files.

The metadata both formats start with is an int32 length of the rest, an int32
save format, the name and the seed text as .NET strings, and the int32 seed.
That is all the importer reads, plus the version at the start of a `.db` and
the `.ok` (four bytes: the save format again). Server 1.0.17 writes format 41
(`valheim::RELEASES`); a newer world is refused when `--runtime-version`
names a release there, and warned about otherwise.

Not yet verified on a running server: that a 1.0 folder loads under a name
other than the one in its `.fwl2`, and how long a large pre-1.0 world takes
to convert against the descriptor's ready timeout.

## Adding an importer

1. A module in `homerun-core/src/engine/import/` exporting an `Importer` for
   the game's descriptor `id`, added to `IMPORTERS`. Read only what you must,
   and say in the module header what the game's world is and what is refused.
2. Tests with the real layout: a header cut from a world the server wrote
   (`engine/testdata/`), or the paths a real download has.
3. Nothing else: the feature `world-import:<id>` follows from `IMPORTERS`.

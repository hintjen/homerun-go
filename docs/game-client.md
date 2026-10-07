# Starting a player's game client

## Overview

A server for a descriptor game (Palworld first) gets a **Play** button that
starts the player's own copy of the game, from **Steam** or **Xbox Game
Pass**. It connects to the server only where the game supports that.
Otherwise it starts the game and the UI shows the address.

There is no store-neutral way to start a game. Every launcher that covers
several stores (Playnite, GOG Galaxy, Lutris) keeps a small adapter per store
that reads that store's own record. This is the same shape: one adapter for
Steam and one for Xbox.

It comes in the engine's two halves:

- **The decisions,** in `homerun-core::engine::client`: which copy to start,
  exactly what to run, and whether that also joins. Pure, like the rest of the
  crate.
- **The effects,** in `homerun-supervisor::client_launch`, run by the runner
  as `homerun-game client plan|launch`: reading the registry and the store
  files, asking Windows for its packages and processes, and starting the
  program. See [The runner](#the-runner-homerun-game-client) below.

Source: `rust/homerun-core/src/engine/client/`. The plan, with the research
behind it and the decisions taken, is `plans/game-client-launch.md` in the
desktop repository.

## The descriptor: `client.stores`

```json
"client": {
  "joinUrl": null,
  "joinHint": "In Palworld, choose Join Multiplayer Game and paste the address into the box below the server list.",
  "stores": [
    { "store": "steam", "appId": 1623730, "join": "info" },
    {
      "store": "xbox",
      "packageFamilyName": "PocketpairInc.Palworld_ad4psfrxyesvt",
      "applicationId": "AppPalShipping",
      "join": "info"
    }
  ]
}
```

- **`store`** is `steam` or `xbox` (Game Pass and the Microsoft Store are one
  package system). An unknown store parses as `Unknown`, and validation warns
  about it and doesn't fail. It comes from a newer schema, and the stores this
  build knows still work. That's why the type is a flat struct with a
  discriminant and not a tagged enum, as with `RuntimeSource`.
- **`appId`** (Steam) is the number in `steam://rungameid/<appId>`.
- **`packageFamilyName`** and **`applicationId`** (Xbox) make up
  `shell:AppsFolder\<family>!<application>`. The application is **declared,
  never taken from the manifest's first entry**: Age of Empires IV's package
  has `Game` and `Editor`, and an add-on content package has none at all.
- **`join`**:
  - `url` opens `client.joinUrl` through Steam, which starts the game and
    joins. Use it only for a game that honours it and that a person has seen
    work: `steam://connect` reaches a game only if the game wired it up, and
    does nothing visible for most that didn't (Rust, Valheim, Squad, Arma 3).
  - `args` starts the game with `client.joinArgs` filled in, which joins.
    Steam only: `steam.exe -silent -applaunch <appId> <args>`. The same rule
    applies: only for a game a person has seen it work for.
  - `info`, the default, starts the game and has the UI show the address and
    `joinHint`.

### `client.joinArgs`: Terraria joins by itself

Terraria's client takes `-join <host> -port <port>`. With the arguments, it
goes from character selection straight to connecting; seen on 2026-10-07,
through Steam's `-applaunch`, which passes them to the game unchanged and
without a prompt.

```json
"joinArgs": ["-join", "{host}", "-port", "{port:game}"],
"stores": [{ "store": "steam", "appId": 105600, "join": "args" }]
```

Each element is one of three shapes, or the template is refused
(`client::join::build_join_args`):

- **a flag:** `-` or `+`, a letter, then letters, digits, `_`, `.`, `-`
  (`-join`, `+connect`). Never a value, so a descriptor can't put a path or a
  second command into the game's command line;
- **`{host}` or `{port:<name>}`**, the whole element;
- **both, joined by `:`** (`{host}:{port:game}`), for a game that takes one
  `host:port` argument.

A filled host is held to `is_join_host` **and must not start with `-` or
`+`**, so the game can't read the server's address as a flag. Everything before
the template's elements (`steam.exe -silent -applaunch <appId>`) is the core's.
The address arrives from the desktop as `--link-file`, the server's link as the
API returned it, and goes through `address_from_link` like any other.

Without an address, or with a port the gateway hasn't assigned, the game still
starts, as `rungameid` with `join: info`, and `joinRefusal` says why it won't
join by itself.

### What validation refuses

`check_client_stores` in `engine/validate.rs`:

- an entry missing its store's ids, or carrying the other store's;
- a family name or application id not in Windows' shape;
- the same store twice;
- `join: url` with no `client.joinUrl`, or on Xbox, which has no connect link;
- `join: args` with no `client.joinArgs`, or on Xbox, whose launch takes no
  arguments;
- a `joinArgs` element in none of the three shapes, or naming a port the game
  doesn't declare or doesn't expose.

## The plan: `client::plan`

`plan(descriptor, machine, address)` takes what the runner found (a
`Machine`: Steam's install, the Store packages, the running processes) and
the server's join address if the caller has one. It answers one of:

| Outcome | Meaning |
|---|---|
| `launch` | a `Program` (`steam` with its exe, or `explorer`), its `args`, the `installDir`, and the `join` it will do |
| `already-running` | a copy is running from an installed store's folder; nothing starts |
| `not-installed` | the game declares stores and none has it |
| `no-stores` | the descriptor declares no store this build can start |

**Steam first.** When both copies are installed, Steam's starts.
`PREFERENCE` holds that product decision; the order in the file doesn't
matter.

**The program is one of two, fixed here.** `steam.exe` from a path held to
that shape (`steam::steam_exe`: absolute, on a drive, ending in
`\steam.exe`, no `..`), or `explorer.exe`, which the runner resolves under
`%SystemRoot%`. A launch is a program and an argument list. **Nothing is ever
handed to a shell to interpret.**

**Every id is re-checked before it is used,** here, not trusted because
validation ran somewhere else.

**A link that can't be built still starts the game.** A `url` store with no
address yet, or a template the join rules refuse, falls back to
`rungameid`. `joinRefusal` then says why, in a sentence for the player.

## Already running, and `client::started`

Palworld can stay running after the player quits, with no window, until it's
killed in Task Manager. While that process exists, **a launch starts
nothing**. So a process under the install folder isn't proof that a launch
worked:

- `plan` answers `already-running` when one is there **before** the launch,
  from any installed store.
- `Launch::started(before, now)` counts only a process that **wasn't in the
  list taken before the launch**.
  - It counts **the game's own executable** when the store names it. Only Game
    Pass does, through `MicrosoftGame.config` (below).
  - Otherwise any new process in the install folder counts. That fallback
    applies to Steam always, and to a Game Pass package without the file.

`explorer.exe` exits **1** having started the game, so its exit code can't
be used either. The runner polls `started` until a timeout.

The exact executable matters for Game Pass because the launch stub,
`GameLaunchHelper.exe`, runs from the install folder too. Counting it would
call a launch good when the game behind the stub then failed to start, for
example on an Xbox sign-in.

Paths compare as Windows compares them (`is_under`): either slash, any case,
with or without `\\?\`. A sibling folder with a longer name
(`Palworld2`) is not inside `Palworld`.

## Steam: `client/steam.rs`

Steam's own record, in Valve's KeyValues ("VDF") text format:

- `<Steam>\steamapps\libraryfolders.vdf` lists every library and, in current
  Steam, the app ids each holds. The **older format** lists only paths, so
  `Library::apps` is `None` and every library has to be asked.
- `<library>\steamapps\appmanifest_<appid>.acf` exists per installed game
  and names its folder under `steamapps\common\`.

The KeyValues reader handles quoted strings with `\\ \" \n \t` escapes,
nesting, `//` comments, a BOM, and case-insensitive keys (the old format wrote
`LibraryFolders`). Nesting is capped at 16, so a hostile file can't recurse
the stack away.

**Installed means a manifest exists.** `StateFlags` are kept but not judged:
a game that's updating or still downloading has a manifest, and Steam tells
the player about that itself as soon as the launch reaches it.

`installdir` must be one plain folder name. It's Steam's file, but it decides
which folder's processes count as the game.

Not read: the Start menu. Steam's shortcut there exists only if the player
kept it.

## Xbox: `client/xbox.rs`

Windows keeps the record. The runner asks it for the declared families and
passes back a `Package` for each, with:

- the family name, install location, signature kind and framework flag, from
  Windows;
- the application ids, from the package's `AppxManifest.xml`;
- the game executable per application, from its `MicrosoftGame.config`.

The runner reads both files from the install folder and hands their text to
`client::manifest`, below.

- **Store-signed only** (`signatureKind == "Store"`), not a framework, with an
  install location. A sideloaded package can take any family name.
- The declared application must be in the package, or it counts as not
  installed. That's what keeps an add-on content package from being launched.
- Family names and application ids compare case-insensitively.

Every Game Pass game on the dev machine (15) has the same entry point,
Microsoft's `GameLaunchHelper.exe`. So the same `shell:AppsFolder` line should
start each, and the real game process has a name of its own
(`Palworld-WinGDK-Shipping.exe`).

Windows' Start app list (`Get-StartApps`) is **not** reliable for this.
Palworld's Game Pass build is installed on the dev machine and isn't in it.

## The manifests: `client/manifest.rs` (desktop only)

Behind the **`client-manifests`** feature, which only the desktop's runner
enables. A phone never starts a Game Pass game, so the mobile builds ship no
XML parser. It's the crate's second dependency exception after
`ed25519-dalek`, and `Cargo.toml` argues it.

- **`appx_applications`** reads `AppxManifest.xml`: the `Id` of each
  `<Application>` under `<Applications>`, in file order.
- **`game_executables`** reads `MicrosoftGame.config`: each `<Executable>`
  under `<ExecutableList>`, as its `Id` (the application it belongs to) and its
  `Name` (relative to the install folder). For Palworld that's
  `AppPalShipping` → `Pal/Binaries/WinGDK/Palworld-WinGDK-Shipping.exe`. For
  Age of Empires IV it's `Game` → `RelicCardinal.exe` and `Editor` →
  `EssenceEditor.exe`.

Both are read by local name, ignoring namespaces. The real manifest uses a
default namespace plus `uap:`, `desktop6:` and `rescap:` prefixes.

**These files are the package author's, and anyone can sideload a package.**
So:

- **`roxmltree` refuses a DTD by default.** An entity-expansion file (the
  "billion laughs") is a parse error, not a memory bomb, and a test pins that
  default.
- **Files over 1 MB are refused.** Palworld's two are under 6 KB together.
- **Every value is held to a shape and dropped if it doesn't fit.** An
  application id must look like one. An executable's path must be relative and
  stay inside the install folder: no drive, no leading separator, no `..`.

A file the runner couldn't read, or that doesn't parse, costs precision, not
the launch. No applications means the package isn't launchable. No
executables means `started` watches the folder.

`npm run test:core` runs the crate's tests with the feature on.

## The join link: `client/join.rs`

A port of the desktop's `gameRunner/joinUrl.ts`, which keeps working until it
calls this instead. Its tests are ported case for case.

- **`JOIN_URL_SCHEMES` is exactly `steam:`.** The scheme is decided before
  anything is substituted, and a template with any other scheme is never
  filled in. The OS starts whatever program is registered for a scheme.
- `{host}` and `{port:<name>}` only. A secret, a setting, `{serverDir}`,
  `{HOST}` and `{}` are all refused.
- The host and ports come from the API, so they're validated as values
  (`is_join_host`: letters, digits, `.`, `-`). A value that would change the
  link's meaning is refused, never escaped.
- `address_from_link` prefers `domain.uri` over `fqdn`, because an SRV name
  answers no plain lookup. A bare `forward_ports` entry (`"28015/udp"`) is a
  port the gateway hasn't assigned, and **its number is never used as the
  public port**.

**The one difference from the TypeScript.** `joinUrl.ts` finishes by
round-tripping through `new URL` and refusing anything that changes. This
crate has no URL parser. Instead, a template's literal text is limited to
letters, digits and `:/._~-+%=&?`, so a filled link contains nothing a URL
parser rewrites. That refuses a few templates the round trip would allow, and
admits none it refused.

## The runner: `homerun-game client`

```text
homerun-game client plan   games/palworld/game.json --json
homerun-game client launch games/palworld/game.json --json [--link-file link.json] [--timeout-seconds 120]
```

- **`plan`** surveys the machine and prints the plan, and **starts nothing**.
  The desktop asks it before anyone presses Play, to show the button's state:
  not installed, already running, or which store will start.
- **`launch`** prints the plan as soon as it has one, so the UI can open the
  join modal while the game loads. It then runs the launch and prints whether
  the game appeared.
- **`--link-file`** is the server's link as the API returned it. Only a store
  that joins by link needs it; the core turns it into the join address.

Both are one short-lived invocation per press, **separate from `supervise`**,
so a launch never touches a hosted server's session. `--json` prints one line
per event:

| `event` | Fields |
|---|---|
| `client-plan` | `plan`: the core's `Plan`, tagged by `outcome` (`launch`, `already-running`, `not-installed`, `no-stores`) |
| `client-started` | `launch`, `pid` |
| `client-not-started` | `launch`: nothing appeared before the timeout |
| `client-failed` | `launch`, `message`: the program couldn't be started at all |

A refusal is an answer, so it exits 0 with the plan on stdout. A non-zero exit
means the command itself couldn't run (no descriptor, a bad argument).

The runner advertises **`client-launch`** in `--features` and in `ready`. The
desktop requires it before it offers Play, so an older runner leaves the button
out rather than failing when it's pressed.

### `homerun-supervisor/src/client_launch.rs`

Behind the supervisor's **`client-launch`** feature, which turns on the core's
`client-manifests`. Only the runner enables it; `npm run test:rust` does too.

Everything that touches the machine is behind a small `Probe` trait. The whole
flow (survey, plan, spawn, wait) is therefore tested on any computer against an
in-memory fake, and only `WindowsProbe` needs Windows:

| Question | How Windows answers it |
|---|---|
| Where is Steam? | `reg query HKCU\Software\Valve\Steam /v SteamPath` |
| Which Store packages? | one `Get-AppxPackage` call, for the names before the `_` in the declared families, passed through an environment variable and never into the script's text |
| What is running? | a Toolhelp snapshot, then each process's full image path (`QueryFullProcessImageNameW` with limited query rights, which is enough for the player's own processes) |
| Start it | the plan's program with its argument list, detached and in its own process group, so it doesn't belong to the short-lived runner |

**What's read.**
- **Steam:** only the libraries Steam says may hold the declared app id. Steam's
  own folder is always searched, because the older library file never listed it.
- **Game Pass:** only the declared families, plus each one's two manifests.

Files are read with `std`, capped at 1 MB, lossily as UTF-8, and never written.
A file that's missing or unreadable costs precision, not the launch: see the
manifests section.

**Install folders are resolved through their links, and this is load-bearing.**
A Game Pass game installed to another drive or folder runs from there, and
Windows reports its process from there. On the dev machine:

- The package's install location is
  `C:\Program Files\WindowsApps\PocketpairInc.Palworld_…`.
- That's a junction to `D:\WindowsApps\PocketpairInc.Palworld_…`, which is a
  junction to `D:\XboxGames\Palworld\Content`.
- The game runs as
  `D:\XboxGames\Palworld\Content\Pal\Binaries\WinGDK\Palworld-WinGDK-Shipping.exe`.

Compared as Windows reports the package, the game was never seen: not as
started, and not as already running. The first real launch test ran into this.

So the probe's `resolve` follows every link before the core sees a folder:

- It uses `canonicalize` where the folder can be opened.
- Otherwise it follows `read_link` hop by hop, up to 8, because `WindowsApps`
  lets a user read a junction's target without opening what's behind it.
- It writes the result without the `\\?\` prefix.

Steam libraries get the same treatment. Note that `Get-CimInstance
Win32_Process` reports the *unresolved* path, which makes a by-hand comparison
look like it should have matched.

**Waiting.** The process list is read before the launch and then every 500 ms,
for up to 120 s by default. Steam may have to start, sign in and update first.
The launch counts once `Launch::started` sees a new process.

A survey is fast. On the dev machine, `client plan` for Palworld (Steam
installed without it, Game Pass with it) took half a second, most of it
`Get-AppxPackage`.

`survey_this_machine` is an ignored test that surveys the machine it runs on
and prints the plan, starting nothing:

```text
cargo test --manifest-path rust/homerun-supervisor/Cargo.toml --features client-launch survey_this_machine -- --ignored --nocapture
```

## File map

| File | Role |
|---|---|
| `homerun-supervisor/src/client_launch.rs` | `Probe`, `WindowsProbe`, `survey`, `launch`, `Outcome` |
| `homerun-game-cli/src/client.rs` | `homerun-game client plan\|launch`, its arguments and its JSON lines |
| `engine/client/mod.rs` | `plan`, `Launch::started`, `started`, `is_under`, `PREFERENCE` |
| `engine/client/steam.rs` | KeyValues reader, `libraryfolders.vdf`, `appmanifest_*.acf`, `steam_exe` |
| `engine/client/xbox.rs` | `Package`, `GameExecutable`, `find`, `executable_for`, id and path shapes, `apps_folder_target` |
| `engine/client/manifest.rs` | `AppxManifest.xml` and `MicrosoftGame.config`, behind `client-manifests` |
| `engine/testdata/palworld.*` | Palworld's real manifest and game config, trimmed |
| `engine/client/join.rs` | the join address and link, ported from `joinUrl.ts` |
| `engine/descriptor.rs` | `ClientStore`, `StoreKind`, `JoinVia` |
| `engine/validate.rs` | `check_client_stores` |
| `engine/schema.rs` | `client.stores` in the schema |

## Triage

Start with `homerun-game client plan <game> --json`: it shows what the runner
found and decided, and starts nothing.

- **Play says "not installed" and the game is installed on Steam.** Check
  that `libraryfolders.vdf` lists the library, that
  `appmanifest_<appid>.acf` is there, and that the registry's `SteamPath`
  ends where `steam.exe` is.
- **Play says "not installed" and the game is on Game Pass.** Check
  `Get-AppxPackage <Name>`: it must show `SignatureKind : Store`, and the
  descriptor's `applicationId` must be among the manifest's application ids.
- **Play says "already running" and nothing is on screen.** A lingering
  process (Palworld does this). Close it in Task Manager and press Play again.
- **The game opened but Homerun said it didn't start, or it's running and Play
  doesn't say so.** Most likely an install folder that wasn't resolved through
  its junctions: compare the plan's `installDir` with the folder the process
  really runs from. Otherwise the timeout ran out before a *new* process
  appeared.
  - With an `executable` in the plan (Game Pass), compare it to the running
    game's path: a game update can move its executable, and
    `MicrosoftGame.config` is re-read on every launch.
  - Without one, compare the plan's `installDir` to the process's path.

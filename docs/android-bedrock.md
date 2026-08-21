# Bedrock servers on Android, served by PowerNukkitX

## Overview

A player on an Android phone picks **PowerNukkitX** in the create wizard and
gets a server a Bedrock client — on a console, a phone, a tablet or a Windows
PC — can join. It is a Bedrock server that happens to be a Java jar, so it runs
on the JVM this host already stages, supervised by the same `ProcessEngine` that
runs a Paper server.

The plan and the reasoning behind each decision are in
[`plans/android-bedrock.md`](../plans/android-bedrock.md). This file is what the
code does.

### Why this exists at all

Not a new feature so much as a hole being closed. The create wizard gates the
Bedrock tile on `serverBackends` containing `javaNative`, and Android declares
that — so the tile has been **offered on Android all along**, and picking it
created a `minecraft/native-bedrock` server that the core then refused to start
with *"This is a Bedrock server, and this device can only host Java Edition."*

Mojang's Bedrock Dedicated Server cannot close it. BDS is a glibc ELF and
Android is bionic — the staged runtimes come from Termux for exactly that
reason — and its licence forbids redistribution, which non-negotiable #9 would
require, because API 29+ blocks `exec` from writable storage and an executable
therefore has to ship inside the APK.

PowerNukkitX is a Bedrock server written in Java, LGPL-3.0. Everything it needs
already existed here.

### What is built, and what is not

Everything below runs in the 725 host-native core tests and the 169 FFI tests,
and the Android host compiles against it. **No PowerNukkitX process has ever
been started on a phone.** M0 in the plan — does it boot on bionic, does
JNA/OSHI blow up, does a client join — has not been run, and until it has, this
subsystem is code that is verified and not proven.

## The game type

`minecraft/native-powernukkitx`, and it is Pumpkin's shape exactly: a game type
of its own, because this is different server software rather than a loader on
top of some. That also makes it immutable in the place that matters — a world
written by PowerNukkitX is a LevelDB Bedrock world, and no Java engine can open
it.

`native-bedrock` is untouched. Both phones still refuse it, the desktop still
serves BDS, and nothing here changes what a Bedrock server *is*.

Two spellings are accepted (`powernukkitx` and `native-powernukkitx`), the same
way `is_pumpkin` and `is_bedrock` take both, and the test is
`hosting::is_nukkit` rather than a comparison in any host.

## `minecraft/nukkit.rs`

Everything PowerNukkitX-specific in the core. It was read out of the
PowerNukkitX source rather than inferred, and the module docs say so key by key,
because four of these were nearly wrong:

| | |
|---|---|
| The config file is **`pnx.yml`** | Not `server-settings.yml`. `PowerNukkitX.java` decides whether to run its interactive setup wizard by asking whether that exact file exists — so writing it is also what keeps a phone from sitting at `starting` forever |
| Keys are **kebab-case** | okaeri renders `maxPlayers` as `max-players`, and saves with `withRemoveOrphans(true)` — a key it does not recognise is deleted on the next boot. A typo does not fail loudly, it stops taking effect |
| `gamemode` and `difficulty` are **integers** | Not the words `server.properties` uses |
| Online mode is **`settings.xbox-auth`** | |
| The seed is **not in `pnx.yml`** | It lives per-world in `worlds/<name>/config.json` |

### The settings merge

`merge_settings` is [`crate::properties::merge`](../rust/homerun-core/src/properties.rs)
one nesting level down: the keys this host owns are replaced, and comments,
ordering, unknown keys and PowerNukkitX's own nested blocks survive verbatim.
PowerNukkitX rewrites this file on every boot, so the merge is always merging
into what it last wrote.

Two rules keep it from corrupting the file:

- A key is replaced only at **exactly two spaces of indent** and only when it
  already carries a scalar. That is what stops `network-settings.rate-limit`'s
  children — four spaces in — being mistaken for keys of the category, and stops
  a sub-block header being overwritten with a value.
- A managed key the file lacks is inserted **at the end of its own category's
  block**, never appended to the document. A repeated top-level key is a
  duplicate mapping key and SnakeYAML rejects the file outright.

### The world, and the seed

`Server.generateLevel` prefers an existing `worlds/<name>/config.json` over
anything passed to it, and invents a random seed when there is none. So a
requested `LEVEL_SEED` or `LEVEL_TYPE` is written into that file, and **only**:

- before the first launch, never over an existing one — after generation the
  file describes a world that exists, and rewriting it with a different seed
  generates a *different world* into the same directory while the first is still
  on disk;
- and only when the player actually asked for something. A default launch writes
  no `config.json` at all, which keeps the hand-built JSON off the path of every
  ordinary server.

`LEVEL_TYPE` maps `DEFAULT`→`normal` and `FLAT`→`flat`, on the overworld only —
flat means a flat overworld, not a flat universe. **`LEGACY` has no equivalent**:
it is Bedrock's finite 512×512 world, which PowerNukkitX cannot generate, so it
becomes an ordinary infinite world. A player who picked it gets a world; what
they do not get is the border.

A worded seed goes through Java's `String.hashCode`, which is what Minecraft
does with a typed seed and therefore the only answer that makes "the seed from
my other server" work.

### Identities

`required_lookups` returns **empty**, always. A Bedrock player is an Xbox
gamertag, `ops.txt` and `white-list.txt` hold plain names one per line, and a
UUID resolved against Mojang would match nobody — so returning names would make
the host issue requests whose answers are wrong.

Bans are appended, never rewritten, for the same reason the Java list is: an
in-game `/ban` lands in the file and no sync ever sees it. The format is a JSON
*array* of `{name, creationDate, source, expireDate, reason}` — not the shape a
Java server writes into a file of the same name.

## The launch

`Core.isNukkit(gameType)` forks `JavaServerBackend` at one point, into
`prepareNukkit`. Everything about the *shape* is the same — download, pick a
runtime, read a `Main-Class`, spawn — so it produces the same `Launch` the other
three shapes do and nothing downstream knows the difference. What it skips is
everything about Java servers: no modpack, no loader, no installer, no argfile,
no mods, no plugins, no EULA.

**The command line is the core's**, from `minecraft.jvm.launch` with a game
type:

```
--skip-setup --accept-license --disable-ansi --language eng
-DdisableSentry=true
```

Every one is load-bearing. Without `--skip-setup`, a first boot runs a setup
wizard reading off stdin. Without `--accept-license` — *even with*
`--skip-setup` — it still prints the LGPL and waits for an answer. Either way a
phone sits at `starting` forever with a healthy process that will never announce
itself.

`-DdisableSentry=true` is a privacy requirement, not a preference: PowerNukkitX
ships Sentry auto bug reporting **on**, and bStats-style metrics **on**. Both
are forced off, at the command line *and* in the config, because a config file a
world restore can overwrite is not a guarantee.

**Java 21, exactly.** Through `Core.selectRuntimeFor(21, "PowerNukkitX", …)`,
which already existed for a Java requirement with no Mojang artifact behind it.
25 removes the `sun.misc.Unsafe` memory access Netty and fastutil still reach
for.

**No EULA.** `AcceptEula` is in every launch plan on purpose (`launch.rs` says
why), so "there is no EULA" is said by the core answering with an **empty**
`eulaFile` rather than by `plan()` growing a branch. The host writes nothing
when the name is empty; a host that wrote it unconditionally would create a file
called `""`.

## The jar

`ServerJar.ensureNukkit`. Resolution differs — one GitHub release with one
asset, not a Mojang manifest and not a loader — and everything after it is the
shared path: the digest, the shared cache keyed on it, the resumed download, the
marker, the offline fallback to the jar already on disk.

It is cached under the loader key `powernukkitx`, which is deliberately not one
of `jar::Loader`'s: that enum is what `TYPE` parses into, and this is not
something anyone can put on a Java server. It only has to be a name no Mojang
jar shares, so a PowerNukkitX 3.0.3 and a Minecraft 3.0.3 cannot collide.

### Updating without a store release

**The jar is data, so a new PowerNukkitX reaches players with no Play release.**
That is what makes the download-never-bundle decision worth more than its 60 MB.

`nukkit::release` picks the newest **stable** release — drafts and prereleases
are skipped, ordered by `published_at` rather than by array position or by
reading the tag as a version. A moved version is logged to the console, because
that line is the only evidence a player has that their server changed underneath
them.

`blessed` is the API's pin, taken from the server's `VERSION`. `LATEST` or
nothing means newest; a concrete tag means that release and no other. **That is
the safety valve**: with nothing between PowerNukkitX publishing and every phone
running it, a release that eats worlds is stopped by a field change on the
server rather than by shipping a build through store review.

What still needs a store release is the *rules* — the `pnx.yml` keys, the
console patterns, the program arguments, the required Java major. The same
coupling Paper and Mojang already have here.

## The tunnel

Almost free, because the crossplay work got here first. `WireProxy.render` used
to take an `exposure = "java"` default that nothing ever overrode — right while
every server this device hosted spoke TCP, and a silent failure the moment one
did not. Crossplay needed the Bedrock UDP forward alongside the Java TCP one, so
it threaded the exposure through `TunnelSession` to `render` and put the mapping
in the core as `minecraft::exposure_for`.

All this game type adds is one clause in that mapping — and it is the one entry
there that is not obvious from its name, because what decides an exposure is the
protocol the *client* speaks, not what the server is written in. PowerNukkitX is
a Java process serving Bedrock players over UDP.

A PowerNukkitX config is one section different from a Java one:

```ini
[UDPServerTunnel]
ListenPort = 19132
Target = 127.0.0.1:<the port PowerNukkitX actually bound>
```

`ListenPort` is **19132 always** — the gateway DNATs player traffic there on the
WireGuard interface, and only `Target` follows the local port. `[UDPServerTunnel]`
exists only in `hintjen/wireproxy-fork`; upstream has no inbound UDP tunnel at
all. No voice forward: PowerNukkitX loads no Java mod, so `LISTEN_VOICE` has
nothing behind it.

### Bedrock ignores SRV, and that reaches past the tunnel

Java servers are addressed by a flat, port-less name and the client follows an
`_minecraft._tcp` SRV record to the regional gateway and pinned port. **Bedrock
clients do no SRV lookup**, so a player has to be handed an explicit host *and*
port — the Bedrock client's Add Server dialog has a Port field, and they will
have to use it.

The tunnel is unaffected; the *address a player types* is not. The rule lives on
the API in `Link.player_connect_address(game_type)`, and a game type that falls
through to its `else` branch is handed a port-less SRV name no Bedrock client
can resolve. See the API work below.

## The console

`console::joined` and `left` learned a second vocabulary rather than gaining a
branch. PowerNukkitX writes `Ada[/10.0.0.4:52134] logged in with entity id 12 at
(…)` — the name is not adjacent to the marker, an address sits between them — so
`player_before_address` reads that shape.

The forgery guard from the vanilla parser applies and is why the marker is
checked *after* the address rather than anywhere in the line: chat is
`<Ada> hello`, so a griefer typing `Griefer[/1.2.3.4:1] logged in with entity id
5` produces a line whose text before `[/` is `<Ada> Griefer`, which fails the
name test on the angle brackets.

Names may contain **spaces** here and cannot on a Java server. A Bedrock identity
is an Xbox gamertag and legacy gamertags have them.

The ready line needed nothing: `Done (1.234s)! For help, type "help" or "?"` is
already what `is_ready` matches.

`LineMeaning.announced_version` is new and additive. `Starting Minecraft: BE
server version v1.21.100` is the **only** honest source for the Bedrock version:
the jar carries a PowerNukkitX release number, and the release metadata says
nothing about Minecraft. The core extracts it and `Core.Line` carries it; no
Android surface displays it yet.

## What did not need changing

Longer than the list that did:

- **Backups.** `BackupManager.WORLD_DIRS` is `["world", "worlds"]` and
  PowerNukkitX writes `worlds/<name>/`, so `hasLocalWorld` — the input to the
  restore decision, the on-stop backup gate and the restored-root walk — is
  already right. restic backs up the server directory whole, so the LevelDB
  world, the YAML and the name files ride along with no include list to
  maintain.
- **The backup lease.** Nothing in it is game-specific.
- **Moderation and Insights.** Android uses no RCON for either — player tracking
  reads the console and `native-server-rcon` dispatches through the console
  command path — so this needs its console vocabulary and nothing else. Whether
  PowerNukkitX implements RCON is irrelevant here.
- **The stop ladder.** `stop` on stdin, then the ladder the supervisor already
  walks. The *timings* are unverified: a Bedrock world save is not a Java one.
- **`ServerHost.select`.** It is served by `Served::Jvm`, so the routing that
  sends a Java server to the Java backend sends this one there too.

## The wizard

`homerun-app-ui` owns every screen. The tile is titled **PowerNukkitX** — named
for what it is, the same call the Pumpkin tile makes, because finding out months
later from a missing feature is worse than reading an unfamiliar name once. The
word Bedrock is in the description, which is what says what the thing does.

It sits **last in `serverTypes`**, so on Android the list reads Java Vanilla,
Mods & Plugins, Modpack, Pumpkin, PowerNukkitX.

The gate is a capability, `powernukkitxServers`, absent-means-false — not
`serverBackends`, for the reason the comment above the Pumpkin gate already
gives: that list advertised `pumpkin` on Android for months before anything
could route to it. Where it is true the **Bedrock tile comes off**, because a
host that serves Bedrock-style servers this way has no BDS to offer and two
names for one thing is worse than one.

**No version step.** It takes the Bedrock *family* — the same gamemodes, the
same level types, the same tick distance, the same `buildBedrockEnv` — but its
version is whatever the release implements rather than anything a player picks,
so `VERSION` stays `LATEST` and the host resolves the newest blessed release.
`native-get-latest-bedrock-version` is about BDS and still answers with a
refusal on a phone; nothing calls it for this type.

## What the API still has to learn

Not in this repo, and **the first one fails silently**:

1. **`Minecraft.create` picks the forwarded port from the game type**
   (`api/homerun/minecraft/models.py`): `("bedrock", "native-bedrock")` gets
   `19132/udp`, everything else `25565/tcp`. A game type it does not recognise
   is treated as Java, so the link is provisioned with a TCP forward and the UDP
   tunnel carries nothing.
2. **`GAME_TYPE_CHOICES`** gains `native-powernukkitx`. `NATIVE_GAME_TYPES`
   derives from it by the `native` prefix, so that half is free. The column is
   `max_length=20` and the value is 19 characters.
3. **`NO_JAVA_PLUGIN_GAME_TYPES`** gains it: no Geyser, no voice chat, so those
   optional ports would be a promise the server cannot keep.
4. **`Link.player_connect_address`** and `resolve_public_server_fqdn` treat it as
   Bedrock-addressed. (`resolve_public_server_fqdn` tests `game_type ==
   "bedrock"` only today, so `native-bedrock` public servers already miss the
   Bedrock branch.)

## File map

| File | What it holds |
|---|---|
| `rust/homerun-core/src/minecraft/nukkit.rs` | Everything PowerNukkitX: the `pnx.yml` merge, the world config, the name files, the ban list, release resolution |
| `rust/homerun-core/src/minecraft/hosting.rs` | `is_nukkit`, `Host::nukkit`, the `serves` arm |
| `rust/homerun-core/src/minecraft/mod.rs` | The PowerNukkitX clause in `exposure_for`, and the four `Game` methods branching on the game type |
| `rust/homerun-core/src/minecraft/jvm.rs` | `NUKKIT_PROGRAM_ARGS`, `NUKKIT_JVM_OPTIONS` |
| `rust/homerun-core/src/minecraft/console.rs` | `player_before_address`, `bedrock_version` |
| `rust/homerun-core/src/game.rs` | `config_inputs_for`, `LineMeaning::announced_version` — both additive |
| `rust/homerun-pumpkin-ffi/src/core_dispatch.rs` | `minecraft.exposure`, `minecraft.hosting.isNukkit`, `minecraft.nukkit.release`, the game type on `jvm.launch` and `game.configInputs` |
| `android/.../ServerJar.kt` | `ensureNukkit`, and `place` — the half of `ensure` that does not care what published the jar |
| `android/.../JavaServerBackend.kt` | `prepareNukkit`, the skipped EULA, the skipped mod and plugin sync, the exposure on the tunnel |
| `android/.../WireProxy.kt`, `TunnelSession.kt` | The exposure, threaded to `tunnel.render` |
| `android/.../HostCapabilities.kt` | `powernukkitxServers` |

## Triage

**The server sits at `starting` and never announces itself.** Look for a licence
prompt or a setup wizard in the console. Both `--skip-setup` and
`--accept-license` are required and either one alone leaves it waiting on stdin.

**`UnsatisfiedLinkError` during boot.** JNA, reached through OSHI. The jar ships
57 `.so` files and every one is glibc, macOS or Windows — none bionic. Metrics
being off removes the known boot-path caller; if `network/NetworkInterface`
touches it anyway, extract `libjnidispatch.so` from JNA's Android AAR and set
`-Djna.boot.library.path`. **This is the unresolved risk and it is what M0
exists to answer.**

**A setting had no effect.** okaeri saves `pnx.yml` with
`withRemoveOrphans(true)`, so a key it does not recognise is deleted rather than
rejected. Check the spelling against `config/category/*.java` — the field name in
camelCase, the key in kebab-case.

**The seed was ignored.** Only written before the first launch, and only into
`worlds/<name>/config.json`. If the directory already existed, the file was left
alone on purpose.

**The server runs, the card is green, nobody can join.** Two candidates, and the
console does not distinguish them. Either the tunnel is TCP — check the rendered
`wireproxy.conf` for `[UDPServerTunnel]` and `ListenPort = 19132` — or the API
provisioned a `25565/tcp` forward because it does not recognise the game type.

**A player cannot find the server from a hostname.** Bedrock does no SRV lookup.
They need the host *and* the port, in the Add Server dialog's two fields.

**Mods or plugins are missing.** They are not synced for this game type and never
will be: PowerNukkitX loads no Fabric mod and no Bukkit plugin, and `mods::sweep`
deletes jars it does not recognise — which on this engine is all of them.

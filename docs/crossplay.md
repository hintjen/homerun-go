# Crossplay on Android

## Overview

A **crossplay** server (`game_type: native-crossplay`) is a Java server that
Bedrock Edition clients can also join. It is an ordinary Paper server plus two
plugins:

- **Geyser** listens on a UDP port, speaks the Bedrock protocol, and translates
  each Bedrock session into a Java one against the server it is running inside.
- **Floodgate** lets those translated sessions in without a Mojang account, so
  the server keeps `online-mode=true` and Java players still authenticate
  normally.

Both run **inside the server's own JVM**, as plugins. There is no second
process. The desktop does this differently — it runs Geyser *Standalone*, a
separate JVM with its own config, lifecycle and orphan-process handling — and
on a phone that would be memory that is not there and another thing to keep
alive against the OOM killer.

## What was here before

Crossplay was offered, launched without complaint, and did not work.

The wizard showed it (`step-server-type.tsx` gates on `jvmHost &&
moddedServers`, both true on Android), `hosting.rs` routed it to the JVM, and
`Minecraft.create` forwarded `19132/udp` at the gateway — so the overview
header displayed a **Bedrock:** address. Nothing installed Geyser. The address
went nowhere, and the tunnel would not have carried it anyway: `WireProxy`
never passed an `exposure`, so it forwarded one TCP port and nothing else.

`plans/android-parity.md` claimed crossplay was "capability-gated off on
Android". It never was.

## The shape

Nothing in the launch is special-cased. Three ordinary steps each answer
"nothing to do" for every other server, so they run unconditionally:

| Step | Where | What crossplay adds |
|---|---|---|
| Resolve mods | `ModInstaller.sync` | `crossplay::merge_projects` folds `geyser` into `MODRINTH_PROJECTS` before the resolver runs |
| Fetch what Modrinth lacks | `CrossplayInstaller.sync` | the Floodgate **Spigot** jar, from GeyserMC's API |
| Open the tunnel | `TunnelSession.open` | `exposure = "crossplay"`, so the Bedrock UDP forward exists |

```
  create      game_type = native-crossplay, TYPE = paper
  launch
    core      crossplay::merge_projects  → "geyser"
    host      ModInstaller resolves and installs it from Modrinth
    host      CrossplayInstaller fetches floodgate-spigot.jar
    core      crossplay::config          → plugins/Geyser-Spigot/config.yml
    host      writes it, if it is not already there
    core      exposure_for(game_type)    → "crossplay"
    host      wireproxy gets a [UDPServerTunnel] on 19132
    JVM       Paper starts → Floodgate loads → Geyser loads after it
```

### Geyser is derived at launch, never stored

Nothing writes Geyser into the server's environment. The game type implies it,
and the implication is drawn on every launch.

That is deliberate and it is load-bearing twice over. A server created before
any of this existed starts working on its next launch with no migration and
nothing to recreate — **verified: the server this was proven on has
`MODRINTH_PROJECTS = NULL` in the database.** And a UI bundle, which is
replaced over the air, can never become the thing that decides what a crossplay
server is.

## `crossplay` — `rust/homerun-core/src/minecraft/crossplay.rs`

Every decision, and no I/O. The host fetches bytes and writes files; it decides
none of the below.

| Function | Answers |
|---|---|
| `is_crossplay` | is this the game type that wants a Bedrock bridge |
| `merge_projects` | `MODRINTH_PROJECTS` with the crossplay plugins folded in |
| `floodgate` | which GeyserMC flavour to fetch, or `None` |
| `floodgate_build` | that flavour's URL, filename and SHA-256, out of the build metadata |
| `config` | what to seed Geyser's own configuration with |

`minecraft::exposure_for` sits beside it in `mod.rs`: it turns a game type into
the `exposure` string `tunnel.render` expects. **Derived from the game type and
nothing else** — a crossplay server's `TYPE` is an ordinary loader, so a host
inferring this from the loader builds a Java-only tunnel for a server sold as
crossplay.

### Why Paper and not Fabric

The API's *hosted* crossplay path uses Fabric, because Modrinth publishes
Geyser, Floodgate and Fabric API for it and one resolver gets all three. That
is the wrong trade here. Checked against Modrinth on 2026-08-21:

| | Geyser | Floodgate |
|---|---|---|
| **paper** | 657 builds; the latest spans `26.1, 26.1.1, 26.1.2, 26.2` | **none** — fabric and neoforge only |
| **fabric** | 1015 builds; the latest spans `26.2` alone | yes |

One Paper build of Geyser covers a range of Minecraft versions; the Fabric build
pins to exactly one. On Fabric, a Minecraft release Geyser has not published for
*that day* breaks crossplay outright. Paper also skips the loader-installer run,
which on a phone is a minute of work and another thing to fail.

The cost is that Floodgate needs its own downloader. Taken.

### `merge_projects` is a merge, not a list

A server that came from the desktop may already carry a pinned
`geyser:<versionId>`. Appending a bare `geyser` beside it puts two entries for
one project into the resolver, which installs the plugin twice under two
filenames — and Bukkit refuses to load the second. The pin wins, because it is
the more specific instruction and because silently overriding what a player
chose is not the core's business.

It splits with `mods::split_list`, which is public **for this reason**: two
splitters that disagreed would let exactly that duplicate through.

### The config is a seed, not a sync

```yaml
bedrock:
  port: 19132
java:
  auth-type: floodgate
```

Two keys, and the host writes the file **only when it is absent**. Geyser reads
its config through Configurate, which fills every key it is not given from the
defaults and then rewrites the file fully expanded on first start. Dropping a
two-key partial back over that on the next launch would hand Geyser a config
with no `config-version` and invite a migration nobody has tested — and there is
nothing to correct anyway, because nothing on a phone can edit the file in
between.

- **`bedrock.port`** is written even though it is also Geyser's own default. The
  gateway DNATs to this port and the wireproxy `ListenPort` is fixed to it, so
  the coupling is real; writing it makes the coupling greppable instead of a
  coincidence that breaks silently if Geyser ever changes a default.
- **`auth-type`** is what lets a Bedrock player in without a Mojang account.
  Geyser `softdepend`s on Floodgate and is documented to detect it, so this may
  be redundant — but the failure mode of being wrong is `online`, which rejects
  every Bedrock join with an authentication error that reads like a network
  problem.

`bedrock.address` is **not** written: the default binds every interface, which
is what the tunnel's `127.0.0.1` target needs. Nothing under `java:` beyond the
auth type, either — a plugin-mode Geyser is inside the server it fronts and
finds `plugins/floodgate/key.pem` itself.

## `CrossplayInstaller` — `android/…/CrossplayInstaller.kt`

The half of crossplay Modrinth cannot supply, plus the config seed.

**Floodgate has no Paper build on Modrinth.** Asking the resolver for one
returns nothing and installs nothing without complaining, which is the entire
reason this file exists. The Bukkit-family jar comes from GeyserMC's download
API instead — two requests, because the metadata names the build *and* carries a
SHA-256 and the canonical filename, so the download is verified and the
destination name is GeyserMC's rather than ours.

**Nothing here can fail a launch.** `PluginInstaller` draws the opposite line
and the difference is what the jars are for: a minigame plugin *is* the server,
while a crossplay server without Floodgate is still a working Java server on its
usual address and only the Bedrock players lose out. Refusing to start would
take the game away from everybody to punish a download.

**The destination filename is stable across builds.** `mods::sweep` only ever
deletes files it installed itself, so a jar fetched here is never cleaned
up — under a versioned name every update would leave the previous build beside
the new one and Bukkit would refuse the duplicate plugin.

## The Bedrock port is a constant

There is no port probe, and that is a decision rather than an omission.

The desktop probes from 19132 upward because it can run several servers at once,
each wanting its own Geyser. Android declares `multipleRunningServers: false`,
so there is one server, one Geyser, and nothing to collide with. The gateway's
`ListenPort` is fixed at 19132 and Geyser's default is the same number.

A probe would buy nothing and cost the one failure this feature cannot afford:
the port has to match in **two** places — `crossplay::config`'s `bedrock.port`
and `renderWireproxy`'s `geyserPort` — and two places choosing independently is
how they come to disagree. A mismatch is a server that starts, logs nothing
wrong, and cannot be joined. So `WireProxy.render` passes `geyserPort = null`
and lets the core default it.

If another app on the phone holds 19132, Geyser fails to bind and says so, and
the Java server is unaffected.

## What the gateway does

`Minecraft.create` forwards both ports for `native-crossplay` unconditionally
(`api/homerun/minecraft/models.py`), so the link carries an external port for
`19132/udp` from the moment the server exists:

```
forward_ports: {"minecraft": ["20084:25565/tcp", "20085:19132/udp"]}
```

The **external** port is what a Bedrock player dials, and it is not 19132. The
API serialises it as `config.links[0].additional_forwards.geyser` and the
overview header shows it as the **Bedrock:** address.

Two things follow that surprise people:

- **`geyser_enabled` is `False` on a working crossplay server.** That flag
  drives the *optional* port for a plain Java server whose owner toggled Geyser
  on. `ServerSerializer.update` skips it for `native-crossplay` on purpose —
  create already forwarded the port, and setting it again would be a second
  source of truth.
- **Bedrock ignores SRV records**, so the Bedrock address is always
  `<gateway host>:<external port>` while the Java address may be a flat SRV
  name. `Link.player_connect_address` is the single source of truth for both.

## Verified on hardware

Pixel 9 Pro XL against staging, 2026-08-21. Server `mossy-mill`,
`minecraft.fractalnetworks.co:20085`.

Geyser and Floodgate installed from their two different sources and loaded in
the right order:

```
[floodgate] Took 919ms to boot Floodgate
[Geyser-Spigot] Loading Geyser version 2.11.2-b1228 (git-master-a748e49)
[Geyser-Spigot] Started Geyser on UDP port 19132
[Geyser-Spigot] Done (3.817s)! Run /geyser help for help!
```

A Bedrock client joined, through Floodgate:

```
[Geyser-Spigot] Player connected with username elPTFO (2168)
[Geyser-Spigot] elPTFO (logged in as: elPTFO) has connected to the Java server
[floodgate] Floodgate player logged in as .elPTFO (UUID: 00000000-0000-0000-0009-01f96f719a2b)
```

The `.` prefix and that UUID are Floodgate's signature — high 64 bits zero, low
64 bits the player's Xbox XUID — so `auth-type: floodgate` was genuinely in
effect. `xuidToFloodgateUuid` in the desktop repo formats the same value.

**`(2168)` is the client's Bedrock protocol, and Geyser accepted it while
advertising 2169.** The number in the server-list pong is the newest version
Geyser knows, not a requirement; a client one version behind is fine. Do not
read a protocol difference there as the cause of a failed join.

## Triage

Work outward from the phone. Each step tells you which of four different bugs
you have.

### Are the jars there?

```sh
adb shell run-as app.gethomerun.mobile.debug ls files/servers/<id>/plugins/
```

Expect `Geyser-Spigot.jar`, `floodgate-spigot.jar`, and — once the server has
run — the `Geyser-Spigot/` and `floodgate/` data directories.

| Missing | Cause |
|---|---|
| both jars | the core answered "not crossplay". The game type is not reaching the backend — check `gameType = settings?.rawGameType` in `BridgeRouter`, not the reduced form |
| Geyser only | Modrinth resolution failed; the console names every mod that did not make it |
| Floodgate only | the GeyserMC fetch failed. Non-fatal by design, so the launch continued — look for `Could not install Floodgate:` on the console |
| the data dirs | the plugin was installed but never loaded. Read `logs/latest.log` for the loader's complaint |

### Is Geyser listening?

```sh
adb shell cat /proc/net/udp | grep 4ABC     # 0x4ABC = 19132
```

`00000000:4ABC` is `0.0.0.0:19132`. If it is absent, `logs/latest.log` will say
why — most likely another app holds the port.

### Is the tunnel carrying it?

```sh
adb shell run-as app.gethomerun.mobile.debug cat files/servers/<id>/wireproxy.conf \
  | grep -vi "privatekey\|presharedkey"
```

> **Never print that file unfiltered.** It contains a WireGuard private key.

Expect, alongside the TCP tunnel:

```ini
[UDPServerTunnel]
ListenPort = 19132
Target = 127.0.0.1:19132
```

If it is missing, `exposure` did not reach `renderWireproxy` — the server is
running as a Java-only server and no Bedrock player can reach it.

### Is the gateway forwarding?

A RakNet unconnected ping to the **external** port answers in one step. Sending
`0x01`, a timestamp, the 16-byte magic and a client GUID should return a `0x1C`
pong:

```
MCPE;<motd>;2169;26.45;0;20;<serverGUID>;Another Geyser server.;Survival;1;19132;19132;
```

A pong proves gateway → wireproxy → Geyser end to end. A timeout means the
packets are dying before the phone, and `logs/latest.log` will have nothing at
all for the attempt — which is how you tell this apart from every failure above.

> **Ping the external port, not 19132.** `minecraft.fractalnetworks.co:19132`
> times out on a perfectly healthy server; the port is the one in
> `additional_forwards.geyser`.

### The client reaches Geyser and drops

`logs/latest.log` shows

```
Bedrock user with ip: /127.0.0.1 has disconnected for reason §rBedrock client disconnected
```

with no `Player connected` line. Geyser saw the connection and the *client* gave
up, so the fault is between the client and Geyser rather than in any of the
above. Retry before investigating: this was seen once during verification and
did not reproduce.

### One unexplained failure, recorded honestly

During verification a first Bedrock attempt failed with a client-side
`InitialConnection-41` and **left no line in Geyser's log at all** — the packets
never reached the phone. Two minutes later, with nothing changed on the device or
the server, the same client connected and played.

Nothing on the phone can explain that, which points upstream at when the
gateway's DNAT for the external UDP port actually starts forwarding. It is not
diagnosed. If a cold start reproduces it — stop the server, start it, try
Bedrock immediately — it belongs in the gateway, or in a "still setting up" hint
in the UI.

## File map

| File | Role |
|---|---|
| `rust/homerun-core/src/minecraft/crossplay.rs` | every decision: which jars, from where, what Geyser is told |
| `rust/homerun-core/src/minecraft/mod.rs` | `exposure_for`, and `Minecraft::forwards` which renders it |
| `rust/homerun-pumpkin-ffi/src/core_dispatch.rs` | the five `minecraft.crossplay.*` and `minecraft.exposure` arms |
| `android/…/CrossplayInstaller.kt` | the Floodgate fetch and the config seed |
| `android/…/ModInstaller.kt` | folds the crossplay slugs in before resolving |
| `android/…/WireProxy.kt`, `TunnelSession.kt` | carry `exposure` to the tunnel |
| `android/…/JavaServerBackend.kt` | where the three steps sit in a launch |

## Not here yet

- **iOS.** Pumpkin loads no Bukkit plugin, so `hosting.rs` refuses crossplay
  there and will keep doing so.
- **The desktop**, which keeps Geyser Standalone. Converting it to plugin mode
  is a real simplification and entirely separate work.
- **The `native` server Geyser toggle.** The resources tab offers "Bedrock
  Players" on a plain `native` server; it installs the Geyser plugin and nothing
  else — no Floodgate, no config, no UDP forward. Everything it needs now
  exists, but wiring it up also has to move `save_optional_ports` and the
  gateway forward around, so it is a separate change.
- **A Bedrock-side health signal.** The overview header shows the Bedrock
  address whenever the game type is `native-crossplay`, without checking that
  Geyser is actually bridging.

## See also

- [`android-mods.md`](./android-mods.md) — the resolver Geyser arrives through,
  and the sweep rule that makes the Floodgate jar safe.
- [`core-bridge.md`](./core-bridge.md#minecraft) — the method signatures.
- [`android-server-backend.md`](./android-server-backend.md) — where these steps
  sit in a launch.
- [`../plans/android-crossplay.md`](../plans/android-crossplay.md) — the plan,
  including what was dropped and why.

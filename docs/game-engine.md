# The game engine — running a server from its descriptor

## Overview

`homerun-core::engine` is one implementation of "fetch, prepare, launch,
observe, stop a game server", driven by a `game.json`. Adding a game is a
descriptor and a docs page, and for most games it is not code. What only one
game needs lives in that game's *extension* (below); the schema grows when a
**second** game needs the same thing, and every later game gets the growth.

It comes in two halves. **The decisions** are pure, like the rest of
`homerun-core`: no sockets, no processes, no filesystem, no clock. **The
effects** — fetching a runtime, unpacking it, driving steamcmd, speaking
RCON, asking the operating system which ports a process bound — live in
`homerun-supervisor` behind a default-off `game-engine` feature. The two are
joined by the `engine.*` bridge namespace.

That split is what lets the whole engine be tested with no game installed:
the decisions run in milliseconds on any machine, and the effects are tested
against real sockets, real child processes and real archives on loopback.

Source: `rust/homerun-core/src/engine/`.

**Naming.** *The runner* is our own executable (`homerun-game`): CLI and
supervisor. A *game's server* is the vendor's binary the runner downloads.
Never confuse the two — Homerun does not build, host or redistribute a game's
server.

## Why this is not an implementation of `game::Game`

It would be the obvious move and it is the wrong one.

`Game` is frozen at `game/v1` and deliberately excludes artifact resolution.
Its own header names a Steam depot as the example of why: Minecraft resolves a
jar from Mojang's manifest, another game might resolve a depot, a container
image, or nothing at all because it ships in the app, and those have no honest
common signature. A descriptor-driven game resolves a Steam depot.

Widening the trait to fit would break the Android and iOS hosts at once, and
break them *silently* for anyone who has not rebuilt — the bridge resolves
methods by string at runtime, not by symbol at link time.

So this is a sibling of `game`, not a subclass of it: its own module, its own
`engine.*` namespace, and no change to `game.*` at all. Minecraft keeps the
trait; descriptor-driven games get this.

## The rules that are about safety

Each is enforced in code, and each exists because the working behaviour and
the safe behaviour are not the same thing.

### Substitution is single-pass, and setting values are never templated

`engine::template` replaces a placeholder with its value and **never scans
that value again**. It is opaque text from the moment it lands.

The reason is immediate. A player names their server:

```
{secret:rcon}
```

If substitution re-scanned substituted text, the next pass would resolve that
placeholder and the server's RCON password would become its public hostname —
advertised in a server browser, printed to a log, visible to everyone who can
see the server at all. There is a test named for exactly that value, and it
asserts the password appears nowhere in the output rather than merely that the
output is some expected string.

The same reasoning is why a *setting's* value is never templated. Only the
descriptor's own strings — argv, env, config values — go through the
templater, and those are authored by us and shipped inside a signed host
build. The one exception is a setting's `default`, which is also
descriptor-authored, and which may contain `{serverName}` and nothing else;
`engine::settings` resolves it before the value becomes player data, and
`engine::validate` refuses a default that contains anything more.

Two consequences fall out of the same rule and are enforced by
`engine::validate`:

- **`launch.exe` may not be templated.** `"exe": "{setting:binary}"` would let
  whoever creates a server choose which file on the machine Homerun executes.
- **`client.joinUrl` may not contain a secret.** It is in the servable half:
  the API hands it to any UI, so a `{secret:rcon}` in it publishes the admin
  password.

### A person accepts each game's terms, and nothing infers it

`engine::licence` refuses `fetch` and `start` without an explicit record of a
person's decision. Absent is refused; it is never defaulted to accepted,
anywhere — not from a descriptor, not from a previous server, not from the
fact that the runtime is already on disk.

`licence: null` means the game has no terms of its own. It never means terms
that were accepted. Those are different claims, and keeping them apart is the
whole point of the module.

**Terms are often more than one document, and `licence.documents` is where the
rest go.** Rust's are three -- the Facepunch Terms of Service, the Facepunch
Community Server and Hosting Guidelines, and the Steam Subscriber Agreement --
separately published, with three URLs. A single `{name, url}` records one of
them: the other two survive as prose in the name and their links are lost, so
what a person accepted cannot be resolved back from what was recorded, which is
the one job an acceptance record has. `documents` is a list of `{name, url}`;
an empty one means the terms really are the single document `name` and `url`
describe, which is what every descriptor written before the field meant. Where
it is not empty, `name` and `url` stay the one-line summary a host shows when it
has room for one link, and `url` has to be one of the listed documents --
otherwise the link most people would actually follow is a fourth document
nobody listed.

An agent driving the runner stops and asks a human. If a vendor's own
installer prompts for terms, that is a stop, not a prompt to answer.

### steamcmd is anonymous only

A game whose dedicated server needs a Steam account that *owns* it is out of
scope — not a feature request. There is no account we could use that would not
be either a shared credential or the player's own.

### A player's text is not a switch

`engine::settings::check_text` refuses player text that begins with `+`, `-`
or `/`, contains a `"`, a control character, a backtick, `${` or `$(`, or runs
past 256 characters. It runs on every `string` setting and on the server's own
name, in `resolve`, which is the one call every path makes before a value
becomes an argument — including the CLI's, which has no API in front of it.

Arguments are a `Vec<String>` from `invocation` to `Command::args`, so a space
in a value cannot split it in two. That property is real and it is not enough:

- A `+key value` parser — Valve's, Facepunch's — reads **one argv element**
  that happens to be `+rcon.web` as a new switch rather than as the value of
  the switch before it. A server named `+rcon.web` turns on the web console
  on a server whose player chose the name.
- A game that re-reads the raw command line — Unreal's `-Key=Value`,
  Facepunch.CommandLine — parses what Windows handed it, not the vector Rust
  built. Rust quotes for MSVCRT's rules, and a parser that is not MSVCRT can
  be broken out of with a `"`.
- A control character reaches a log, a properties file, and a console's
  stdin, where a newline is a second command.

**One rule, wherever the value lands.** Not a stricter rule for argv than for
a config file: the same value routinely lands in both, so a per-site rule buys
precision only for a setting used in exactly one place — and which place that
is changes when a descriptor is edited, with nothing telling the player their
name has just become illegal. `{serverName}` decides it outright, being a
Homerun name that exists before a game is chosen and must be judged the same
way for every game.

The cost is named rather than hidden: a name may not begin with `+`, `-` or
`/` and may not contain `"`, so `-=[Clan]=-` is refused where `=[Clan]=-` is
not. The API enforces the same rule, so a player meets it in a form rather
than at a launch that fails. A setting with `options` is exempt — its values
come from the descriptor, which is ours.

## The descriptor — `descriptor.rs`

The Rust types **are** the schema. There is no second spelling: the JSON
Schema is generated from them, the API's registry is generated from the file,
and every other layer reads one of those two.

Two properties look like carelessness and are not:

- **Every field has a default.** Not because a descriptor may omit anything —
  most fields are required and `validate` says so — but because a missing
  field has to fail as a sentence a player can read, and serde's own
  `missing field 'exe' at line 34 column 5` is not that. Parsing accepts
  nearly everything; validation is the only gate, which is also what lets the
  rules be stated once and tested exhaustively.
- **`deny_unknown_fields` is off.** A descriptor written against a newer
  schema must *degrade*, not fail: the host ignores the key it has never heard
  of and runs the game. The alternative is that adding an optional field
  breaks every host in the field that has not been rebuilt — the same freeze
  the `Game` trait carries, arrived at from the other direction.

The one place that rule cannot hold is a tagged enum, because serde has to
pick a variant before it can ignore anything. `RuntimeSource` therefore
carries an explicit `Unknown` variant, so a runtime source this build cannot
fetch is a readable refusal rather than a parse error.

## Settings — `settings.rs`

**Everything arrives as a string.** The API validates settings against the
registry's copy of the descriptor and stores them in
`config.environment_variables`, which is a string map because every existing
env-var code path depends on it being one. So `maxPlayers` reaches this crate
as `"10"`, `pve` as `"false"`, and a setting nobody touched as `""`.

This module is where that stops being true. It coerces each value to its
declared type, and from there down the engine deals in real integers and
booleans.

**An empty string and a JSON `null` are the same answer: unset.** For every
type, including `string`. An empty hostname is a hostname the player did not
set, and it must drop its flag rather than pass an empty argument to a server
that would then advertise itself with a blank name.

It re-validates bounds the API already validated, because the API is not the
only caller: `homerun-game launch --settings maxPlayers=9000` reaches exactly
this code with no API in the picture, and a probe on a developer's machine is
where a bad value is most likely. A backstop that only runs when the front
door was used is not a backstop.

Unlike `validate`, it stops at the first problem: its audience is a player who
set one thing wrong, and the protocol carries one `error.message`.

**A closed set has exactly one spelling**, agreed across three repositories:
`type: "string"` with a non-empty `options` of strings. There is no `enum`
type and there is no second way to say it, so `validate` refuses `options` on
an `int` or a `bool` and refuses a choice that is not text. A small set of
numbers is a string setting whose options are `"1"`, `"2"`, `"4"` — which is
what reaches argv either way. `min`/`max` are `int`-only and ignored
elsewhere, which earns a warning rather than a refusal; an integer that does
not fit an `i64` is refused. An empty `options` list means the same as none.

## Templating — `template.rs`

`{setting:<key>}`, `{port:<name>}`, `{secret:<name>}`, `{serverName}`,
`{serverDir}`. `{host}` is legal only in a join URL, which the API fills in;
the engine refuses it in a launch line so a launch cannot come to depend on an
address this side does not know.

`{{` is a literal `{`. An unclosed brace is text. Anything else between braces
must be a placeholder this module knows, or it is a validation error — never
an empty string. Silently dropping an unknown placeholder is how a server
starts with `+server.hostname` and no name after it.

**An unset setting removes its flag.** `{setting:seed}` with no seed does not
become an empty argument: `+server.seed ""` is not the same launch line as one
with no seed in it. The rule, exactly:

| Token | With `seed` unset |
|---|---|
| `"+server.seed", "{setting:seed}"` | both tokens go |
| `"seed={setting:seed}"` | only that token goes |
| `"+world.offset", "{setting:offset}"` where offset is `-5` | nothing goes; `-5` is a value, not a flag |

A preceding token is dropped only when the dropped token was *solely* a
placeholder, the token before it **in the descriptor** starts with `+` or `-`
and carries no placeholder of its own, and that token is still standing. The
check is against the descriptor's source token, not the emitted argument —
otherwise a negative value would be mistaken for the flag to remove.

"Still standing" is the part that reads like pedantry and is not. Dropping
looked at the last *emitted* argument, which after an earlier drop is some
earlier token entirely: `["-batchmode", "+a", "{setting:n1}", "{setting:n2}"]`
with both settings unset lost `+a` to the first drop and then `-batchmode` to
the second, because `-batchmode` was what the second drop found at the end of
the list. A game launched with neither flag opens a window on a headless
machine, and nothing about that failure points back at a seed nobody set.

The rule cannot tell `+server.seed {setting:seed}` from
`-batchmode {setting:seed}`, where the flag carries no value of its own and
the seed is a bare positional — that descriptor really does lose `-batchmode`
on every launch without a seed. It is inherent to the rule, so `validate`
warns about it instead: quiet when the flag names the setting, which is how
descriptor authors spell a flag-and-value pair, and loud when it does not.
A guess about spelling, hence a warning and never a refusal.

An environment variable has no equivalent: a variable set to the empty string
is a different thing from one that is not set, so an unset setting leaves its
variable absent.

A **config file's** managed key does have an equivalent, and it is removal.
The key reflects the setting, so a setting that went from set to unset takes
its key out of the file rather than leaving the value from the launch before —
a cleared seed that kept generating the old world is the failure that decided
it. `properties::remove` and the JSON branch of the runner's `prepare` do
that; everything unmanaged in the file survives either way.

### A private port stays on this computer, and it is checked

`expose: false` says a port is not published through the gateway. That was a
promise nothing kept: `prepare` validated the bind address and then dropped
it, and `platform::Listening` carried a protocol and a port with no address,
so `127.0.0.1:28016` and `0.0.0.0:28016` were the same observation. An RCON
console could end up on the LAN behind one password and every check passed.

Three things together make it real:

- **`{bindAddress}`** hands the address to the game. A descriptor that never
  uses it is a descriptor whose server binds wherever it likes, and
  `validate` warns about one that has an administrative console.
- **`Listening` keeps the address** it observed, on all three platforms. The
  Linux `/proc` tables write it as host-order words, so `0100007F` is
  `127.0.0.1` and not `1.0.0.127` — reading it the obvious way gives a
  plausible address that is not the one bound.
- **The runner refuses a launch** where a port declared `expose: false` is
  observed on anything but loopback: it stops the server through the normal
  ladder and sends `port_exposed`. `::ffff:127.0.0.1` counts as loopback,
  which `Ipv6Addr::is_loopback` does not say on its own.

**Exposed ports may bind wider, deliberately.** The tunnel targets loopback,
so a published port has no *need* to be on `0.0.0.0` — but games routinely
bind every interface for one with no way to be told otherwise, and refusing
that would refuse most of a catalogue over a port that is meant to be
reachable. What is worth stopping a server over is the private one.

Today `{bindAddress}` is always `127.0.0.1`; the runner refuses any other
value. Widening that is a contract change rather than a flag.

## Ports and the gateway — `ports.rs`

Three numbers, and two of them are never the same:

- the descriptor's `port` — what the server prefers to bind, **and** the
  gateway's dest port
- the **bound** port — where it actually landed, which moves when the
  preferred port is taken. This is the forward's target, and what `{port:…}`
  resolves to
- the **public** port — allocator-assigned by the gateway, never requestable,
  and therefore never equal to the game port

A gateway *service* is at most one TCP mapping plus any number of UDP
mappings; a *link* carries many services. The Homerun API provisions **one**
service per server today — the API's own limit, not the gateway's — so a game
needing two is a platform gap that `doctor` reports as a warning, not an
engine error.

## Lifecycle — `control.rs`

**Readiness is a substring, deliberately.** Not a regex. Four console defects
surfaced in a single PowerNukkitX bring-up — an ANSI stripper eating `[main]`,
a bare timestamp, a thread tag, an operator list whose case made `/deop` a
no-op forever — and every one was a parser being cleverer than the stream
deserved. A substring is what a person can check against a real log by eye,
and `game verify` re-checks it against the real binary, which is the only
check that has ever caught this class of bug.

A descriptor with no marker never reports ready. That fails visibly at the
start timeout rather than reporting a server that is not up as up.

The **console kind** is a decision; the transport is not. This module says
"speak WebSocket RCON on the port named `rcon` with the secret named `rcon`";
opening the socket is the supervisor's job.

Every **stop ladder** ends in a kill, because a stop that can be refused is
not a stop, and the rung before it exists so a normal shutdown never reaches
the kill. A console stop with no verb, or no console to send it to, starts at
the rung below rather than inventing a polite rung that would only time out.

## Extensions — `extensions/`

What only one game needs — a vendor's sign-in API, a rotating token — lives in
that game's extension, compiled into the runner and chosen by name from the
descriptor (`"extension": { "name", "config" }`). Its pure half is here: an
`ExtensionSpec` per extension (config validation, the values it supplies
through `{extension:<key>}`, the hosts it may reach), the registry, and
`url_allowed`, the one check every URL a person is shown goes through.
`validate` refuses an extension this build lacks, an `{extension:<key>}` the
extension does not supply, a secret one anywhere but `launch.env`, and any in
`client.joinUrl`.

**`docs/game-extensions.md` is the page for all of it** — both halves, the
primitives, the protocol, testing, and how to write one. It is not repeated
here so the two cannot drift.

## The JSON Schema — `schema.rs`

Written by hand, generated by `npm run schema:descriptor`, and **committed**
at `rust/homerun-core/schema/game.v0.json`. The monorepo pins a copy at
`games/schema/game.v0.json`, so that pin is a plain file diff needing no Rust
toolchain.

Committing a generated file usually earns its keep only if something cannot
generate it, and here two things cannot: the monorepo's CI has no cargo, and a
reviewer wants to read a schema change as a diff rather than infer it from a
change to a `json!` macro. `the_committed_schema_is_not_stale` fails when the
file and the types disagree, and names the command that regenerates it:

```bash
npm run schema:descriptor -- rust/homerun-core/schema/game.v0.json
```

Hand-written because `homerun-core` has three dependencies and a standing
argument for each; `schemars` would be a fourth, pulled in to generate a
document that changes a few times a year. The risk is drift, and
`the_schema_names_every_field_the_types_serialise` is the alarm: it serialises
a descriptor with every field populated and asserts the schema describes each
one, naming any that are missing.

The schema describes shape, not sense. Everything in `validate` — a default
outside its own bounds, a port an argument names but the descriptor does not
declare, a secret in the join URL — is beyond what JSON Schema can say. A
descriptor that validates against the document can still be refused.

## The effects half — `homerun-supervisor`, behind `game-engine`

Everything above is pure. The things a descriptor-driven game needs that a
pure crate cannot do — fetching a runtime, unpacking it, driving steamcmd,
speaking RCON, asking the operating system which ports a process bound — live
in the supervisor crate behind a **default-off** feature called
`game-engine`.

Off by default for the same reason every other heavy thing here is: this
crate builds and tests host-native on any machine in seconds, and that is
what makes it worth having. `game-engine` is the second heaviest feature
after `device-ws` — an HTTP client, a zip reader and a websocket.
`npm run test:rust` turns it on, because its tests are the only coverage the
fetcher and the console transports have and they need no device and no game.

It implies `process-engine`: a descriptor-driven game is a child process, so
there is nothing here without one.

| Feature | Tests | Time |
|---|---|---|
| `process-engine` | 197 | ~4s |
| `game-engine` | 230 | ~6s |

### Fetching — `fetcher.rs`

**We are not a mirror.** Every byte comes from the vendor. Homerun downloads
a game's server onto the player's own machine after that player has accepted
the game's terms; it does not host, repackage or redistribute one. `steamcmd`
is fetched from Valve at first use for the same reason — shipping a copy
inside our installer would be redistributing Valve's client.

**Nothing here agrees to anything on anyone's behalf.** `steamcmd` is driven
with `+login anonymous` and nothing else, its stdin is `/dev/null` so a
prompt gets end-of-file rather than an answer, and its output is scanned line
by line for the shapes an agreement prompt takes. Finding one kills the
process and ends the fetch with a message telling the person to run steamcmd
themselves and read what it asks. `prompt_detected` is deliberately broad: a
false positive costs one puzzled look, a false negative means a program
agreed to a licence for someone.

A direct download is streamed to a `.part` file, **resumed** with a `Range`
header when one is already there, and renamed only after its sha256 matches.
Three details are load-bearing:

- A file that **fails** its digest is deleted, not kept. Keeping it invites
  the next run to resume *into* it and fail for ever — the desktop's Pumpkin
  runtime learned this as "delete-on-corrupt".
- A server that **ignored** the `Range` header answers `200` rather than
  `206`; appending to the part would then corrupt it in a way the digest
  catches only after the whole transfer. The status is checked.
- The digest is computed **after** the download, in one pass over the file,
  because a resumed transfer has no running hash to continue from.

A runtime directory records what it holds in a `.homerun-build` stamp. A
directory with files and no stamp is *not* a runtime — that is what an
interrupted download leaves behind, and treating it as finished is how a
server starts against half an install.

**Updating and verifying are different questions, and steamcmd charges very
differently for them.** `app_update` asks Steam what changed and fetches
that; `validate` rereads and checksums every file. Running both on every
start made the second the whole cost: the Rust pilot measured a full
re-verify of 5,869,171,402 bytes, minutes per start, on a runtime that was
already complete and already current. So an update check still runs every
time an unpinned runtime is launched — that is what a force-updating game
requires — and a verify runs only when there is a reason to doubt what is on
disk: no stamp (a first install, or one interrupted before it could stamp),
or a host that sets `Present::suspect`. `engine::fetch` makes that decision
and `fetcher::steamcmd_args` carries it. The risk accepted is a runtime that
is quietly corrupt in a way Steam believes is current, which costs one slow
repair rather than every start.

**Unpacking is careful even though the archive is pinned.** The sha256 pins
the *bytes*, which says nothing about the *paths inside them*: a vendor
archive nobody has audited entry-by-entry can still contain
`../../windows/system32/…`, and a pinned digest would be a pinned digest of a
malicious layout. So extraction refuses absolute paths, drive letters and
anything that climbs out — the same rule `scripts/ui-bundle.js` applies to a
UI bundle. What is deliberately *not* imposed is an entry-count or size
ceiling: a game runtime genuinely is tens of thousands of files and many
gigabytes, and a ceiling tuned for a UI bundle would refuse every real game.

`HOMERUN_STEAMCMD` names an existing `steamcmd` — which is how a Linux or
macOS session uses one, since only Windows can bootstrap it.

**A vendor download is checked and remembered, because it cannot be
pinned.** `runtime.source: "vendor"` fetches from the vendor's own HTTPS site,
named in the signed descriptor, in the version the player chose. The host
resolves that choice (including "latest") to one concrete version and passes
it in; the engine never lists or resolves versions. `url` is a pattern with
`{version}` or `{versionDigits}` (the version without its dots) in its path
and no other placeholder; `extract` must be `zip`; `stripComponents` drops the
archive's top folder(s) (it works for any zip extract); `versionSetting` names
the `string` setting that holds the choice. `sha256` is not used. The version
must be dotted digits, `^[0-9]+(\.[0-9]+){0,5}$`, because it becomes part of a
URL and a directory name. What stands in for a pinned digest:

- the address is the descriptor's, over HTTPS, and a redirect to any other
  origin is refused rather than followed (debug builds also accept plain
  HTTP to `127.0.0.1`, for the test suites; nothing a player runs does);
- the body must be as long as `Content-Length` said, and as `size` when the
  descriptor gives one; a vendor download is never resumed;
- every archive member is read to its end so its CRC is checked, including
  members that stripping leaves with no name, and traversal is refused;
- **trust on first use, per machine**: `<runtimeRoot>/<id>/.vendor-hashes.json`
  maps each version to the sha256 of its first download, written only after
  that download unpacked cleanly. A later download of a recorded version must
  match it; if it does not, the vendor's file for that version changed and
  the fetch is refused before anything is unpacked or run.

Each version lives in `<runtimeRoot>/<id>/<version>`, stamped `v<version>`, so
a version already on disk is `alreadyPresent` and switching back is free.
`engine::fetch::install_dir` is the one place that directory is computed.

### The console — `rcon.rs`

Two dialects: **Source RCON** (Valve's binary protocol over TCP) and
**WebSocket RCON** (Facepunch's JSON dialect, which is what Rust uses with
`+rcon.web 1`).

Both are **synchronous**, using `tungstenite` rather than the
`tokio-tungstenite` already here behind `device-ws`. A console command is one
request and one reply on loopback; routing it through an async runtime would
mean this crate owned a runtime in a build that otherwise needs none.

**One connection per command.** There is no session to lose, no reconnect
loop, no half-authenticated state, and no background thread whose failure is
invisible. A console that silently stopped working is a far worse failure
than one that is a few milliseconds slower.

The cost is named rather than hidden: a WebSocket RCON console **does not
stream the server's own output**, because a client that connects per command
sees only what arrives while it is waiting. The server's stdout is captured
separately by the process engine, so the console *log* loses nothing; what is
lost is chat and command output originating elsewhere. If that matters, a
held connection belongs in this module, not in its callers.

Two details of Source RCON that a careless implementation gets wrong and a
short test would never catch:

- A long reply arrives as **several** packets with no length prefix and no
  terminator. The only portable way to know it has ended is to send a second,
  empty command and read until *its* reply comes back. A reader that stopped
  at the first packet would look entirely plausible and truncate every long
  reply — the test server deliberately splits its answer in two.
- A wrong password is an auth response with an id of `-1`, and servers
  commonly send an empty `RESPONSE_VALUE` *before* it. Anything that is not
  an auth response is ignored.

No TLS, and none wanted: `wss://` would mean the console had left the
machine, which is what the device websocket is for.

### The platform adapter — `platform.rs`

Windows is the only host for descriptor-driven games, and not just for now:
iOS cannot spawn a process and Android can only exec files shipped inside the
APK, so a *downloaded* server binary is unrunnable on both.

That would be an argument for writing Windows code inline. The reason not to
is the suite — a `#[cfg(windows)]` sprinkled through the fetcher and the
engine would mean half this crate could only be *read* on Linux. So the OS
assumptions live in one module behind functions with one meaning each.

| Function | Windows | Linux | macOS |
|---|---|---|---|
| `listening_ports(pid)` | `netstat -ano` | `/proc/net/*` joined to `/proc/<pid>/fd` by inode | `lsof` |
| `process_stats(pid)` | `Get-Process` | `/proc/<pid>/{status,stat}` | `ps` |
| `graceful_interrupt(pid)` | **refuses** | `SIGINT` | `SIGINT` |
| `user_data_roots()` | `%APPDATA%` and friends | `~/.config`, `~/.local/share` | + `~/Library/Application Support` |
| `executable(dir, name)` | adds `.exe` | as given | as given |

`graceful_interrupt` failing on Windows is a **platform fact, not a gap**. A
console control event can only be sent to a process group attached to a
console, and a server spawned with piped stdio has neither;
`GenerateConsoleCtrlEvent` would signal *this* process's group, which
includes the app. That is why `engine::validate` warns about
`stop.via: interrupt` rather than accepting it quietly, and why the stop
ladder treats a failed interrupt rung as something to log and climb past.

Two parsing traps are worth knowing, because both produce a plausible wrong
answer:

- `netstat` columns **differ per protocol** — a TCP row has a state column
  and a UDP row does not — so reading the pid as "the fifth field" works for
  TCP and silently reads `*:*` for UDP. The pid is taken as the *last* field.
- Only `LISTENING` TCP rows count. An `ESTABLISHED` row is a connection the
  server *made*, and forwarding one would publish an outbound socket.

The module never guesses. A port list that omits a port the server bound is
recoverable — the caller polls again — while a port list containing one it
did not bind produces a tunnel that connects, loads cleanly and carries
nothing.

### Descriptor-driven supervision — `process_engine.rs`

Until descriptor-driven games existed, everything this engine knew about a
server was Minecraft's: `console::is_ready` decided when it was up, the
roster came from Minecraft's join and leave lines, and the stop verb was the
literal `stop` on stdin.

Those three answers moved into a `Supervision` the host supplies.
`ProcessEngine::new` still means "a Minecraft server", so nothing that
already used this engine changed; `ProcessEngine::supervised` is the
descriptor-driven door.

**A console line is decoded lossily, and that is not a nicety.**
`BufRead::lines()` yields `Err(InvalidData)` for a line that is not UTF-8,
and `map_while(Result::ok)` — which is what the pump used — reads that as the
end of the stream. One bad byte therefore ended log capture for the rest of
the server's life and dropped the pipe. A player whose name is in a legacy
code page stops `server-log`; before the ready marker it means the server
never reports ready at all and is killed at the start timeout. So the pump
reads to the newline, decodes with `from_utf8_lossy`, and carries on. A line
longer than 16 KiB is cut with a ` [truncated]` marker and the rest of it
discarded, which is what stops a pipe that turns out not to be carrying lines
from being a memory problem. Everything else matches `lines()` exactly — one
trailing newline removed, then one carriage return — because this is the path
Minecraft's console takes on Android and its parsers were written against
that. A test asserts the equivalence against `lines()` itself.

The fetcher's steamcmd pump uses the same reader, where the consequence was
sharper still: steamcmd prints the paths it installs into, so a Windows
username that is not ASCII meant `Success! App` was never seen and the fetch
failed every time on that machine.

| | Minecraft | From a descriptor |
|---|---|---|
| Ready | `console::is_ready` | a substring the descriptor names |
| Roster | join/leave lines, with names | join/leave substrings, **counts only** |
| Console | a line on stdin | stdin, or RCON on a loopback port |
| Stop | `stop`, then terminate, then kill | the descriptor's verb, then the same |

The roster difference is deliberate. A descriptor's presence markers say
*that* somebody joined, not who; parsing a name out of a line whose shape we
have seen in exactly one vendor's log would be a guess, and a wrong name is
worse than no name because it reaches the API as a player.

The **stop ladder** is the part worth watching. Both callers produce one —
`minecraft::jvm::stop_ladder` and `engine::control::stop_ladder` — and this
file has its own `Rung` that both convert into, rather than one of the two
winning. That is not indirection for its own sake: the core is not allowed to
depend on its own Minecraft module, so there is no shared type up there to
use, and a supervisor walking two different ladder types would be two stop
paths pretending to be one.

## The bridge — `engine.*`

A namespace of its own in `homerun-supervisor/src/core_dispatch.rs`, for the
reason at the top of this page. Each arm is a thin call into the module above.

| Method | Answers |
|---|---|
| `engine.validate` | is this a descriptor that describes a game? |
| `engine.schema` | the JSON Schema, from the types |
| `engine.secrets` | which secrets the host must generate |
| `engine.licence` | may we, and what does the game want written? |
| `engine.doctor` | can this machine, and what will be wrong? (optional `runtimeVersion` for a vendor runtime) |
| `engine.fetchPlan` | where the server comes from (optional `runtimeVersion`; required for a vendor runtime) |
| `engine.settings` | the API's strings, typed |
| `engine.invocation` | the command line |
| `engine.classify` | ready / joined / left, for one console line |
| `engine.console` | stdin, or which RCON where |
| `engine.stopLadder` | the rungs |
| `engine.readyTimeoutMs` | how long to wait |
| `engine.forwards` | the wireproxy forwards |

`descriptor` is accepted as the object or as the file's text, because both
callers exist: the desktop has parsed its bundled copy already, the CLI has
just read one off disk.

`engine.classify` is the busiest arm by a distance — one call per console line
during world generation — which is the other reason readiness is a substring
test.

## File map

| File | Holds |
|---|---|
| `mod.rs` | the module's argument, and one end-to-end test across every seam |
| `descriptor.rs` | the serde types — these *are* schema v0 |
| `validate.rs` | every fault in a descriptor, collected, in sentences |
| `settings.rs` | the API's strings coerced and bounds-checked |
| `template.rs` | placeholders, single-pass; the security property |
| `extensions/` | the pure half of game extensions; see `docs/game-extensions.md` |
| `invocation.rs` | argv, env and cwd, with unset settings dropped |
| `fetch.rs` | Direct / SteamCmd / AlreadyPresent |
| `control.rs` | readiness, presence, console kind, stop ladder |
| `ports.rs` | forwards and gateway services |
| `licence.rs` | the gate in front of every download and launch |
| `doctor.rs` | the verdict for one machine |
| `schema.rs` | the exported JSON Schema, and its two alarms |
| `../../schema/game.v0.json` | the generated schema, committed for the monorepo to pin |

And in `homerun-supervisor`, behind `game-engine`:

| File | Holds |
|---|---|
| `fetcher.rs` | download, resume, verify, unpack; steamcmd, anonymous only |
| `rcon.rs` | Valve's binary RCON and Facepunch's WebSocket dialect |
| `platform.rs` | every OS assumption in the crate, in one place |
| `process_engine.rs` | `Supervision` — readiness, roster, console and stop, per game |
| `vendor_http.rs` | HTTPS to a game extension's allowed hosts only (`docs/game-extensions.md`) |
| `local_secret.rs` | DPAPI sealing for what a game extension keeps (`docs/game-extensions.md`) |
| `testdata/rust.json` | the pilot's descriptor, used as a fixture throughout |

## Triage

**A server starts and no player can join it.** Check the forwards. The
gateway-facing port is the *descriptor's* `port`; only the target follows what
the server bound. A forward that "corrects" the listen port to match the bound
one produces a config that loads cleanly, connects cleanly, and is
unreachable.

**A server starts with a setting the player set, ignored.** The setting
probably reached the engine as `""`. That is *unset* by design, and an unset
setting drops its flag. If the player really did set it, the API stored it
empty.

**A launch line is missing an argument entirely.** Look for a `{setting:…}`
whose value is null. The flag *immediately* before it in the descriptor goes
too — that is the rule, not a bug. `engine.invocation` in a test with the same
settings will show it. If the missing argument is not next to a null
placeholder, that is a bug rather than the rule, and `validate`'s warnings are
where to look first.

**`engine.validate` refuses a descriptor that looks fine.** Read the whole
list rather than the first line; `validate` collects, and the first problem is
often a consequence of the last. A `hosts` entry with no matching `platforms`
key, or the reverse, is the usual one — it means the file was half-edited.

**A descriptor from a newer Homerun.** Unknown keys are ignored on purpose.
The exceptions are a `schema` number above this build's, and an unknown
`runtime.source`; both are refused in words that say updating Homerun should
fix it.

**"This game needs a part of Homerun called … that this version does not
have."** The descriptor names an extension this build was not compiled with.
A newer runner is the fix. A test-only name (`fixture`) is refused in any
build that is not a test build, which is the point.

**A download keeps failing at the same point.** Check for a `.download.part`
that is larger than the file should be, or a server that answers `200` to a
`Range` request. Both are handled, and both are what a half-written resume
looks like when it is not.

**steamcmd stops with "Steam is asking someone to agree".** That is the rule
working, not a bug. Run steamcmd yourself once, read what it asks, and answer
it — nothing in Homerun will answer it for you.

**A descriptor-driven server never reports ready.** Its marker is a
substring, matched literally against each console line. Put the real line in
front of it: `engine.classify` with that exact line answers in one call, and
`game verify` is what re-checks the marker against the real binary.

**A game that will not stop cleanly on Windows.** If its descriptor says
`stop.via: interrupt`, that is unsupported on the only platform that ships —
`platform::graceful_interrupt` refuses, the ladder climbs past it, and the
save is whatever the game managed before the terminate. `engine::validate`
warns about it at authoring time.

**The console works for Minecraft and not for a new game.** The route is in
`Supervision::console`, not in the engine. A game whose descriptor says
`console.via: rcon` needs a `ConsoleRoute::Rcon` built with the port the
server was *told* to bind.

**The schema and the types disagree.** Two different alarms.
`the_committed_schema_is_not_stale` means the checked-in file is behind the
types — regenerate it with the command above. `the_schema_names_every_field_the_types_serialise`
means a field was added to `descriptor.rs` and not to `schema()`; it names the
field.

# The game engine — running a server from its descriptor

## Overview

`homerun-core::engine` is one implementation of "fetch, prepare, launch,
observe, stop a game server", driven entirely by a `game.json`. Adding a game
is a descriptor and a docs page; it is not code. A game the schema cannot
express grows the schema, once, and every later game gets the growth.

Everything in this module is pure, like the rest of `homerun-core`: no
sockets, no processes, no filesystem, no clock. The effects live in
`homerun-supervisor`, and the two are joined by the `engine.*` bridge
namespace. That is what lets the whole engine be tested with no game
installed — the suite below runs in milliseconds on any machine.

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

## The three rules that are about safety

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

An agent driving the runner stops and asks a human. If a vendor's own
installer prompts for terms, that is a stop, not a prompt to answer.

### steamcmd is anonymous only

A game whose dedicated server needs a Steam account that *owns* it is out of
scope — not a feature request. There is no account we could use that would not
be either a shared credential or the player's own.

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
placeholder and the token before it starts with `+` or `-` and carries no
placeholder of its own. The check is against the descriptor's source token,
not the emitted argument — otherwise a negative value would be mistaken for
the flag to remove.

An environment variable has no equivalent: a variable set to the empty string
is a different thing from one that is not set, so an unset setting leaves its
variable absent.

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

## The JSON Schema — `schema.rs`

Written by hand, and exported by `npm run schema:descriptor`. The monorepo
pins the output at `games/schema/game.v0.json` and checks the two agree.

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

## The bridge — `engine.*`

A namespace of its own in `homerun-supervisor/src/core_dispatch.rs`, for the
reason at the top of this page. Each arm is a thin call into the module above.

| Method | Answers |
|---|---|
| `engine.validate` | is this a descriptor that describes a game? |
| `engine.schema` | the JSON Schema, from the types |
| `engine.secrets` | which secrets the host must generate |
| `engine.licence` | may we, and what does the game want written? |
| `engine.doctor` | can this machine, and what will be wrong? |
| `engine.fetchPlan` | where the server comes from |
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
| `invocation.rs` | argv, env and cwd, with unset settings dropped |
| `fetch.rs` | Direct / SteamCmd / AlreadyPresent |
| `control.rs` | readiness, presence, console kind, stop ladder |
| `ports.rs` | forwards and gateway services |
| `licence.rs` | the gate in front of every download and launch |
| `doctor.rs` | the verdict for one machine |
| `schema.rs` | the exported JSON Schema, and its drift alarm |
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
whose value is null. The flag before it goes too — that is the rule, not a
bug. `engine.invocation` in a test with the same settings will show it.

**`engine.validate` refuses a descriptor that looks fine.** Read the whole
list rather than the first line; `validate` collects, and the first problem is
often a consequence of the last. A `hosts` entry with no matching `platforms`
key, or the reverse, is the usual one — it means the file was half-edited.

**A descriptor from a newer Homerun.** Unknown keys are ignored on purpose.
The exceptions are a `schema` number above this build's, and an unknown
`runtime.source`; both are refused in words that say updating Homerun should
fix it.

**The schema and the types disagree.** `npm run schema:descriptor` regenerates
the document; `cargo test -p homerun-core engine::schema` names any field that
exists in the types and not in the schema.

# Game extensions — code only one game needs

## Overview

A descriptor (`game.json`) is data, and most games need nothing else. Some
need a little code that is theirs alone: a vendor's own sign-in API, a token
that rotates, a console flow only that game prints. A **game extension** is
where that code lives, so it does not leak into the engine as a descriptor
field with one user, into the protocol as a per-game event, or into Homerun
Desktop as a per-game screen.

A descriptor turns one on by name and gives it data:

```json
"extension": { "name": "hytale", "config": { "…": "data only that extension reads" } }
```

It can never supply code. An extension is compiled into the signed runner,
and the descriptor only chooses among the ones that are — the same line
`console.rcon.protocol` already draws. Nothing is loaded at run time, because
an extension sits next to account tokens.

Each extension has two halves, the same split as the rest of the engine:

| Half | Lives in | Holds |
|---|---|---|
| Pure | `rust/homerun-core/src/engine/extensions/<name>.rs` | its `ExtensionSpec`: config validation, the values it supplies, the hosts it may reach, and any decision two hosts could answer differently |
| Effects | `rust/homerun-game-cli/src/extensions/<name>.rs` | a `GameExtension`: the hooks, which may wait on a person, talk to the vendor, and send console commands |

Both halves stand on primitives the runner and supervisor provide, and reach
the world only through them: a sealed per-machine store, a per-server store,
HTTPS limited to the spec's hosts, generic sign-in and prompt events, and the
server's console. That is what keeps an extension small enough to review and
unable to get the safety rules wrong.

**No release extension exists yet.** `PUBLISHED` is empty; the only extension
is the test-only `fixture`. Hytale's server sign-in is the first planned one.

### When something belongs in an extension

The schema grows when a **second** game needs the same thing. Until then the
need lives in the first game's extension; when the second game arrives it
moves into a primitive or a descriptor field, and the first extension
switches over in the same change. `docs/game-engine.md` states the same rule
from the engine's side.

| A game needs… | Goes in |
|---|---|
| something expressible as data that another game already needs | a descriptor field and a primitive |
| something expressible as data that only it needs | its extension, until a second game needs it |
| something that is not data (an HTTP API, a rotating token) | its extension |
| a new config file format | a config-writer primitive, never an extension |

## The pure half — `homerun-core/src/engine/extensions/`

### `mod.rs`

`ExtensionSpec` is plain data and function pointers, so the core keeps its
dependencies (`serde`, `serde_json`, `ed25519-dalek`) and an extension's pure
half may not add one:

| Field | Answers |
|---|---|
| `name` | what `extension.name` says; `[a-z0-9-]+` |
| `supplies` | every key `{extension:<key>}` may name, and whether each is secret |
| `validate` | problems with this descriptor's `extension.config` (always given an object; `null` means `{}`) |
| `hosts` | where the extension may reach over HTTPS, and send a person to sign in |
| `config_schema` | its config's JSON Schema, spliced into `game.v0.json` |

**The registry is two lists.** `PUBLISHED` is what a release build carries and
the only thing the schema describes. `registry()` adds the test-only `fixture`
under `cfg(test)` or the `test-extensions` feature.

**`url_allowed(url, hosts)`** is the one check every URL a person is shown,
and every request an extension makes, goes through: `https://` only, no
userinfo, no port, and the host matched on whole labels. For `hytale.com`,
`accounts.hytale.com` passes; `hytale.com.evil.net`, `evilhytale.com` and
`evil.net@accounts.hytale.com` do not. The last one lands on an allowed host
but shows a person another site's name first, which is exactly what a
phishing link looks like.

### What `validate` checks, in `engine/validate.rs`

- The name is registered in this build, and the config passes the spec's own
  `validate`. An unknown name reads "This game needs a part of Homerun called
  … that this version does not have. Updating Homerun should fix it."
- Every `{extension:<key>}` names a key the extension supplies, and the
  descriptor names an extension at all.
- A **secret** key appears only in `launch.env`. Argv is readable by every
  process on the machine, and a config file sits in a folder that is backed
  up.
- Nothing an extension supplies appears in `client.joinUrl`.

### `{extension:<key>}`, in `engine/template.rs`

Deliberately not `{secret:…}`. The host generates every secret
(`engine.secrets` lists them), and an extension's values are not the host's
to generate; keeping the namespaces apart also means a host secret and an
extension value can never collide. Substitution is single-pass like every
other placeholder: a supplied value that looks like `{secret:rcon}` stays
text.

### `fixture.rs`

The pure half of the reference extension: it requires a bare `host`,
supplies `token` (secret) and `profile` (plain), and allows exactly its
`host`.

## The effects half — `homerun-game-cli/src/extensions/mod.rs`

`GameExtension` is one game's extension, alive for the whole runner process.
Its `begin` starts one run and returns a `Run`, which owns that run's state
and is dropped when the run ends. Every hook but `begin` has a default that
does nothing.

| Hook | When | Thread | May block? |
|---|---|---|---|
| `begin` | after fetch, before config is written or the launch composed | lifecycle | yes, watching `ctx.stopping()` |
| `Run::on_line` | every output line, already redacted | output pump | **no**: returns `Action`s |
| `Run::on_state` | `Started` (with `server-started`); `Stopping` (the first stop seen) | sampler | **no**: returns `Action`s |
| `Run::on_stop` | after the process tree has exited, before the terminal event | its own | up to `STOP_BUDGET` (10 s), then abandoned |
| `forget` | `extension-forget`: delete what it keeps on this machine | main | briefly |
| `status` | `extension-status`: signed in or not, and as whom | main | briefly |

An `Action` is `Console(command)`, `SignIn`, `SignedIn`, `Note` or
`Fail(error)`. A worker thread carries them out, so the output pump never
waits on a console or a vendor.

### The rules the runner enforces, so no extension can get them wrong

- **What `begin` supplies fills `{extension:<key>}`**, and every key must be
  one the spec declares or the start fails with `extension_failed`: an
  undeclared key never said whether it is secret. Which values are secret is
  the spec's answer, and those join the host's secrets in the redaction
  applied to `server-log` and the crash tail.
- **A sign-in URL is shown only if `url_allowed` passes** against the spec's
  hosts. From `begin` the call returns an error; from an observer the action
  is dropped and logged.
- **A panic never takes the runner down.** Every call is inside
  `catch_unwind`. In `begin` it fails the start with `extension_failed`; in
  an observer it switches that run's observers and `on_stop` off, and is
  logged once to stderr.
- **Console commands are rate limited:** at most one a second per run, and
  the same command not again within 30 s. A server that says "not signed in"
  on every tick must not start a sign-in on every tick.
- **`Fail` stops the server as a refusal,** the same road as `port_exposed`:
  the extension's error, then `server-crashed`, never `server-stopped`.
- **A failed launch still gets `on_stop`** (as `Crashed`), because `begin` may
  have made something to undo — a vendor session — before the launch could
  not go ahead.

### What an extension is given

| Capability | `begin` | `on_stop` | `forget` / `status` |
|---|---|---|---|
| `config()`, `server_id()` | yes | yes | — |
| `descriptor()` | yes | — | — |
| `stopping()`, `wait(d)` | yes | `time_left()` instead | — |
| `note(text)` | as `fetch-progress` (phase `extension`) | as a `host` log line | — |
| `sign_in(..)`, `signed_in(..)` | yes | — | — |
| `prompt(..)` | yes | — | — |
| `http(request)` | yes, ends on a stop | yes, ends with the budget | — |
| `machine_store()` | yes | yes | yes |
| `server_store()` | yes | yes | — |

**Withheld on purpose:** processes, files and sockets outside the list above;
the host's secrets (an extension never sees `rcon` or any other generated
secret); other extensions' stores; the server and runtime folders. An
extension that could spawn or write anywhere would be a second engine, with
none of the process-tree ownership, path confinement or network audit the
runner does now.

### `store.rs`

| Store | Where | Holds | Protection |
|---|---|---|---|
| `MachineStore` | `<tools dir>/extensions/<name>/store.bin` | one JSON document per extension per machine: an account's refresh token | sealed to this Windows user (`local_secret`), exclusive lock, atomic save |
| `ServerStore` | `<server dir>/.homerun/extensions/<name>.json` | one small document per server: a chosen profile | plain JSON, atomic save; **never a secret** |

The tools directory is the runtime root's parent (`prepare::tools_dir`), where
steamcmd and a vendor's downloader already live — outside every server
folder, so a machine store is never in a backup.

**Why the lock and the rename.** A refresh token that rotates is invalidated
the moment its replacement is issued. Two servers starting together that both
refresh the stored token sign the person out; a crash between "vendor issued
a new token" and "new token on disk" does the same. `update` therefore holds
an exclusive lock (`File::try_lock`, waiting up to 10 s) from reading the old
document to saving the new one, and saves by writing a new file and renaming
it over the old — the old document or the new one, never neither, never half.

**An unreadable document reads as `None`** — copied from another computer or
account, damaged, or sealed for another extension. An extension treats that
as "not signed in" and asks again, rather than failing every start.

### `prompt.rs`

A question with a closed set of answers, asked only from `begin`. The runner
sends `prompt`; the host shows its one choice dialog and answers with
`prompt-answer` on the main thread; `Prompts` is where the two threads meet.
One prompt is open at a time, because one server runs at a time. There is no
overall time limit — a person may take as long as they like — and a Stop is
how they decline. Every prompt ends with `prompt-closed`, answered or not.

An answer that is not one of the options is refused (`descriptor_invalid`,
with the server id). An answer for a prompt that is already closed is not an
error: a person who clicks as the server stops has done nothing wrong.

### `fixture.rs`

The reference extension's effects half, built only with the `test-extensions`
feature. Its config picks the behaviour a test wants, and it uses every hook
and every capability, which makes it the example a new extension starts from:

| Key | Does |
|---|---|
| `signIn` / `signInUrl` | send a server sign-in, trusted or not |
| `awaitStop` | wait in `begin` until a stop |
| `panicIn` | `"begin"` or `"line"` |
| `consoleOn` + `command` | send `command` when a line contains `consoleOn` |
| `failOn` | fail the run when a line contains it |
| `supplyUndeclared` | supply a key the spec does not declare |
| `askProfile` | ask once per server which profile, and supply it |
| `remember` | count starts in the sealed machine store |
| `vendorUrl` / `vendorOnStop` | `GET` in `begin`, `DELETE` in `on_stop` |

## The primitives — `homerun-supervisor`, behind `game-engine`

### `vendor_http.rs`

HTTPS to a vendor's own hosts, and nothing else.

- **Only the hosts the spec names.** The URL, and every redirect hop, must
  pass `url_allowed`; a redirect anywhere else ends the request.
- **Bounded:** 30 s per request, a body of at most 1 MiB.
- **Cancellable:** the request races the caller's cancellation, so a Stop is
  never stuck behind a silent peer. From `begin` that is the stop signal; from
  `on_stop`, the end of its budget.
- **Quiet:** method, host and path go to stderr; the query string and bodies
  never do, because that is where tokens and device codes travel.

Failures come back as `NotAllowed` (→ `extension_failed`, "…a site Homerun
does not trust…"), `Cancelled`, or `Unreachable(message)` (→
`vendor_unavailable`, with the message).

`Policy::loopback_http` also allows `http://127.0.0.1:<port>`, for the
lifecycle tests' fake vendor. The runner sets it only when built with
`test-extensions`.

### `local_secret.rs`

Seals bytes to this computer's current user with Windows DPAPI
(`CryptProtectData`, UI forbidden). The extension's name is mixed in as
DPAPI's optional entropy, so one extension's file moved into another's folder
does not open. **Elsewhere it refuses** rather than fall back to a plain
file: the runner is Windows-first.

**Every sealed value starts with one scheme byte** (`Scheme`; `1` is DPAPI).
It is there so the backend can change without making anyone sign in again:
`scheme_of` says how a value was sealed, `current` says how this build seals,
and a value in an older scheme is opened and written back in the current one
on the next save (`MachineStore::update` always re-seals). A scheme this build
does not know is refused in words that say a newer Homerun wrote it, rather
than read as damage.

**Other platforms, when they come.** Keystores (macOS Keychain, Linux Secret
Service, iOS Keychain, Android Keystore) hold small items, while DPAPI holds
nothing and has no size limit, so the expected shape is envelope encryption:
a random key per extension in the keystore, the document encrypted with it on
disk, as a new `Scheme`. `seal`/`open` and every caller stay as they are.
Still to decide then: whether headless Linux, which usually has no Secret
Service, refuses (the default) or offers an explicit owner-only-file mode;
and, for phones, the host supplies the key over the FFI, since their
keystores are reachable from Kotlin and Swift rather than Rust.

## The protocol

Every addition is generic; nothing names a game.

| Kind | Name | Fields |
|---|---|---|
| Event | `sign-in` | `serverId`, `purpose` (`download` \| `server`), `url`, `code?`, `expiresInSecs?` |
| Event | `signed-in` | `serverId`, `purpose` |
| Event | `prompt` | `serverId`, `promptId`, `kind: "choice"`, `title`, `message?`, `options: [{value, label}]` |
| Event | `prompt-closed` | `serverId`, `promptId` |
| Event | `extension-status` | `extension`, `signedIn?`, `account?` |
| Command | `prompt-answer` | `serverId`, `promptId`, `value` |
| Command | `extension-status` | `extension`, `runtimeRoot` |
| Command | `extension-forget` | `extension`, `runtimeRoot` — refused with `busy` while a server is fetching or running |

`runtimeRoot` on the two extension commands is the one `fetch` and `start`
are given: the machine store is found beside the runtimes. Without it the
command is refused (`descriptor_invalid`) rather than guessed at.

Error codes: `sign_in_required`, `sign_in_expired`, `account_not_allowed`,
`vendor_unavailable`, `extension_failed`. Each arrives with a message written
for a player; the code is what a host branches on.

Features: `ready.features` and `homerun-game --features` carry `extensions`
(the mechanism, these events and commands) and one `extension:<name>` per
registered extension, generated from the registry. **A host must require
`extension:<name>` for any descriptor naming that extension.** Unknown
descriptor fields are ignored by design, so a runner built before extensions
existed would silently ignore the block and launch the game without it; the
feature gate is the only thing that prevents that.

## Testing an extension

- **Contract tests** (`extensions::tests`) iterate the registry, so a new
  extension cannot skip them: the two registries agree; `status` answers on a
  fresh machine; after `forget`, not signed in and no store file left.
- **The harness** (`extensions::tests::harness`) drives an extension's
  `begin` and `Run` directly, with a recording `Output`, a real `Prompts` a
  helper thread answers, and stores under a temporary runtime root. Most of an
  extension's tests need no process and no network.
- **Lifecycle tests** (`tests/lifecycle.rs`, module `extensions`) run the real
  runner against the fake game, with `serve_in_turn` as a fake vendor on
  loopback.
- **Break each guard on purpose** (the `tests-that-bite` skill). One lesson
  from doing it here: a guard whose absence makes the code *wait* — a prompt
  that ignores Stop — makes a naive test hang rather than fail. Run the call
  under test on its own thread and fail on a timeout, as
  `prompt::tests::a_stop_abandons_the_prompt_and_closes_it` does.

Run them with `npm run test:game`, which passes `--features test-extensions`.

## Writing a new extension

1. **Pure half:** `homerun-core/src/engine/extensions/<name>.rs` with a
   `SPEC`; add it to `PUBLISHED`. Keep every decision that is not an effect
   here, as a plain function with unit tests.
2. **Effects half:** `homerun-game-cli/src/extensions/<name>.rs` implementing
   `GameExtension`; add it to `registry()`. Copy the shape of `fixture.rs`.
   Call nothing but the context.
3. **Regenerate the schema:** `npm run schema:descriptor --
   rust/homerun-core/schema/game.v0.json`.
4. **Tests:** harness tests for the flows, a loopback fake of the vendor, and
   break each guard you added.
5. **Hosts:** the desktop requires `extension:<name>` for descriptors naming
   it, and shows the generic sign-in card and choice dialog — nothing
   game-specific.
6. **Docs:** a section here for anything the extension decides that a reader
   could not infer, and the game's own docs for why it needs one at all.

## File map

| File | Holds |
|---|---|
| `homerun-core/src/engine/extensions/mod.rs` | `ExtensionSpec`, `PUBLISHED`, `registry()`, `url_allowed` |
| `homerun-core/src/engine/extensions/fixture.rs` | the reference extension's pure half (test only) |
| `homerun-core/src/engine/validate.rs` | `check_extension`: the rules above |
| `homerun-core/src/engine/template.rs` | `{extension:<key>}` |
| `homerun-core/src/engine/schema.rs` | `extension`, generated from `PUBLISHED` |
| `homerun-game-cli/src/extensions/mod.rs` | the hooks, contexts, action worker, registry, commands |
| `homerun-game-cli/src/extensions/store.rs` | `MachineStore`, `ServerStore` |
| `homerun-game-cli/src/extensions/prompt.rs` | `Prompts`: the open question between two threads |
| `homerun-game-cli/src/extensions/fixture.rs` | the reference extension's effects half (test only) |
| `homerun-game-cli/src/runner.rs` | where the hooks are called in a run |
| `homerun-supervisor/src/vendor_http.rs` | HTTPS to allowed hosts only |
| `homerun-supervisor/src/local_secret.rs` | DPAPI sealing |

## Triage

**"This game needs a part of Homerun called … that this version does not
have."** The descriptor names an extension this build was not compiled with.
A newer runner is the fix. `fixture` is refused by every build that is not a
test build, which is the point.

**A start fails with `extension_failed` and "does not trust".** The extension
asked to show, or reach, a URL on a host its spec does not allow. Check the
descriptor's `extension.config` against the spec's `hosts`; the stderr log has
`extension http: … -> refused` for requests.

**A start fails with `extension_failed` and nothing else useful.** Look at the
runner's stderr: a panic in `begin`, or a supplied key the spec does not
declare, is logged there by name, and deliberately not shown to the player.

**Every start asks the person to sign in again.** The machine store is not
being kept or not being read. Check `<tools dir>/extensions/<name>/store.bin`
exists; if it does and stderr says it "cannot be read", it was sealed on
another computer or by another Windows account, or for another extension.

**Two servers starting together, and one fails with "Another server is using
this game's sign-in right now".** The store's lock was held for more than
10 s — an extension doing slow work (a vendor request) inside `update`. Keep
network calls outside `update` where the flow allows, and inside it only when
the rotation demands it.

**A `prompt` never gets an answer.** The host has to reply with
`prompt-answer` carrying the same `serverId` and `promptId`, and a `value`
from the options. A wrong value is an `error`; a stale `promptId` is silently
ignored.

**`extension-status` or `extension-forget` answers `descriptor_invalid`.**
They need `runtimeRoot`, the same one `fetch` and `start` get.

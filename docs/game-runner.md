# The desktop game runner

## Overview

`homerun-game` runs descriptor games for Homerun Desktop without linking
Pumpkin. `homerun-core` decides; `homerun-supervisor` fetches and owns the
process; this binary connects them to the person or Electron controlling it.

Build locally with `cargo build --manifest-path rust/homerun-game-cli/Cargo.toml`.
Run `npm run test:game` for the protocol and real-process fixture tests. No test
downloads a vendor's game or accepts a vendor's terms.

## `runner.rs`: supervise

`homerun-game supervise` takes UTF-8 NDJSON commands on stdin and emits only
NDJSON on stdout. Diagnostics go to stderr. Protocol version 1 matches the
monorepo's `plans/multi-game-contracts.md`; `ready.build` is the first twelve
hex characters of this executable's SHA-256, including any signature.

`ready` also carries `features`: the names in `protocol::FEATURES`, which say
what this build understands that the protocol version does not. The protocol
version is bumped only on a break, so it cannot describe a descriptor field
added compatibly -- `launch.cwdBase` and `saves.mounts` arrived without one, and
a runner published before them agrees to run such a descriptor and then puts the
world where the host does not know to look. A host compares the two lists and
refuses the launch instead. `homerun-game --features` prints the same list as a
JSON array, which is what `scripts/publish-desktop-artifacts.js` asks the built
binary so a manifest cannot claim a capability the artifact beside it lacks.
Adding a name is how a descriptor field becomes something a host may rely on;
removing one is a break.

### A vendor's own downloader (`runtime-vendor-tool`)

`runtime.source: "tool"` is for a game whose server files only its owner's
account can fetch -- Hytale is the first. The descriptor pins the vendor's
downloader (`runtime.tool`: url, sha256, extract, exe); the runner fetches and
verifies it into `<tools dir>/tools/<game>-<sha12>/` like any direct download,
then runs it with `runtime.args`. Only `{output}` (where to write the archive)
and `{credentials}` (the downloader's own sign-in file) are substituted, and
`validate` refuses any other placeholder, so nothing a player chose reaches its
command line.

The first time, the downloader asks the person to sign in. Lines matching
`runtime.signIn` become the generic **`sign-in`** event with `purpose:
"download"` (`url`, and `code` when the downloader prints one separately) --
the same event a game's extension sends, so a host has one sign-in card. The
person approves in their own browser and the download continues; once the
downloader finishes signed in, **`signed-in`** closes the card. Nothing is typed into the
downloader, and an agreement prompt ends the run exactly as it does for
steamcmd. The sign-in file lives at `<tools dir>/credentials/<game>.json` --
beside steamcmd, never in a server folder, so never in a backup -- and the
runner never reads it.

What the downloader fetches cannot be pinned: the vendor serves only its
current build. So every fetch first runs `runtime.versionArgs`; the last line
of output is the version. If it matches the runtime's stamp nothing is
downloaded; otherwise the archive is downloaded to `{output}`, unpacked into
the runtime directory and stamped with that version. If the check fails and a
build is installed, the installed build is used (an outage should not stop a
server that needs nothing new). Unpacking adds and overwrites; it does not
delete files a newer build dropped.

The downloader's own URL is unversioned for Hytale, so when the vendor
replaces it the pinned digest stops matching and the fetch fails with a
checksum error until the descriptor is re-pinned. That is deliberate: an
executable Homerun runs is one somebody vetted.

The commands are hello, fetch, start, start-tunnel, console, stop, status,
shutdown, extension-status and extension-forget (see *Game extensions*). A new process announces ready, and hello repeats the announcement.
An incompatible hello ends the session. Unknown commands are ignored. Bad
JSON is diagnosed without echoing it (it could contain secrets). A recognized
command with missing or incorrectly typed fields emits `descriptor_invalid`
with a recoverable `serverId`; malformed console requests also complete a
recoverable `reqId`. No request field values or serde diagnostics are echoed.
Commands are
limited to one MiB; an oversized line closes the session rather than retaining
unbounded input. EOF takes the same cleanup path as shutdown.

One fetch/start worker is admitted at a time. It owns the process and stop
signal, while the command loop remains responsive during downloads and world
generation. A stop during a download cancels it. Fetch cancellation races the
HTTP request and body reads, so an unresponsive peer cannot prevent shutdown.
steamcmd's output is drained independently and cancellation does not require
it to print another line. Its stdin is closed; prompts are never answered.

Every fetch/start first requires explicit licence acceptance, then validates
the descriptor. There is no silent fallback to a Minecraft process. `start`
also fetches the runtime if needed. Server settings remain opaque player text
when core composes the invocation. Nonempty secrets are redacted from streamed
logs and the last-100-lines crash tail.

The preferred ports are checked together before spawn. Occupied ports fail
with port_unavailable; this version does not choose replacement ports. As
with any bind preflight, another process can race the check. Readiness is not
published until both the marker and the owned process tree's declared listening ports
are observed. Ports are emitted before server-started. A missing marker or
port reaches ready_timeout and follows the game's stop ladder.

Windows inspects native IPv4/IPv6 TCP and UDP tables for the entire owned Job
Object, continuously from spawn through shutdown. Inspection runs independently
of RCON requests. Private ports observed beyond loopback cause immediate tree
termination and `port_exposed`; an inspection failure causes termination and
`port_inspection_failed`. Neither refusal waits for the normal save grace.
See “Continuous network checks” below for probe inventory and limitations.

ProcessEngine drains stdout and stderr independently; readiness on stderr
works even if stdout is quiet. Its original Engine trait callers still receive
merged lines. The runner's streamed entry point preserves pipe identity.

The runner reuses the descriptor's console/stop ladder. It reports sampled
RSS and CPU counters, log-presence counts, and JSON RCON rosters with `name` /
`platformId` / `platform`, or `DisplayName` / `SteamID`. Other roster formats
and A2S player queries are not implemented; they produce no invented roster.
RCON is request/response, not the source of server logs. Console refusals also
complete console-response with the request ID so the desktop does not hang.
Exit codes are omitted when the existing Engine outcome does not retain them.

A tunnel is a child of this runner and stops with the server. tunnel-started
means the wireproxy process spawned, not that the remote gateway is reachable.
It gets the same job object the game does, so a runner that dies abruptly does
not leave a gateway connection behind with nothing serving it.

**Mounted Windows games require a Job Object.** The runner creates a named
job and assigns the game using `PROC_THREAD_ATTRIBUTE_JOB_LIST` during
`CreateProcessW`, before the initial thread can run. Creation or assignment
failure refuses launch. Descendants inherit membership; breakaway is not
allowed. The stop ladder can terminate the whole job even after a launcher
exits. The runner itself stays outside the job.

Normal cleanup terminates the remaining tree and confirms zero active
processes before unlinking saves. Hard runner death closes its job handle and
initiates termination, but recovery does not assume that termination has
finished: it reopens the recorded job, terminates and drains it before
unlinking or updating. Query failure or timeout preserves the links and
journal. Mounted launches on non-Windows platforms refuse until equivalent
ownership and recovery exist.

Unmounted game and tunnel launches retain their existing best-effort job
behavior: post-spawn assignment, a small assignment window, and fallback if
creation or assignment fails. That fallback provides no ownership guarantee
and cannot be used for mounted saves. Detaching from the runner on desktop
quit is unaffected.

See `docs/shared-core.md` and `rust/homerun-supervisor/src/job.rs`.

## Game extensions: `src/extensions/`

The runner runs a descriptor's extension: `begin` after the fetch and before
the launch is composed (it may wait on a person, and ends on a Stop), then
observers on every line and state change, and `on_stop` within 10 s before the
terminal event. It enforces, for every extension, the rules an extension must
not get wrong: sign-in URLs only on the spec's hosts, supplied secrets
redacted, panics contained, console commands rate limited, a vendor reached
only over `vendor_http`, and what is kept sealed to this user.

The commands `extension-status`, `extension-forget` and `prompt-answer`, and
the events `sign-in`, `signed-in`, `prompt`, `prompt-closed` and
`extension-status`, belong to it, as do the codes `sign_in_required`,
`sign_in_expired`, `account_not_allowed`, `vendor_unavailable` and
`extension_failed`.

**`docs/game-extensions.md` is the page for all of it.** It is not repeated
here so the two cannot drift.

## Vendor runtimes: `runtimeVersion` and `--runtime-version`

A descriptor whose `platforms[host].runtime.source` is `vendor` downloads the
game's server from the vendor's own site in a version the player chose. The
host resolves that choice -- including "latest" -- to one concrete version
and sends it as `runtimeVersion` on `fetch` and `start`; standalone, it is
`--runtime-version <v>` on fetch, launch, probe, verify and doctor. The runner
never lists or picks versions. A vendor descriptor with no version is refused
with `descriptor_invalid`, as is a version that is not dotted digits
(`^[0-9]+(\.[0-9]+){0,5}$`): it becomes part of a URL and a directory name.
Other sources ignore the field. Both the source and the field are covered by
the `vendor-runtime` feature name, so a host refuses such a game on a runner
that does not advertise it.

Each version has its own runtime directory, `<runtimeRoot>/<id>/<version>`,
stamped `v<version>`; the fetch, the executable check, a `runtime` working
directory, `{runtimeDir}` and save mounts all use it. The lock and the save
mount journal stay per game, and the journal records which version's
directory its mounts were made in, so recovery unlinks them there.

The download itself is not pinned by digest, so the fetcher holds it to what
can be checked: HTTPS to the descriptor's address, no redirect off that
origin, a body exactly as long as announced (and as `size`), every archive
member's CRC, `stripComponents` for an archive nested under one top folder,
and no traversal. The sha256 of the first download of each version is
recorded per machine in `<runtimeRoot>/<id>/.vendor-hashes.json`, and every
later download of that version must match it. When it does not, the fetch
fails with `fetch_failed` saying the vendor's file for that version changed,
and nothing is unpacked or run. Deleting the record is how a person who has
checked the new file tells this machine to trust it.

## Host-supplied Java: `javaPath` and `--java`

A JVM server's descriptor names the Java it needs, `requires.java.major`, and
`launch.program: "java"` in place of `launch.exe`. It downloads no JRE of its
own: Homerun Desktop already keeps Java runtimes (one bundled, others fetched
once per major and shared with Minecraft), so the host resolves one and sends
its `java` as `javaPath` on `fetch` and `start` (`--java <path>` standalone,
including `doctor`). Feature `host-java`; an older runner would ignore both
fields and have no program to run.

The runner checks what it was given before it downloads or spawns anything
(`prepare::host_java`): the path must be absolute and exist, and the program's
own `java -version` must report exactly the major the descriptor names. A
missing, wrong or unrunnable Java is `requires_unmet` with a sentence a player
can read ("Hytale needs Java 25, and the Java this app provided is 21"), never
a fall-through to some other program. The host chose the path; the check is
there because a store can hold a damaged or half-extracted JRE.

The launch spawns `javaPath` with `launch.args` unchanged, so `{runtimeDir}`
still names the game's own files. `JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS` and
`JDK_JAVA_OPTIONS` are removed from the server's environment unless the descriptor sets
them, so a value left on the player's machine by some other tool cannot
change how it runs. The match is on an exact major, like the desktop's own
`resolveJava`: a vendor that moves to a new Java bumps the descriptor.

The lifecycle tests use `examples/fake_java.rs` as the host's `java` (a real
executable, because the runner spawns into a job object, which a batch file
cannot be); `cargo test` builds it.

## `prepare.rs`: directories, settings and resources

For cwd-relative assets, `launch.cwdBase: "runtime"` selects the shared runtime
while `saves.mounts` redirects save directories into the server folder. See
[runtime working directories and save mounts](./runtime-save-mounts.md) for
locking, recovery before updates, path restrictions and real-game limitations.
Mounted launches require atomic Windows Job Object membership; recovery and
normal cleanup confirm the entire job has exited before unlinking saves. The
legacy best-effort job fallback described above applies only to unmounted games.

The runtime root holds one directory per game. The shared steamcmd cache is
its sibling under the runtime parent. RAM/free disk/CPU capacity comes from
the platform module, and a required resource that cannot be measured causes
a refusal rather than an optimistic launch.

Relative cwd paths are confined to the selected server or runtime base; config
paths stay confined to the server directory. Managed files
cannot traverse symbolic links. JSON object and properties files merge managed
keys without erasing unrelated settings. A managed key whose setting is unset
is **removed** rather than left at the previous launch's value; unmanaged keys,
comments and layout survive that too. Other descriptor config formats
(INI, TOML, XML) currently fail with an explicit capability message; implement
their merge rules before onboarding a game that requires them. The descriptor
and vendor executable remain trusted bundled inputs, not a sandbox for hostile
programs. Port checks do not establish where a vendor writes its saves or which
network interfaces it chooses: the onboarding probe must verify those facts.

## `cli.rs`: standalone use

`--help` lists every flag. A positional argument is a descriptor filename or a
slug resolved as `games/<slug>/game.json`.

```text
homerun-game doctor games/example/game.json --json
homerun-game fetch games/example/game.json --accept-licence --json
homerun-game fetch games/terraria/game.json --accept-licence --runtime-version 1.4.5.8
homerun-game launch games/example/game.json --accept-licence --server-dir servers/example
homerun-game stop --server-dir servers/example
homerun-game probe games/example/game.json --accept-licence --observe-seconds 10
homerun-game verify games/example/game.json --accept-licence --json
```

Only pass --accept-licence after a person has actually accepted the applicable
terms. Settings and host-generated secrets come from separate JSON files via
--settings-file and --secrets-file; they are not stored in command-line strings.

launch stays in the foreground; Ctrl+C requests its normal stop. Standalone
launch/probe/verify exclusively own `.homerun-runner.lock` in their server
directory. stop writes a matching token to `.homerun-stop`, then waits for the
owner to release its lock. It never kills a PID from a stale file. These files
are for the local CLI only: supervise has no control file, named pipe or socket.
The server directory is a trusted, user-owned directory; these tokens protect
against stale requests, not another user who can write that directory.

probe and verify use the real fetch/start/stop path, observe after readiness,
then save `evidence/<host>/probe.json` (or --evidence). verify exits nonzero on
a lifecycle error. The report retains at most 10,000 events and explicitly
names unverified claims. It is evidence of readiness, bound ports, sampled
resources and stop completion, **not** proof of save persistence, stray writes,
all player-query formats, or gateway reachability. Real-game onboarding still
needs those checks. No real Rust server has been tested by this implementation.

With --json, CLI lifecycle events have the same envelopes as supervise.
doctor has no success event in protocol v1: success is its exit status, warnings
go to stderr, and refusals are error events. Human mode prints its full verdict.

## File map

| File | Responsibility |
|---|---|
| `rust/homerun-game-cli/src/protocol.rs` | Wire types and literal JSON compatibility tests |
| `src/runner.rs` | Worker ownership, events, tunnels, stdin and EOF cleanup |
| `src/prepare.rs` | Validation, fetch, invocation and confined configuration writes |
| `src/cli.rs` | Arguments, local ownership, stop requests and probe evidence |
| `src/extensions/` | Game extensions; files listed in `docs/game-extensions.md` |
| `tests/lifecycle.rs` | A self-reinvoking fake game, local HTTP and real subprocess tests |
| `rust/homerun-supervisor/src/process_engine.rs` | Process lifecycle and independent pipe draining |
| `rust/homerun-supervisor/src/fetcher.rs` | Cancellable effects, including a silent download peer |

## Triage

**Stopped before server-started:** inspect error and server-log events. A
runtime stamp alone is not enough; its executable must exist too.

**Ready marker but no server-started:** a declared port has not been observed
in the owned Windows process tree (or the root PID on the Linux development
adapter). Do not publish guessed ports to get past the check.

**"The vendor's file for that version changed":** the bytes served for a
version this machine has already downloaded differ from the first download.
Nothing was run. Find out why before deleting that version's entry from
`<runtimeRoot>/<id>/.vendor-hashes.json`.

**Standalone lock left behind:** confirm the old runner and server have exited,
then remove that folder's `.homerun-runner.lock`. A stale PID is never killed.

**verify passed but joining fails:** verify does not test the gateway or client.
The evidence says exactly which facts were and were not observed.

## Continuous network checks

On Windows descriptor launches require atomic Job Object membership before game code
executes. A dedicated watcher samples native IPv4/IPv6 TCP-listener and UDP-endpoint
tables every 250 ms for all current job members, including descendants. RCON sampling
runs separately. This is detection and response, not pre-bind network isolation:
a short-lived endpoint between polls may be missed and scheduler delays remain possible.

Declared private ports must remain loopback throughout startup, running and shutdown.
A breach emits `port_exposed` and immediately terminates the owned tree, bypassing the
save grace; unsaved progress may be lost. Failure to inspect sockets or job membership
emits `port_inspection_failed` and also terminates the tree. These are refusals, never
successful stops. The Linux development adapter retains server-PID scope and PID termination, reports
inspection failures and labels that narrower scope in evidence. It does not claim
Windows Job Object descendant coverage. Other unsupported inspection adapters refuse.

`probe`/`verify` also refuse undeclared non-loopback endpoints. Undeclared loopback
endpoints are recorded without failing. UDP tables include all bound endpoints; they
do not distinguish an auxiliary outbound socket from a service. Such endpoints need
review rather than being silently omitted. Ordinary `launch`/`supervise` enforce
private ports but do not apply the probe's undeclared-port refusal.

Probe JSON gains `network`: scope, nominal polling interval, timing origin, sample
count and a bounded inventory of PID/protocol/address/port, declared/confined flags,
first/last observation and sample counts. Observations are aggregated, not a packet
trace. Exceeding the distinct-endpoint bound fails verification instead of truncating
an apparently complete audit. No Windows Firewall rules are created or altered.

The watchdog performs no protocol output. It records refusal, terminates the Job,
and lets the lifecycle thread deliver the error afterwards. A blocked stdout consumer
can delay events but cannot leave the violating game alive. On root-process exit the
owned tree is drained before monitoring ends or a terminal event is published.

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

The commands are hello, fetch, start, start-tunnel, console, stop, status and
shutdown. A new process announces ready, and hello repeats the announcement.
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
| `tests/lifecycle.rs` | A self-reinvoking fake game, local HTTP and real subprocess tests |
| `rust/homerun-supervisor/src/process_engine.rs` | Process lifecycle and independent pipe draining |
| `rust/homerun-supervisor/src/fetcher.rs` | Cancellable effects, including a silent download peer |

## Triage

**Stopped before server-started:** inspect error and server-log events. A
runtime stamp alone is not enough; its executable must exist too.

**Ready marker but no server-started:** a declared port has not been observed
in the owned Windows process tree (or the root PID on the Linux development
adapter). Do not publish guessed ports to get past the check.

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

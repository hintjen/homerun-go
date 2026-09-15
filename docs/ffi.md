# `homerun-pumpkin-ffi` — the C surface

The Rust library both mobile hosts link. It owns the server's lifecycle,
console buffer, and crash reporting, and exposes them over a C ABI so Swift
and Kotlin talk to one implementation instead of two.

Source: `rust/homerun-pumpkin-ffi/`.

## Why this crate exists at all

The Pumpkin server was written to be a process. Two of its habits are fatal
inside an app:

- it calls `process::exit(1)` when it cannot bind its port, which on a phone
  terminates the entire app with no report;
- it writes to stdout, which neither platform surfaces, so the console — a
  headline feature — would be empty.

The prototype fixed both by patching Pumpkin. That works but makes the fork
expensive to rebase. Here the fixes live in **our** crate, so the fork can
shrink to library-mode patches only and eventually disappear upstream.

## Layout

| Module | Responsibility |
|---|---|
| `lib.rs` | The `extern "C"` surface. Marshalling, panic containment. |
| `server.rs` | Composes everything into the one server this device hosts. Owns the global. |
| `engine.rs` | The `Engine` trait — the only part that needs Pumpkin. |
| `pumpkin_engine.rs`, `pumpkin_settings.rs` | The linked Pumpkin engine and the assignment of a launch's settings onto its types. Behind `pumpkin-engine`. |
| `process_engine.rs` | The `Engine` that supervises a **child process**. Behind `process-engine`; never iOS, which cannot spawn one. |
| `state.rs` | The state machine and its legal transitions. |
| `log_buffer.rs` | Bounded console buffer with monotonic cursors. |
| `preflight.rs` | Port availability, checked before the engine can exit the process. |
| `crash.rs` | Panic hook, crash reports, last-panic capture. |
| `core_dispatch.rs` | `homerun-core`'s shared decisions, with no platform in it. |
| `core_bridge.rs` | The JNI adapter around `core_dispatch` (Android only). |
| `jni_bridge.rs` | The JNI adapter around the C surface itself (Android only). Calls the same C functions rather than reaching past them. |
| `host_dispatch.rs` | The shared decisions that need one effect — a socket, a file — on the same wire as the pure ones. |
| `device_ws/` | The websocket the dashboard connects to: listener, TLS, ACME, JWKS. Behind `device-ws`, on for both phone targets. See `plans/device-websocket.md`. |
| `app_logs.rs` | This app's own logs, for `get-app-logs`: logcat on Android, a host-registered provider everywhere else. Redaction is not here: the core scrubs the frame as it builds it (`device_ws::protocol::outgoing`), so no driver can send the log raw. Always compiled. |
| `host_log.rs` | Where this crate's diagnostics go when the platform captures neither stdout nor stderr. Android wires logcat itself; iOS registers a sink. Always compiled. |
| `backup_job.rs` | Progress, cancellation and the one-at-a-time guard for a backup. Built everywhere. |
| `backup_engine.rs` | The linked backup engine. iOS only, behind `backup-engine`. |
| `engine_settings.rs` | What the player's settings mean to an engine — clamps, UUIDs, what cannot be honoured. No Pumpkin, so it is in the fast suite. |

Everything except `Engine::run` and `backup_engine` is platform-independent
and unit-tested on any machine — 142 tests under `npm run test:rust`, no device
and no Pumpkin required, plus the `device-ws` module's own when that is on.

`core_dispatch` is deliberately built on every target, not just the two mobile
ones, which is what lets its dispatch tests run under plain `cargo test`.

## Calling convention

Every function returns a heap-allocated JSON C string.

**The caller must free it with `homerun_free_string`.** Leaking these leaks
the server's entire console over a long session.

```c
uint32_t homerun_abi_version(void);
void     homerun_free_string(char *ptr);

/* The shared decisions. Method catalogue in docs/core-bridge.md. */
char *homerun_core_call(const char *method, const char *args);

char *homerun_server_start(const char *request_json);
char *homerun_server_settings_preview(const char *request_json);
char *homerun_server_stop(void);
char *homerun_server_state(void);
char *homerun_server_stats(void);
char *homerun_server_players(void);
char *homerun_server_logs_since(uint64_t cursor);
char *homerun_server_command(const char *command);
char *homerun_server_metrics(void);

/* The host's own console lines, and the launch boundary. */
char *homerun_server_note(const char *line);
char *homerun_server_console_begin(void);

/* The dashboard's console and RCON, served from this device. Behind
   `device-ws`; a build without it answers that it cannot serve one. */
char *homerun_device_ws_start(const char *config);
char *homerun_device_ws_stop(void);

/* Two callbacks a host may register, both added at ABI 8 and both there
   because of iOS: one for where this crate's own diagnostics go, one for
   where this app's own logs come from. NULL unregisters either. Android
   needs neither — logcat answers both. */
char *homerun_set_log_sink(homerun_log_sink_fn sink);
char *homerun_set_app_logs_provider(homerun_app_logs_fn provider);
```

**A host that registers no log sink gets no diagnostics at all from this
crate**, and on iOS that is every diagnostic the device websocket produces.
`println!` is not the fallback: after a launch stdout is the pipe feeding the
player-visible console, so a line written there is shown to a player as if the
server had said it. Android is the other shape of the same trap — it captures
neither stream, so the line goes nowhere. Everything logs through the `log`
facade for that reason.

Fallible calls answer `{"ok":true,…}` or `{"ok":false,"error":"…"}`.

**Error strings are shown to players.** They are written for players, and a
test asserts they contain no `errno`, `unwrap`, `panicked at`, `Mutex`, or
`null pointer`. Keep it that way.

Check `homerun_abi_version()` at startup; it is bumped whenever the surface
changes shape.

### Starting a server

`homerun_server_start` takes one JSON request rather than arguments, so a new
setting does not change a C signature — the same call three repositories agree
on:

```json
{ "serverId": "abc", "dataDir": "/…/servers/abc", "port": 25565,
  "settings": {
    "env": { "MOTD": "…", "GAMEMODE": "creative", "MAX_PLAYERS": "8" },
    "gameType": "native-crossplay",
    "resolved": [{ "name": "Notch", "id": "069a79f4-…" }]
  } }
```

`port` 0 means the default. `env` is the API's `environment_variables`
verbatim; `gameType` is its `game_type` verbatim, because `native-crossplay` is
what forces offline mode and a value reduced to java/bedrock cannot say so.
`resolved` is whatever identities the host managed to look up — a name missing
from it is derived offline, or dropped in online mode, and is never a reason to
fail a launch.

**`settings` is optional, and its absence is the dangerous state, not the
harmless one.** Omitting it starts the server on the engine's own
configuration, which for Pumpkin includes `online_mode = true`. That is what a
host which has not been taught to send settings does — Android's Pumpkin
backend today — so the console says so rather than leaving it silent.

A refusal made **in this call, before the supervisor runs** — an invocation
the library cannot honour, a request it cannot parse — is written into the
console as `[Homerun] The server could not start: …` as well as returned. The
reply is the one place nobody looks: Android reads `ok` from it and moves to
the exit path, and the crash report that follows is built from the console.
A build compiled without `process-engine` refused every launch this way and
reported nothing but its own download progress until this existed. The
supervisor's own refusals (a taken port) already wrote their line;
`ServerHost::start`'s "already running" deliberately does not, because it
would land in the console of the server that *is* running.

`homerun_server_settings_preview` takes the same request and reports what would
be applied, without starting anything:

```json
{ "ok": true, "settings": { "motd": "…", "gameMode": "creative", … },
  "summary": "[Homerun] Settings applied: …", "advisories": ["…"] }
```

It exists because `homerun_server_start`'s arguments are otherwise only
observable by starting a real server, which blocks for its lifetime. A
misspelled key — `game_type` where the wire says `gameType` — compiles, links,
and yields a server on defaults with nothing anywhere saying so. `ios/coretest`
calls this with a request built by the same Swift the app uses.

What a setting *means* is decided in Rust (`engine_settings.rs`, on top of
`homerun-core::minecraft::settings`), never per host. See
`docs/ios-server-backend.md` for which settings a linked Pumpkin can honour.

### Responses

`homerun_server_state` → `{"state":"stopped|starting|running|stopping|crashed"}`

`homerun_server_stats` → `{"running":bool,"state":str,"serverId":str?,"startedAtMs":n?,"port":n?}`

`homerun_server_players` → `{"players":[{"name":str,"uuid":str?}],"max":n?}`, or
`null` when not running. Do not render a roster for a server nobody can join.

`homerun_server_logs_since` → `{"lines":[str],"cursor":n,"dropped":bool}`

`homerun_server_metrics` → `{"ok":true,"samples":{…}}` — one run's graph, oldest
sample first, each `{"t":ms,"memUsedMb":n?,"cpuPercent":n?,"playerCount":n?}`.
The supervisor samples for as long as a server runs, so a host polls to *read*
what is already there, never to cause a reading. `cpuPercent` can exceed 100:
a server uses more than one core, and clamping would hide the moment worth
seeing.

## Host integration rules

These are not style preferences. Each one corresponds to a failure that is
miserable to diagnose after the fact.

**Call `homerun_server_start` on a dedicated thread with ≥16 MB of stack.**
The default 512 KB overflows inside the engine and kills the process with no
panic report. It blocks for the server's entire lifetime.

```swift
let thread = Thread { /* homerun_server_start(...) */ }
thread.stackSize = 16 * 1024 * 1024
thread.start()
```

**Never set a timeout on start.** Starting a server legitimately takes
minutes on a phone — world generation, mod downloads. The bridge has no
blanket call timeout for the same reason (PROTOCOL.md §5).

**Poll logs with the returned cursor, not a stored count.** Cursors are
monotonic sequence numbers and never reused. `dropped: true` means the
buffer discarded lines you had not read — surface a "…output skipped…"
marker rather than pretending the gap did not happen.

Cursors do **not** survive a restart. The buffer clears between runs but
sequences keep climbing, so a cursor held across a restart reports
`dropped` instead of silently replaying the new run as if it continued the
old one.

**Free every returned string**, including from calls that failed.

## The console buffer

Bounded at 2000 lines (a few hundred KB). A backgrounded phone may not poll
for minutes while a busy server emits thousands of lines, so the buffer
evicts the oldest rather than growing without limit — and reports the gap
instead of hiding it.

### It holds the host's lines too — `homerun_server_note`

A launch is minutes of work before there is a server to have a console: a jar
to fetch, a runtime to unpack, a world to restore, a tunnel to bring up. Those
lines are the only answer to "why did starting take two minutes", and each
host used to keep a **second buffer** for them because there was nowhere here
to put them. Android's was a 30-line reimplementation of `log_buffer.rs`; iOS
had none at all, so it emitted them as events and a console opened after the
fact showed nothing of the launch.

`homerun_server_note` puts them in the one buffer, in the order they actually
happened relative to the server's own output.

### What clears it, and why it is not `start`

`homerun_server_console_begin`. A host calls it once, when it decides to
launch — *before* the slow part it is about to narrate. `homerun_server_start`
is far too late: by then the interesting lines are already written, and
clearing there deleted exactly what this exists to keep.

Two rules follow, and both were learned by getting them wrong:

- **A note never clears.** Inferring "a new launch has begun" from the first
  note looks tidy and is wrong: the on-stop backup writes `[Backup] …` lines
  for minutes *after* a run has ended, so the first of those wiped the console
  of the run the player had just watched stop.
- **`start` still clears a console holding a finished run.** The safety net for
  a host that never calls `console_begin` — it then behaves exactly as it did
  before any of this existed. The cost of forgetting is losing that launch's
  own notes, never showing the previous run's.

**One buffer means one emitter.** A host that both emits a note itself and
re-emits it from the pump reading the same buffer sends every line twice. The
pump is the one that turns a console line into an event; `note` only writes.

## Crash handling

`crash.rs` installs a panic hook that writes `crash-reports/panic-<ts>.txt`
with a backtrace, and keeps the message so a failed start can explain itself.

**The hook is installed from `core_dispatch::call`**, at the top of every
call, not from `server.rs`. It is idempotent behind an `AtomicBool` so the
cost is one relaxed atomic load, and it means a device that never hosts a
server still has a panic hook — which was not true before, and is most of the
app.

**There are two crash directories, and the split matters.** `set_crash_dir`
takes the server's data directory, which restic backs up as part of the world:
panics were riding into players' world backups. `set_app_crash_dir`
(`error.attach`) points at host storage instead, and the hook prefers it.

Two more things worth knowing:

- **Every `extern "C"` function wraps its body in `catch_unwind`.** A panic
  crossing the FFI boundary is undefined behaviour, not a crash you can
  debug.
- **The last-panic slot is cleared at the start of every run.** Without
  that, a panic from anywhere earlier in the process leaks into the *next*
  crash's message and blames the wrong thing. That was a real bug, caught by
  a test, and `a_crash_is_not_blamed_on_an_older_unrelated_panic` pins it.

### Reporting a crash off the device — `errors.rs`

`reporting.crash.report` takes an optional `context` — the host's description
of itself — and completes it with what only this crate knows: the ABI version,
the engines compiled in, and the app's own log through `app_logs::collect`.
See `crash_host_context`. Four arms carry app errors beside it:

| Arm | Does |
|---|---|
| `error.attach` | Points the app-level crash directory. Once, at launch. |
| `error.report` | Locks the process-global ledger, decides, returns a `Request` or a hold. |
| `error.stash` | Writes one file. No network — the caller is already dying. |
| `error.drain` | Reads last launch's files, **deletes each before parsing**, caps at 5. |

The delete-before-parse is a loop cut, not tidiness: a report that panics the
core while being parsed would be re-read on the next launch, and again.

The ledger is a `static OnceLock<Mutex<Ledger>>` here rather than round-tripped
through the host like `lifecycle` and `metrics`. Four threads produce into it —
the JVM crash handler, the WebView bridge thread, the panic hook, the host's
reporting coroutine — and a per-caller copy would turn "20 sends per session"
into "20 per caller".

**None of this exports a symbol, so `FFI_ABI_VERSION` did not move.** See
[`app-errors.md`](./app-errors.md).

## One server at a time

Enforced in `state.rs` and again in the hosts. The engine keeps global state
and distinguishes worlds by process working directory, so a second concurrent
server is not a feature that was skipped — it is not expressible without
restructuring upstream.

Starting a second server returns a player-facing message rather than an
error code:

> Another server is already running. Stop it first — this device can host one at a time.

This matches the desktop app, where users create many servers and run one.

## The engine seam — `engine.rs`, `pumpkin_engine.rs`

`Engine` is the seam, and there are two implementations.

`PumpkinEngine` (`pumpkin_engine.rs`) drives the real server, and sits behind
the **`pumpkin-engine` feature, which is off by default**. `StubEngine` stands
in otherwise: it reports startup, honours stop requests, and can be told to
fail, which is how the failure paths are covered without a real server.

That default is load-bearing for the development loop. With the feature off,
the crate builds and its 36 tests run in about two seconds on any machine,
with no Pumpkin, no wasmtime, and no device. With it on, a cold build pulls in
the whole server. `server::host()` picks between them at compile time; the app
builds enable the feature (`scripts/build-rust.js`), and `--stub` cross-builds
a target without it.

### What `PumpkinEngine::run` has to do

Pumpkin is a program that was made into a library, and the seam has to absorb
that. In order:

1. **`reset_server_state()`** — the stop flags are process-wide statics. Skip
   this and a second run sees the previous run's stop request and exits
   immediately, which looks like "the server won't start".
2. **Install the console capture** (see below).
3. **`set_current_dir(data_dir)`** — the engine selects a world by working
   directory. This is the concrete reason only one server runs at a time.
4. **Load the config, then override it**: the port (there is no API for it —
   it lives in `configuration.toml`), and `commands.use_console = false`.
   There is no stdin on a phone, and with the console enabled `start()` blocks
   forever waiting on a readline that cannot arrive.
5. **`PumpkinServer::new(...)`**, which returns `Result` in our fork — see
   below.
6. Stash the `Arc<Server>` and a `tokio::runtime::Handle` in a static, so
   `command` and `players` can reach the server from the host's threads while
   `run` is blocked inside the runtime.
7. **Bridge the stop signals.** Ours is an `AtomicBool`; Pumpkin's is a pair
   of statics plus a cancellation token. A watchdog task polls one and calls
   `stop_server()`. Without it a stop request is recorded and nothing acts on
   it.

### The fork patch that makes this possible

`PumpkinServer::new` used to call `process::exit(1)` on any bind failure. That
is fine for the standalone binary but fatal here: it takes the whole app down
with no crash report and no way for the UI to explain itself, violating the
first rule in this repo's `CLAUDE.md`. The fork now returns
`Result<Self, io::Error>` and the binary makes the exit decision itself.

The pre-flight in `preflight.rs` still runs first, because it produces a
better message and avoids building a server that then has to be torn down.
But it is no longer the only thing standing between a taken port and a
vanished app.

### Console output

Pumpkin logs through `tracing` to fd 1, and there is no writer hook to
redirect that to a callback. So the file descriptors themselves are replaced:
`pipe`, then `dup2` over fds 1 and 2, and a reader thread turns the bytes back
into lines.

> **This is process-wide and permanent.** It is installed once, and it
> captures *anything* in the process that writes to stdout, not just Pumpkin.
> The reader thread also outlives any single run, which is why it appends to
> the crate's console buffer directly rather than borrowing `run`'s `on_line`
> — handing a thread a borrow that lives for one call would dangle the moment
> that run ended.

One consequence worth knowing before you debug with `println!`: anything you
print after a server starts goes into the console buffer, not your terminal.
And printing lines you *drained* from that buffer feeds them straight back
into it, which loops. `examples/boot_engine.rs` keeps a `dup` of the original
stdout from before the redirect for exactly this reason.

ANSI colour escapes are stripped on the way in. The engine's logger assumes a
terminal; the console it actually feeds is a WebView, which renders the
escapes as literal `[2m` garbage in front of every line.

### Readiness

`run` takes an `on_ready` callback and must call it once the server is
genuinely accepting connections. That is what moves the host from `Starting`
to `Running`.

> **The host cannot infer this moment.** `run` blocks from its first instant,
> so before this signal existed the host flipped to `Running` immediately
> before calling it — and with a real engine that meant telling a player to
> join a world that was still generating. Pinned by
> `a_slow_start_stays_starting_until_the_engine_is_ready`.

An engine that returns without ever calling `on_ready` never started, and the
failure path in `StubEngine` deliberately does not call it.

## Building

```bash
cargo test                     # host-native; no device, no Pumpkin, ~2s
cargo clippy --all-targets
cargo build --release

# mobile targets — the engine feature is what links the real server
cargo build --release --features pumpkin-engine --target aarch64-apple-ios
cargo build --release --features pumpkin-engine --target aarch64-apple-ios-sim
cargo ndk -t arm64-v8a build --release --features pumpkin-engine
```

In practice use `node scripts/build-rust.js ios`, which adds the feature and
stages the artifact. Add `--stub` to cross-compile without the engine when you
only want to know that the FFI surface still builds for a target.

`crate-type` is `["staticlib", "cdylib", "rlib"]` — iOS links the `.a`,
Android loads the `.so`, and `rlib` keeps the test build fast.

Release uses `opt-level = "z"` + LTO + strip: a debug build of the server is
around 1.8 GB, which is impractical to ship or even copy to a device.

## Backups — `backup_job.rs`, `backup_engine.rs`

Android spawns the restic binary. iOS cannot spawn anything, so it links
`rustic_core` instead, behind the **`backup-engine`** feature. Same repository
format either way, which is the entire point: a world backed up from a phone
has to be restorable from the desktop.

The feature is set per target in `scripts/targets.js`. Android must not enable
it — it would add ~5.6 MB for a job it already does better.

### The surface

Five calls, deliberately *not* methods on `homerun_core_call`:

| Call | Blocks? | Notes |
|---|---|---|
| `homerun_backup_available` | no | 0 without the feature |
| `homerun_backup_latest_snapshot` | seconds | networked; never on a UI thread |
| `homerun_backup_run` | **minutes** | ≥8 MB stack, one at a time |
| `homerun_backup_progress_since` | no | cursor poll, main-thread safe |
| `homerun_backup_cancel` | no | cooperative, coarse |

`core_dispatch` is one table compiled for both platforms and every method in it
is instantaneous and pure — hosts call it from the main thread without
thinking, because that has always been safe. A method there that opened TLS and
blocked for four minutes would eventually be called the same way. A separate
symbol with a loud doc comment is the only guard available.

A build without the feature still exports all five; they answer "this copy of
the app cannot back up worlds". Without that stub, host and Android builds
would fail to link against a header that declares them.

### Two things a linked engine does differently

**There is no exit code.** The host passes no `exitCode` to `backup.classify`,
so restic's exit-3 "completed with warnings" is unreachable and
`Failure::succeeded()` can never be true. A snapshot came back or it did not.
Warnings ride in the reply so the host can say something useful — not so it can
call a failure a success.

**rustic does no repository locking.** It neither writes a lock nor notices one
a desktop restic client left. The backup lease, which the API owns and
`backup::lease_decision` interprets, is the only thing keeping two devices out
of one repository.

There is a third, subtler one. `backup::classify` reads the failure *text*, and
rustic's wording shares nothing with restic's — a refused connection surfaces
as `error sending request … client error (Connect)`, which matched none of the
transient patterns and so reported a dropped wifi connection as a permanently
broken backup. `is_transient` now carries both dialects, with a test pinning
each.

### Cancellation is cooperative, and coarse

rustic exposes no cancellation hook, and unwinding out of a progress callback
would panic through its worker pool. So `homerun_backup_cancel` sets a flag
checked at phase boundaries: a cancel during the open or index phases lands
quickly, one during a transfer lands when the transfer ends.

That is enough for what it is for. iOS gives a backgrounded app about five
seconds' warning, and the useful thing to do with them is report the backup
failed so the lease closes — not to stop the work.

### Progress

Same shape as the console: the engine writes into a `LogBuffer` and the host
reads by cursor. Only a `ProgressType::Bytes` progress moves the counters —
rustic runs several at once, and letting all of them write makes the percentage
jump between unrelated denominators. `total` of 0 means "not known yet".

A `log::Log` sink is installed alongside, because rustic reports a skipped or
unreadable entry through `log::warn!` and returns a complete snapshot anyway.
Without it a linked engine has *no* message to classify.

## Status

Implemented and tested: state machine, console buffer, pre-flight, crash
capture, the whole C surface, one-server enforcement.

`PumpkinEngine` and the stdout/stderr redirection — the **linked** path, which
is now iOS's only — still have not been exercised against a running world.
Treat the run sequence above as the design until that has been done.

The **spawned** path has. Pumpkin boots a world on Android as a child process
under `ProcessEngine`: it prints `Server is now running.`, applies the
settings the host leaves in `homerun-settings.json`, and saves all three
dimensions on `SIGTERM`. Two things had to be fixed for that and neither was
visible from a green suite — `console::is_ready` did not recognise Pumpkin's
readiness line, so a launch sat in `starting` until it timed out behind a
healthy server; and upstream's `main` registers its signal handlers in
sequence, so `SIGTERM` — rung two of the stop ladder — killed the server
without saving. `rust/homerun-pumpkin-bin` exists mostly for the second.

The 142 tests all run against `StubEngine`.

The backup engine **has** been run, on an iOS simulator: `ios/coretest/`
compiled for `arm64-apple-ios-sim` and spawned with `simctl` does a real
backup, lists the snapshot, restores it and compares the bytes — 55 checks,
including that the snapshot's hostname is the device id, which is what the API
resolves `pushed_by` from. What has not been done is any of it on a **device**,
against a real `rest:` repository, or against a world large enough to test what
`to_indexed()` costs in memory. That last one is the risk worth watching.

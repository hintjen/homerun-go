# Multi-game engine session recovery

> Historical checkpoint, describing recovery commit `5aa9490`, not the current
> implementation. Read [the continuation handoff](./multi-game-engine-handoff.md)
> and [runner documentation](./game-runner.md) for current behavior and limits.

## Overview

Recovered 2026-09-18 from the transferred engine session log. This branch preserves the interrupted start of the runner, on top of the completed engine effects branch. It is a WIP snapshot, not a working runner or a merge candidate.

Branch: `recovery/multi-game-engine-2026-09-18` in `hintjen/homerun-go`.
Base: `0236b9e00e982bf5fb874a4a2105055bd518145f` (`engine/supervisor-effects`, draft PR #21).
Local continuation checkout: a separate worktree; use the remote branch on any other machine.

## Completed before interruption

- [PR #19](https://github.com/hintjen/homerun-go/pull/19), `supervisor/rename-crate`: supervisor package/directory rename, stable shipped library names and C ABI. Keep `[lib] name = "homerun_pumpkin_ffi"`; dependents use an explicit package rename.
- [PR #20](https://github.com/hintjen/homerun-go/pull/20), `engine/core-descriptor`: pure descriptor decisions, validation, settings, single-pass templating, invocation and fetch plans, control/stop/forwards, doctor/licence gates, `engine.*` dispatch, exported schema and `docs/game-engine.md`.
- [PR #21](https://github.com/hintjen/homerun-go/pull/21), `engine/supervisor-effects`: fetcher, anonymous steamcmd, RCON/WebSocket RCON, platform module and descriptor-driven ProcessEngine, behind default-off `game-engine`.

These PRs are stacked, not independent branches from main. The coordinator accepted the stack. None was merged as part of recovery.

Historical test reports: PR #19 had 753 core and 171 supervisor tests; PR #20 had 882 core and 186 supervisor tests. PR #21 reported 197 supervisor tests without game-engine and 230 with it, and `npm test` exiting zero. These are session evidence, not fresh verification here.

The schema was committed at `rust/homerun-core/schema/game.v0.json`, with `$id` corrected to `https://gethomerun.app/schemas/game.v0.json`. The coordinator pinned it in the monorepo.

## Exactly what was recovered

The session created local branch `engine/game-cli` at 20:25 UTC, then wrote two files:

- `rust/homerun-game-cli/Cargo.toml`: binary name `homerun-game`, path `src/main.rs`, dependencies on core and supervisor with `game-engine`, serde/serde_json, independent release profile.
- `rust/homerun-game-cli/src/protocol.rs`: NDJSON command/event types, error codes, serialization and protocol tests.

Both Write calls had successful tool results. The session then hit its usage limit at 20:27:05 UTC. No later work appears in the supplied log. There is no recovered `main.rs`, command implementation, runner test execution, Cargo.lock, runner commit or PR. The protocol file has not been compiled. The transferred log contains some mojibake in comments; recovery retains its content rather than silently rewriting source.

The runner crate is therefore NOT BUILDABLE as this snapshot stands: the declared binary entry point is absent. Preserve this checkpoint before implementing it.

## Resume the runner (planned PR 4)

Read `docs/game-engine.md`, `docs/ffi.md`, the repo instructions and tests-that-bite skill. Fetch the latest `hintjen/homerun:feat/multi-game` and read `plans/multi-game-contracts.md` section 2 plus `plans/game-onboarding-pipeline.md`; the contracts win over older sketches.

Implement `doctor`, `fetch`, `launch`, `stop`, `probe`, `verify` (with JSON output), and `supervise` over stdin/stdout NDJSON. Reuse the supervisor's process lifecycle and core decisions. Keep the runner independent of the Pumpkin engine feature and use no Cargo workspace.

Critical protocol requirements:

- Versioned hello/ready handshake; stdout contains protocol JSON only, diagnostics on stderr.
- stdin EOF gracefully stops the server and shuts down; do not orphan the server when Electron dies.
- One running server per runner process. Preserve command responsiveness during long operations.
- Refuse fetch/start unless licence acceptance is explicit. Check this before descriptor validation.
- Revalidate inline descriptors on both fetch and start; collect invalid-descriptor sentences into one `descriptor_invalid` error. Validation warnings do not block startup.
- Start must handle a missing runtime (fetching with progress); compare the latest contracts with the desktop implementation before coding this.
- Report bound ports before server-started, player platform IDs rather than Minecraft UUIDs, process stats as counters, console responses with request IDs, crashes with log tails, and bounded shutdown.
- Unknown commands/fields must degrade per contract. Port, readiness, fetch, spawn and licence failures must use the specified codes and player-readable messages.

Use a fake server and fixture descriptor for an end-to-end direct fetch -> launch -> readiness -> console -> stop test, plus EOF shutdown and error cases. The recovered protocol tests alone cannot establish lifecycle correctness. Follow tests-that-bite: deliberately regress each new behavior and observe the relevant test fail, restore it, and rerun.

## Build and publishing (planned PR 5)

Still unstarted in this log:

- Runner target in `scripts/targets.js`: Windows x64 MSVC, requiresWindows, static CRT; npm build command.
- Runner artifact and manifest in `scripts/publish-desktop-artifacts.js`, build ID from SHA-256 prefix.
- Core addon version/build/ABI export and manifest.
- Agree exact names and paths with the monorepo release-workflow workstream before treating placeholders as final. Rehash after signing; binary before manifest.

No merges, publication, deployment or release workflow dispatch were authorized by this recovery. Continue with branches and draft PRs.

## Integration and remaining uncertainty

Desktop recovery: `hintjen/homerun:recovery/multi-game-desktop-2026-09-18`.
UI recovery: `hintjen/homerun-app-ui:recovery/multi-game-ui-2026-09-18`.
Cross-repository handoff: `plans/multi-game-session-recovery.md` on that desktop recovery branch.

Windows interrupt-stop is unsupported; the ladder proceeds to terminate. WebSocket RCON is one connection per command and does not stream arbitrary server output; use captured logs for server-log events. Settings remain opaque strings, empty means unset; join URLs cannot expose secrets/settings and RCON ports cannot be exposed.

No real Rust game server was downloaded or probed. Rust descriptor values are hypotheses. A person must accept the applicable terms before a real download. Run one heavy job at a time on the shared Windows/WSL machine.

This recovery inspected the supplied log and remote base, then restored source. It did not access the remote machine's current filesystem, so any work performed after that log export would need separate reconciliation. Source session ID: `5dcb16d9-ff3c-461e-80ac-96ae5e7222d6`.

# Multi-game engine continuation handoff

Updated 2026-09-18. This file is portable: use the remote branches below, not
another machine's checkout paths. Raw session logs are not committed.

## Start here

Fetch `hintjen/homerun-go`, check out `feat/multi-game-engine`, and read
[game-runner.md](./game-runner.md), [desktop-game-artifacts.md](./desktop-game-artifacts.md),
and the current monorepo `feat/multi-game:plans/multi-game-contracts.md`.
The engine work is consolidated on `feat/multi-game-engine`, targeting `main`.
It contains the complete former PR 19–23 stack plus the current main changes
through `56af846`. The original branches remain available as historical
checkpoints; use the consolidated PR for review and merging.

The former stack was:

| Piece | Remote branch | Review |
|---|---|---|
| Supervisor rename | `supervisor/rename-crate` | [PR 19](https://github.com/hintjen/homerun-go/pull/19) |
| Core descriptor decisions | `engine/core-descriptor` | [PR 20](https://github.com/hintjen/homerun-go/pull/20) |
| Supervisor effects | `engine/supervisor-effects` | [PR 21](https://github.com/hintjen/homerun-go/pull/21) |
| Runner CLI and supervise | `recovery/multi-game-engine-2026-09-18` | [PR 22](https://github.com/hintjen/homerun-go/pull/22), implementation `be7ce44` |
| Windows artifacts and addon identity | `engine/game-artifacts` | Stacked on the runner branch |

The recovered two-file runner checkpoint is `5aa9490`; it has been superseded
by working source. No merges, signed releases, S3 uploads or vendor-game
downloads were performed in this continuation.

## What now works

The standalone runner supports doctor, fetch, launch, stop, probe, verify and
NDJSON supervise. It validates inline descriptors, requires explicit licence
acceptance, fetches missing runtimes, handles one server, observes readiness
and bound ports, streams stdout/stderr, routes console replies by request ID,
reports stats/presence/crash tails, and follows the stop ladder on stop or EOF.
Downloads remain cancellable even when the HTTP peer sends nothing. Tests use
real local subprocesses and HTTP fixtures, without accepting a vendor licence.

The Windows release target produces `homerun-game.exe` with the static CRT.
The addon exports version, Node ABI and source build identity. The artifact
script hashes final bytes and prepares immutable URLs plus manifests:
`game-runner/latest.json`, `core/latest.json`, and the preserved
`pumpkin/latest.json`. Exact filenames and signing order are documented in
[desktop-game-artifacts.md](./desktop-game-artifacts.md).

## Verified on Windows

- `npm test` after consolidation: 908 core tests, 243 supervisor tests (one pre-existing ignored),
  8 protocol and 11 runner lifecycle tests; existing ABI/revision/capability/UI
  bundle checks; 22 real addon smoke checks; artifact tests. All passed.
  The artifact suite now has six tests, including preservation of Pumpkin's
  Minecraft version probe. iOS and Android bridge conformance checks passed.
- `npm run rust:game-runner` and `npm run rust:core-node`: release builds passed.
  Both Windows artifacts passed the check for redistributable DLL imports.
- Release runner hello/shutdown succeeded; `ready.build` matched the prepared
  manifest's digest prefix.
- Deliberate regressions proved tests catch bypassed licence acceptance,
  skipped graceful-save commands, and a stale artifact identity after a byte
  change. Source was restored and tests rerun green.
  Consolidation also exposed panic-test interference: the runtime tests now
  hold the crash module's shared guard through runtime shutdown. A deliberately
  removed Pumpkin version query failed its regression test, then passed after
  restoration.

## Other sessions' pushed work

Fetched and confirmed these remote tips in this continuation:

| Repository | Branch | Tip |
|---|---|---|
| `hintjen/homerun` | `recovery/multi-game-desktop-2026-09-18` | `9c73bc89e008f78058933134c69c7b0fb3c6c95f` |
| `hintjen/homerun-app-ui` | `recovery/multi-game-ui-2026-09-18` | `5dd2a9de75540c73564c21d1837cb1376591222c` |
| `hintjen/homerun` API | `feat/multi-game` | `aae18c77d86901e0e0d4161ea6257bea0291e0f1`, draft PR 897 |

The other session reported clean typechecks, 76 UI suites / 785 tests, 71
desktop suites / 1,089 tests, and 83 API tests. It fixed the null additional
forward join-address fallback, desktop filesystem test isolation, stop during
runner download coverage, bundled game descriptors, and desktop documentation.
Those counts are its report, not independent reruns here. Actual desktop
packaging and full Windows integration remain unverified.

## Next work, in order

1. Review the consolidated engine PR and the desktop/UI recovery branches. Reconcile the
   monorepo branches with the latest API/contracts before integration. Nothing
   in this handoff authorizes merging or releasing the work.
2. Wire the engine artifact names into the monorepo release workflow: runner
   build/sign/upload, addon manifest, final-byte digests after signing. Update
   `download-assets.js` to pin and verify the addon digest. Build the desktop
   package with its UI bundle and downloaded assets.
3. Exercise the actual Windows runner through Electron using an approved
   fixture: start with an absent runtime, port conflict, console response,
   stop during fetch, normal stop, EOF, crash, restart and backup paths.
4. Onboard a real game only after a person accepts its applicable terms.
   Rust's descriptor is still hypothetical. Probe real ports, readiness,
   save/restart persistence, stray writes, player roster and gateway reachability.
5. Apply the API migration to the intended database and onboard/register the
   game before expecting it in the UI. The registry was empty and migration
   unapplied in the coordinator's handoff; this continuation did not change them.

## Limits that must stay visible

INI/TOML/XML config merging and A2S player queries are not implemented. The
runner supports JSON/properties configuration and specific JSON RCON roster
shapes. Unsupported config formats refuse explicitly. A launcher whose child
owns the listening sockets needs process-discovery support. Port preflight
refuses occupied preferred ports rather than choosing replacements.

Probe/verify demonstrate lifecycle observations, not complete game onboarding:
the evidence explicitly marks save persistence, stray writes, some roster
formats and gateway reachability unverified. Windows interrupt-stop is
unsupported and falls through to the next stop rung. tunnel-started only
means the wireproxy process spawned. See the runner doc before extending
these paths. No real Rust game has run here.

API caveats remain: per-game limits are a TODO, one gateway service per server,
and game-server state/stats endpoints were not exercised in the API tests.
The other session reported pre-existing lint configuration failures on main.

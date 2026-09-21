# Multi-game engine review follow-up

Review received 2026-09-21 for PR 24 at `9ec20d8`. The PR remains draft and
must not be treated as ready for merge or real-game use from fixture results.
This record distinguishes fixes from findings still requiring investigation.

## Merge prerequisites addressed in this follow-up

- B1: use `homerun_supervisor` in Pumpkin's Minecraft version query after the
  crate rename. `npm run check:pumpkin` checks the actual binary crate and the
  supervisor with `--features pumpkin-engine`; `npm test` alone is insufficient.
- H1: bare artifact preparation still requires only Pumpkin and the addon.
  A built runner is included when present; explicit `--only game-runner`
  refuses a missing runner. Tests execute the script entry point with the
  legacy staging layout. This checks preparation, not an S3 workflow run.
- H2: Windows TCP detection no longer reads a localized state label. It uses
  the numeric unspecified foreign endpoint from `netstat -ano`, handles both
  protocols in one process, and polls ports at most once per second. Tests
  cover localized IPv4/IPv6 listeners, UDP, connected sockets and other PIDs.
- Malformed known commands emit an error with the recoverable server ID;
  malformed console requests also complete their recoverable request ID.
  Unknown commands retain forward-compatible ignore behavior. The subprocess
  regression covers missing serverDir, null settings, string acceptance, an
  incomplete fetch, console response correlation and continued status/EOF use.
- Artifact preparation again rejects missing Pumpkin Minecraft metadata,
  including direct callers of `prepareArtifacts`.
- Corrected the remaining old crate paths in Android reporting documentation.

## Before a real game: unresolved high-risk findings

**Hard gate:** no real-game onboarding or operation until bind enforcement,
the argument-value policy, Job Object ownership with process-tree killing,
lossy UTF-8 log decoding and the null-drop fix below are implemented and
verified. Merging the engine PR does not waive this gate.

- H3: bindAddress is restricted to loopback but not passed into invocation;
  observed ports omit the local address. Private/admin sockets can therefore
  bind more widely than intended. Implement and verify actual bind enforcement.
- H4: argv boundaries do not prevent a game's own argument parser from treating
  player strings as flags. Define a game-appropriate argument-value policy in
  core and the API. Do not assume shell-free spawning resolves this issue.
- Windows Job Object ownership and process-tree termination are absent. Test
  abrupt runner death and launcher descendants, including the tunnel child.
- Log readers can stop on invalid UTF-8. Use loss-tolerant decoding and test
  readiness/steamcmd output containing non-UTF-8 bytes.
- Consecutive omitted/null argument values can remove unrelated flags in the
  inherited invocation builder. Add a regression for adjacent placeholders.
- Reconcile API enum/options, integer bounds and newline validation with core
  and the pinned schema in `hintjen/homerun:feat/multi-game`.

## Other unresolved review findings

- Stats launch PowerShell every two seconds. Revisit sampling frequency or use
  native counters; avoid expensive subprocess sampling at that cadence.
- Unpinned Steam runtimes update/validate at every start. Define update policy
  separately from a missing-runtime fetch.
- Vendor and steamcmd subprocesses inherit the host environment; establish a
  minimal compatible environment before handling real credentials.
- Fetcher: investigate resume after HTTP 416, in-place extraction and stale
  stamps, HTTPS/redirect policy and streaming integrity checks, and whether
  Steam buildId is incorrectly treated as a beta branch.
- No artifact `--verify` mode checks manifests against post-signing files.
  Signing must still precede manifest preparation.
- Exit codes are omitted; protocol mismatch emits no event; JSON config values
  remain strings and null leaves existing keys; console replies are not
  redacted and embedded newlines can become multiple stdin commands.
- Readiness/presence substring matching can be spoofed by chat; batch-script
  executable paths are not refused. Review both against real descriptors.
- Restore useful lost process-engine comments where the control flow needs
  explanation.

These are review findings, not all independently reproduced in this follow-up.
Ownership of inherited code does not change their relevance to PR 24.

## Corrected evidence claims

Fresh Windows validation for this follow-up:

- `npm run check:pumpkin` passed for the binary crate and the supervisor's
  `pumpkin-engine` feature. These are type checks, not packaged game launches.
- `npm test` passed: 908 core tests, 244 supervisor tests (one pre-existing
  ignored), 8 protocol tests, 12 runner integration entries (11 substantive
  scenarios plus the fixture), nine artifact tests and the existing checks.
- Deliberately restoring the old crate reference, mandatory runner artifact,
  English-only listener filter and silent malformed-command handling failed
  the corresponding checks. Source was restored and rerun green.
- `git diff --check` passed. No release publication, Electron integration or
  real-game test was performed. The locale regression uses translated output
  fixtures; it is not a run on a German Windows installation.

The original runner suite reported 11 integration tests, but `fake_game` is a
fixture entry point: that was ten substantive scenarios. This follow-up adds
one substantive malformed-command scenario. A secret present in fixture input
does not prove streamed-log redaction without a secret in captured output.

Remaining coverage gaps include RCON routing, tunnel commands, actual UDP
lifecycle, explicit stop during fetch, oversized commands, console concurrency,
secret-bearing game logs and Windows forced-parent-death cleanup. None of the
fixture tests establishes Electron integration or real-game behavior.

The reviewer reported a Linux `/proc` memory assertion failing on this branch,
the inherited effects branch and main. This follow-up does not claim to have
fixed or independently rerun that Linux failure.

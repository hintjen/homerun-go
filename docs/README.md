# Homerun Mobile Documentation

## Overview

Technical documentation for the iOS and Android hosts. Each subsystem gets
one file, written **as it is built** — see
[`../plans/shared-milestones.md`](../plans/shared-milestones.md#documentation-is-part-of-the-milestone).

UI documentation lives in
[`homerun-app-ui`](https://github.com/hintjen/homerun-app-ui); desktop
main-process documentation lives in
[`homerun`](https://github.com/hintjen/homerun) under `homerun-ui/docs/`.

## Documentation Index

### 🦀 [Pumpkin FFI](./ffi.md)

The Rust library both hosts link — server lifecycle, console buffering,
port pre-flight, and crash reporting behind a C ABI.

**Contains**:
- Why the crate exists (so the Pumpkin fork can shrink to library patches)
- The C surface, JSON conventions, and string ownership
- Host integration rules: the 16 MB stack, no start timeout, log cursors
- The console ring buffer and cursor semantics
- Crash handling and why the last-panic slot is cleared per run
- One-server-at-a-time, and where it is enforced
- Wiring a real engine in behind the `Engine` trait

**Read this for**: Calling the server from Swift or Kotlin, wiring Pumpkin
in, or debugging a server that will not start.

---

*Add an entry here whenever you add a doc. A doc nobody can find is not
written.*

<!--
Planned, one per milestone (see plans/shared-milestones.md):

  ios-host.md             iOS      M0
  ios-bridge.md           iOS      M1, extended at M2
  ios-server-backend.md   iOS      M3
  ios-lifecycle.md        iOS      M4
  android-host.md         Android  M0
  android-bridge.md       Android  M1, extended at M2
  android-server-backend.md  Android  M3
  android-lifecycle.md    Android  M4
-->

## House style

Match [`ffi.md`](./ffi.md) and the desktop repo's `homerun-ui/docs/`:

```markdown
# Subsystem Name

## Overview
What it is and why it exists. Lead with the problem it solves.

## <Component> — `path/to/File.swift`
Sections named after the file they document, so a reader can jump from a
stack trace to the right heading.

## File map
| File | Role |

## Triage
Symptom → cause → fix.
```

- Document the **why**, especially anything non-obvious about the platform.
- Mark load-bearing details that break silently when changed.
- End with triage, symptom-first — that is how docs actually get read.
- Update in the same commit as the behaviour change. A stale doc is worse
  than none, because people trust it.

---

**Maintained by**: Homerun Development Team

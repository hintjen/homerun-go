---
name: tests-that-bite
description: Write tests that can actually fail, and prove it by breaking the code on purpose. Use when adding or changing a test, before changing behaviour that tests are supposed to protect, when moving a responsibility between components, or when a bug was found by using the app while the whole suite stayed green — that last one is usually a test performing the very step it was meant to be testing.
---

# Tests that can actually fail

A test that cannot fail is documentation with a runtime cost. The suite in
this repo is checked for teeth rather than counted — see
[`docs/shared-core.md` § The tests are the deliverable](../../../docs/shared-core.md),
which lists deliberate regressions and what caught each.

**The rule: after writing or changing a test, break the code on purpose and
read the failure message.** Not "run the suite and see green" — green is what
a useless test also produces.

## The failure mode that keeps happening

**A test that performs the step under test cannot fail when that step
disappears.**

The stop ladder is the canonical case here. The supervisor is supposed to
write `stop` to the server's stdin before escalating. Its test did this:

```rust
// as the host does
engine.command("stop");
stop.request_stop();
assert_eq!(outcome, RunOutcome::Stopped);
```

Then the host was rewired so the *supervisor* owned the ladder — and nothing
wrote `stop` any more. Every stop sat through the full 30-second save grace in
silence and was then terminated. The test passed throughout, because the test
itself was supplying the missing step.

It was found by a person using the app. Rewritten to set the signal and
nothing else, it fails in 30 seconds flat:

```
the console rung was skipped — this took 30.589496s, so the server was
terminated rather than asked to save
```

**Look for this whenever a responsibility moves between components.** The test
was honest when written; it rotted the moment the caller changed. Setup that
mirrors "what the caller currently does" is the thing to be suspicious of.

## Assert the consequence, not the mechanism

The stop test above asserts **elapsed time**, because the bug was "it took 30
seconds", not "it returned the wrong enum". `RunOutcome::Stopped` was correct
throughout the breakage.

Ask what the *user* would notice, and assert that. Latency, a lost world, a
line missing from a console, a backup that never ran. Mechanism assertions
pass happily while the thing they mechanise has stopped mattering.

## Break each rule separately

If two tests are meant to cover two different rules, breaking one rule must
fail *only* its own test. Otherwise you have two tests covering one rule and a
gap you cannot see.

Worked example — the console-clearing pair:

| Break | Expected |
|---|---|
| make `start` clear unconditionally | `a_launch_keeps_the_notes_it_wrote_before_starting` fails, the other passes |
| make a note clear the buffer | `announcing_a_launch_clears_the_last_run` fails, the other passes |

Both behaved exactly that way, which is what makes them two tests rather than
one written twice.

## A test written by watching behaviour asserts what it watched

If you write the test *after* seeing the code run, it tends to encode the
observed output rather than the rule you meant. Breaking it on purpose is what
separates those: a test pinning a rule fails with a message about the rule; a
test pinning an observation fails with a diff nobody can interpret.

Write the failure message for a reader who does not know what you were doing:

```
the on-stop backup wiped the run it belongs to: ["[Backup] Backing up the world…"]
```

That names the behaviour, the victim, and shows the evidence.

## Build the fixture out of the real artifact

When a device, a crash report or a log gave you the evidence, paste **that**
into the test — not a cleaned-up version of it. The junk in real data is load
bearing.

A grouping fix for iOS panic reports was written against a stack of repeated
`__mh_execute_header` frames, which is what the report mostly is. The real
report also had one line that resolved: `___isPlatformVersionAtLeast`, an
exported builtin sitting in the same position in every backtrace on the
platform. The tidied fixture would have passed on the first try. The real one
failed, because that single frame became the whole signature and grouped every
panic together exactly as before.

So when a new test fails the first time you run it, before touching either
side: work out whether it is telling you the **fix** is incomplete. That is the
more common answer, and it is the whole reason the test was worth writing.

## How to break things safely

```bash
cp path/to/file.rs /tmp/f.bak
# edit — a scripted patch is easier to reverse than a hand edit
cargo test --manifest-path … 2>&1 | grep -E "your_test|test result"
cp /tmp/f.bak path/to/file.rs && rm /tmp/f.bak
```

Do not use `git checkout` to restore — it discards everything else you have
been working on. Always re-run green afterwards, and confirm the file is
actually back before committing.

Three traps, each hit for real:

- **A broken guard can make a test hang instead of fail.** Remove "a Stop ends
  the wait" and a test that waits for the Stop waits forever — the suite never
  finishes, so nothing says the guard is gone. Run the call under test on its
  own thread and fail on a timeout (`recv_timeout`), and give any scripted
  mutation run a timeout too (`subprocess.run(..., timeout=…)`).
- **An interrupted mutation run leaves the mutation in place.** Nothing
  restores the file when the run is killed. Before anything else, grep for
  the mutated line and put it back, then kill the stuck test binary
  (`target\debug\deps\<crate>-*.exe`) and its `cargo` parents.
- **Git Bash heredocs on Windows can eat backslashes.** A `\` line
  continuation in a Rust string sent through `python - <<'EOF'` arrived as a
  run of spaces in the message. Write patch scripts to a file and run the
  file; afterwards, grep the diff for `"[^"]*\S {5,}\S` to catch a string that
  lost its continuation.

## When the suite was green and the bug shipped anyway

That is information about the suite, not just about the bug. Before fixing,
work out **why nothing failed**, and fix that too — otherwise the next
regression in the same area is equally invisible. Usually one of:

- the test supplied the missing step itself
- it asserted the mechanism, which still held
- nothing covered the second run, where state carried over from the first
- an error was swallowed, so the failure never became an observable
- **the code does not compile in the suite at all.** `pumpkin-engine`,
  `device-ws` and `backup-engine` are off by default, so anything inside them
  is unreachable by `npm test` — not weakly covered, *absent*. A console
  command dispatched from the device websocket panicked on every iPhone for
  weeks with 185 FFI tests green, because the two halves of that path live in
  two features the suite never turns on. The fix is not a feature-gated test
  nobody runs: move the **rule** into a module the suite does compile, leave
  only the glue behind the feature, and add the dev-dependency that lets the
  rule be exercised (a `tokio` runtime is cheap; a Minecraft server is not).

Then add the test that would have caught it, and break the fix to prove it does.

---

## Found something this skill got wrong?

Fix it here, in the same commit as the work that revealed it — while you still
remember what was actually confusing. A trap you fell into, a command that did
not behave as described, a step that was missing, an instruction that read two
ways: all of it belongs in this file. The test is whether the next session
avoids the mistake you just made.

If the gap is big enough to be its own skill, say so and offer to write it —
do not create one unasked.

---
name: android-emulator
description: Drive an Android app on an emulator or device from the shell — build, install, tap through the UI by screenshot, read logcat, and verify behaviour end to end. Use when testing Android changes, reproducing a device-only bug, or confirming a fix on a real device rather than in unit tests.
---

# Driving an Android emulator

You can run the whole loop — build, install, tap, read the screen, read the
logs — from the shell, without the user touching the emulator. This skill is
the mechanics of that loop and the traps that cost time.

## First: find your tools and your app

Do not assume paths. Establish these once per session:

```bash
which adb || ls "$ANDROID_HOME/platform-tools/adb"   # often already on PATH
adb devices                                          # nothing attached? see below
```

If `adb` is missing, `ANDROID_HOME` / `ANDROID_SDK_ROOT` point at the SDK;
`adb` lives in `platform-tools/`, `emulator` in `emulator/`.

**Neither variable is set on this Mac, and there is no `java` on PATH.** Both
came from Homebrew rather than Android Studio, which is why nothing is
exported. Gradle needs both spelled out or it fails twice in a row, on two
different messages ("Unable to locate a Java Runtime", then "SDK location not
found"):

```bash
ANDROID_HOME=/opt/homebrew/share/android-commandlinetools \
JAVA_HOME=/opt/homebrew/opt/openjdk@21 \
  ./gradlew :app:compileDebugKotlin
```

`sdkmanager` *is* on PATH, so `readlink -f "$(which sdkmanager)"` finds the SDK
if that path ever moves. That install has `platform-tools/` but **no
`emulator/` package and no AVDs** — there is no emulator on this machine
today, only a real device over USB. `:app:compileDebugKotlin` still type-checks
a change with nothing attached, which is worth doing before you go looking for
hardware.

Starting an emulator, if the project has no script for it:

```bash
"$ANDROID_HOME/emulator/emulator" -list-avds
"$ANDROID_HOME/emulator/emulator" -avd <name> -no-boot-anim &
adb wait-for-device
until [ "$(adb shell getprop sys.boot_completed | tr -d '\r')" = "1" ]; do sleep 2; done
```

Get the app id and launch activity from the project rather than guessing —
`applicationId` (plus any `applicationIdSuffix`, commonly `.debug`) in
`app/build.gradle[.kts]`, and the `LAUNCHER` activity in `AndroidManifest.xml`.
Or ask the device: `adb shell pm list packages | grep <something>`.

## Build and install: prefer the project's own script

Look for a wrapper first — an npm script, Makefile target, or `scripts/*.js`
that builds and installs. Projects wrap gradle for real reasons, and going
around the wrapper silently skips them. Two common ones:

- **JDK selection.** Gradle/AGP support a bounded JDK range. If `JAVA_HOME`
  points at something newer (Android Studio's bundled JBR is typically far
  ahead), gradle fails with an unhelpful message — sometimes a bare version
  number with no explanation. Wrappers usually pick a compatible JDK for you.
- **Native staging.** Projects with Rust/C++ copy freshly built `.so` files
  into `jniLibs` before gradle packages them. Skip that and you debug a new
  APK against a stale library — the symptom is a *runtime* "no such method"
  error, not a build failure.

Falling back to gradle directly:

```bash
./gradlew assembleDebug
adb install -r -t app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n <appId>/<activity>
```

## Before reinstalling: check what is running

Installing kills the running app — and any long-lived work it owns (a server,
a background job, an unsaved session), ungracefully. Check first:

```bash
adb shell "ps -A | grep -E '<appId>|<childProcessName>'"
```

Watch for **child processes** the app spawned, not just the app. Those are the
ones with state worth losing. Stop them through the UI before you install.

## Seeing the screen

```bash
adb shell screencap -p /sdcard/s.png
adb pull /sdcard/s.png ./s.png
```

Then `Read` the pulled file. Pull into your scratchpad directory, not the repo.

**On Windows Git Bash, prefix every `adb` command containing a device-side
absolute path with `MSYS_NO_PATHCONV=1`:**

```bash
MSYS_NO_PATHCONV=1 adb shell screencap -p /sdcard/s.png
MSYS_NO_PATHCONV=1 adb pull /sdcard/s.png ./s.png
```

Git Bash rewrites `/sdcard/s.png` into a Windows path before `adb` sees it, and
`screencap` replies with its *usage message* — which reads like you got the
flags wrong, so the natural response is to fiddle with `-p` and lose two
attempts. It applies to `shell`, `pull`, and `push` alike.

## Tapping

`adb shell input tap <x> <y>` takes **device** pixels, but screenshots are
scaled down before you see them. The Read result states the factor:

```
[Image: original 1080x2400, displayed at 900x2000. Multiply coordinates by 1.20 …]
```

Read coordinates off the displayed image, then **multiply by that factor**.
Using displayed coordinates directly puts every tap up and to the left —
usually still inside *some* control, so it looks like the app misbehaved rather
than like you missed. Confirm real resolution with `adb shell wm size`.

**Screenshot immediately before every tap.** Coordinates go stale faster than
you expect: a list reorders by last-used, an expanded panel pushes everything
down, a modal appears over the thing you were aiming at. A tap computed from a
screenshot taken three actions ago is a guess.

**`input tap` always succeeds.** It reports nothing about what it hit — it is
a touch event at a coordinate, not a click on a control. Confirm the *effect*
(a log line, a changed screenshot) before believing the action happened. Both
failure modes bite:

- A tap you think landed but didn't — you then debug an app that never got the
  input.
- A tap you think missed but landed — the effect just hadn't arrived yet when
  you looked. Give it a beat and re-read before retrying, or you double-fire.

**Don't chain taps on fixed sleeps.** `tap A; sleep 2; tap B` assumes B's
target is where you predicted after A. When A doesn't land as expected, B fires
into whatever is now under those coordinates — often a nav bar, and you end up
on a different screen wondering what happened. Screenshot between steps.

### System surfaces swallow taps aimed at the app

The notification shade, a permission dialog and Recents are drawn by the system
*over* the app. A tap meant for a button in the app lands on whatever the
system is showing — and since `input tap` reports success either way, what you
see is an app that ignored its input.

The shade is the one that bites, because you open it deliberately to read a
notification and then forget it is still down. **Never infer shade state — set
it**, immediately before the tap:

```bash
adb shell cmd statusbar collapse              # before any in-app tap
adb shell cmd statusbar expand-notifications  # before any notification tap
```

Both are idempotent, so running the right one unconditionally costs nothing and
removes the whole class of failure.

An unexpected dialog is the other half: a runtime-permission prompt can appear
mid-flow and eat the next several taps. Look for one rather than theorising
about a stalled app:

```bash
adb shell dumpsys window 2>/dev/null | grep -E "GrantPermissionsActivity|NotificationShade"
```

### Prefer firing the intent over tapping the UI

A notification action, a service command or a deep link is reachable directly,
which skips coordinates, scale factors and shade state entirely:

```bash
adb shell am start-service -n <appId>/<service> -a <ACTION>   # a notification action
adb shell am start -a android.intent.action.VIEW -d "myapp://path"
```

That exercises the handler without proving the `PendingIntent` is attached to
the button — so do both: the intent while iterating on behaviour, one real tap
at the end to prove the wiring. And confirm that tap by **its own log line**,
not by the outcome. An outcome can arrive from somewhere else entirely,
including the user reaching over and doing it by hand while you work; that is a
real way to conclude a control works when you never actually pressed it.

Other input:

```bash
adb shell input text 'hello%stext'          # %s is a space; quote for the shell
adb shell input keyevent KEYCODE_BACK        # also KEYCODE_ENTER, KEYCODE_TAB
adb shell input swipe 540 1600 540 600 300   # scroll up over 300ms
```

## Logs

```bash
adb logcat -c                    # clear first, so you read this run only
adb logcat -d -s MyTag:*         # dump by tag, then exit
adb logcat -d --pid=$(adb shell pidof -s <appId>)   # everything from the app
```

`-d` (dump and exit) rather than a live tail: a tail never returns and blocks
the tool call. Find the project's tags by grepping for `Log.i(`/`Log.w(` or a
`TAG` constant. WebView-based apps are also inspectable at `chrome://inspect`.

## Waiting for slow things

Cold starts, downloads and first-run unpacking take tens of seconds. Do **not**
foreground-sleep. Background a command that exits when the condition is met and
keep working; the completion notification will find you:

```bash
adb logcat -c    # ALWAYS clear first
for _ in $(seq 40); do
  adb logcat -d -s MyTag:* | grep -q -E "<success>|<failure>" && break
  sleep 3
done
adb logcat -d -s MyTag:* | grep -q -E "<success>|<failure>" \
  || echo "TIMED OUT after 2 min — the action probably never landed"
```

**Bound the loop.** An unbounded `until … do sleep 3; done` waiting on
something that will never arrive does not fail, it *hangs* — and a tap that
missed is exactly the case that makes it never arrive. The hang is worse than
the missed tap, because a bounded loop hands you the diagnosis ("the trigger
did not land") while an unbounded one hands you a session that has visibly
stopped doing anything. Cap every wait and say so when it expires.

**`adb logcat -c` before the loop, every time.** Without it the buffer still
holds the *previous* run's matching line, so the loop exits instantly, and
everything you sequenced after it fires far too early — against a screen that
hasn't changed yet. This failure is silent and looks like the app being fast.

Match **both** the success and the failure signatures. A loop that waits only
for success is indistinguishable from a hang when the thing crashes.

Beware timestamps when reading a dump: if the newest line predates the action
you just took, the action hasn't landed *yet* — that is not evidence it failed.

## Verifying end to end

Deciding what counts as proof is the part worth doing deliberately. Before you
start, write down each leg of the flow and the specific line or pixel that
proves it — then check them off:

| Leg | Evidence |
|---|---|
| … | a log line, a UI state, an absent process |

Two rules that generalise:

- **The UI is not evidence of the internals.** A green card can sit on top of
  a failed teardown, a skipped backup, or an exit misjudged as a crash. If the
  code makes a decision you care about, log it and read the log.
- **Wall-clock is evidence.** An operation that should take a second and takes
  thirty succeeded through a timeout and a fallback, not through the path you
  meant to test. Notice the pause; don't just accept the outcome.

Check for orphans afterwards — `adb shell "ps -A | grep <child>"` should come
back empty.

**Verify while the state still exists.** UI panes are conditional: a console
disappears when the server stops, a progress bar when the job finishes. If you
need to read something that only renders in a state, read it *during* that
state — you cannot go back for it.

**Exercise the flow twice.** A first run on a fresh install starts from empty
state and hides every bug about carrying state over: what a second launch
clears, what it wrongly keeps, what it replays. Most of the interesting
defects only appear on run two.

## Diagnosing: probe, don't theorise

When device behaviour contradicts what the code says should happen, the
cheapest move is almost always **one temporary log line**, not another round of
reasoning. A rebuild-install-run cycle is a couple of minutes; three wrong
hypotheses cost more than that and can leave you confidently wrong.

Log the actual value at the boundary you doubt — the FFI reply, the buffer
size, the branch taken:

```kotlin
runCatching { nativeThing(x) }
    .onSuccess { Log.w(TAG, "PROBE reply=$it state=${readState()}") }
    .onFailure { Log.w(TAG, "PROBE failed: ${it.message}", it) }
```

That single probe answered in one run what several rounds of inference had
gotten wrong — and revealed the "missing" line had never been on that path at
all.

Two traps worth naming:

- **Swallowed errors make impossible situations.** A `runCatching { }` with no
  `onFailure` turns a hard failure into a silent no-op, and you end up proving
  to yourself that working code cannot possibly be producing what you observe.
  If a call matters, log its failure — permanently, not just while debugging.
- **"My change broke it" is a hypothesis, not a fact.** Before hunting a
  regression, establish that the thing ever worked: check the code path, or
  re-run against the previous build. A line that was always logcat-only is not
  a console line you deleted.

When you do find the cause, prefer a **test that reproduces it** over a fix you
eyeball. Then break the fix on purpose and confirm the test fails — a test
written after the fact often asserts the behaviour it just watched rather than
the rule you meant, and only deliberately breaking it tells the two apart.

## Triage

**`screencap` prints its usage message.** Missing `MSYS_NO_PATHCONV=1` (Windows).

**Gradle fails with a bare version number or "unsupported class file major
version".** `JAVA_HOME` is too new. Use the project's build wrapper, or point
`JAVA_HOME` at a supported JDK for this AGP version.

**Runtime "no such method"/`UnsatisfiedLinkError` after a native change.** APK
packaged a stale `.so`. Rebuild through the wrapper that stages `jniLibs`.

To check what actually shipped, list the symbols rather than trusting the
build log:

```bash
adb shell getprop ro.product.cpu.abi          # which one the device loads
"$ANDROID_HOME"/ndk/*/toolchains/llvm/prebuilt/*/bin/llvm-nm \
  --dynamic --defined-only path/to/lib.so | grep MySymbol
```

Note the ABI: incremental builds usually rebuild only the one you are running,
so the *other* architecture's library can sit stale for days and pass every
emulator test. It fails the first time someone runs on real hardware — worth
a full rebuild before you claim a native change works on device.

**Taps land on the wrong control.** You skipped the display→device scale
factor, or the layout moved since your last screenshot.

**A wait loop returns instantly and everything after it misfires.** You didn't
`adb logcat -c` first, so it matched the previous run.

**A wait loop never returns and the session looks stalled.** The trigger did
not land — most often a tap absorbed by the notification shade or a permission
dialog. Collapse the shade, screenshot, and check the action fired at all.
Bound the loop so this reads as a timeout rather than as a hang.

**The app did the thing but no log line says you asked for it.** Something
other than your tap caused it. Do not credit the control you were testing —
find the line that proves the path ran, or re-run it.

**Scrolling a pane does nothing.** Swipe *inside* the pane's bounds, not the
page's, and repeat — one swipe moves very little in a long buffer. Swipe
up-to-down to scroll toward the top.

**`adb devices` shows nothing.** Emulator not started, or it died; relaunch and
wait for `sys.boot_completed`. `adb kill-server && adb start-server` clears a
wedged daemon.

**Install fails with `INSTALL_FAILED_UPDATE_INCOMPATIBLE`.** Signed by a
different key than the installed build — `adb uninstall <appId>` first, which
also wipes its data.

**Blank screen in a WebView app.** Web assets missing from the APK; check the
project's asset-packaging step and `chrome://inspect` for the real error.

---

## Found something this skill got wrong?

Fix it here, in the same commit as the work that revealed it — while you still
remember what was actually confusing. A trap you fell into, a command that did
not behave as described, a step that was missing, an instruction that read two
ways: all of it belongs in this file. The test is whether the next session
avoids the mistake you just made.

If the gap is big enough to be its own skill, say so and offer to write it —
do not create one unasked.

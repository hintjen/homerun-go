---
name: android-emulator
description: Drive an Android app on an emulator or device from the shell — build, install, tap through the UI by screenshot, read logcat, and verify behaviour end to end. Use when testing Android changes, reproducing a device-only bug, or confirming a fix on a real device rather than in unit tests.
---

# Driving an Android emulator or phone

You can run the whole loop — build, install, tap, read the screen, read the
logs — from the shell, without the user touching the device. This skill is
the mechanics of that loop and the traps that cost time. It applies to a real
phone as much as an emulator; where they differ, the phone is called out.

## First: find your tools and your app

Do not assume paths. Establish these once per session:

```bash
npm run doctor                     # says what this machine has, per target
adb devices                        # nothing attached? see below
```

**Run `npm run doctor` first and believe it over anything written here.** This
skill has been written from more than one machine, and the toolchain paths are
the part that does not travel. `ANDROID_HOME` / `ANDROID_SDK_ROOT` point at the
SDK; `adb` lives in `platform-tools/`, `emulator` in `emulator/`.

Two setups have been used so far and they agree on almost nothing:

| | macOS (Homebrew) | Windows |
|---|---|---|
| `ANDROID_HOME` | unset — `/opt/homebrew/share/android-commandlinetools` | set — `%LOCALAPPDATA%\Android\Sdk` |
| `JAVA_HOME` | unset — `/opt/homebrew/opt/openjdk@21` | set |
| AVD `homerun_api35` | arm64-v8a | **x86_64** |
| Real phone | wireless debugging | USB |
| `adb` | `adb` | `adb.exe` |

On the Mac neither variable is exported, and Gradle fails twice in a row on two
different messages ("Unable to locate a Java Runtime", then "SDK location not
found") until both are spelled out on the command line. On Windows both are
already set and Gradle needs nothing.

**The AVD's ABI decides which Rust target you must build**, and it differs
between the two machines. Build the one the AVD actually reports
(`getprop ro.product.cpu.abi`), not the one you remember. On Apple Silicon the
arm64 image runs the same slice the phone does, so proving a change there
covers the shipping ABI; the Windows emulator is **x86_64, so it never
exercises the arm64 slice at all** and a change proven on it has skipped what
ships.

`:app:compileDebugKotlin` type-checks a change with nothing attached, which is
worth doing before you go looking for hardware.

### Scope every `adb` command to one device

More than one thing can be attached, and **one of them may belong to someone
else** — another agent session, or the user's own phone mid-task. `adb` with no
`-s` chooses for you and complains only when it cannot decide, so a stray
`adb install` or `adb shell am force-stop` lands on whichever device it liked.
That is somebody else's work interrupted, with nothing in your output saying
so.

Establish the serial once, then use it everywhere:

```bash
ADB="$ANDROID_HOME/platform-tools/adb"        # adb.exe on Windows
"$ADB" devices                                 # look at ALL of them first
EMU=$("$ADB" devices | grep '^emulator-' | head -1 | cut -f1)
"$ADB" -s "$EMU" shell ...
```

A freshly launched emulator shows as `offline` before `device`; poll
`getprop sys.boot_completed` for `1` rather than sleeping a fixed time. If a
device you did not start is listed, leave it alone and say so rather than
assuming it is yours to reset.

### Working in a git worktree

A worktree has none of the repo's untracked build inputs, and Gradle fails on
each in turn rather than on all of them at once. Copy them from the main
checkout before the first build:

- `*-google-services.json` — **configuration** fails, not the build:
  "Property '$1' specifies file … which doesn't exist".
- `android/app/src/main/assets/web` — the UI bundle.
- `android/app/src/main/assets/jre-*` — without it `JavaRuntime.isAvailable`
  is false and the host reports one fewer engine than the build actually has,
  which reads as a routing bug rather than a missing file.
- `android/app/src/main/jniLibs/<abi>/*.so` — for anything you are not
  rebuilding yourself.

The real phone is reached over **wireless debugging, not USB** on the Mac — see
the next section.

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

## A physical device over wireless debugging

USB is not the only way in, and on this Mac it is not the way that works —
plugging the phone in enumerates nothing at all. Wireless debugging is how
hardware is actually driven here.

**Pairing takes two steps against two different ports.** The pairing port
exists only while the phone's dialog is open and changes every time; the
connect port is stable. Discover both rather than asking for them:

```bash
adb mdns services
# adb-ZT422CJNT8-VE5DJ6  _adb-tls-pairing._tcp  10.0.0.23:42923  <- only while the dialog is up
# adb-ZT422CJNT8-VE5DJ6  _adb-tls-connect._tcp  10.0.0.23:34557  <- stable
adb pair    10.0.0.23:42923 <6-digit-code>
adb connect 10.0.0.23:34557
```

The pairing service advertises only while **Developer options → Wireless
debugging → Pair device with pairing code** is on screen. The six-digit code is
the one thing that has to come from the user; `adb mdns services` supplies
everything else, so do not make them read out an address.

**The device then appears twice** — once as `<ip>:<port>` and once under its
mDNS name — and anything that shells out to `adb` dies on `more than one
device/emulator`. `adb disconnect` does not stick; mDNS re-adds it within
seconds. Pin the serial instead, in every command and every wrapper script:

```bash
export ANDROID_SERIAL=10.0.0.23:34557
```

That failure is quiet and costs a full cycle: `gradle assembleDebug` succeeds,
and only the install at the very end fails, so it reads as a build problem.

Wireless debugging also drops on reboot and on network changes, and pairing
does not survive it — expect to redo the code dance rather than assuming the
phone died.

### Plugged in, and `adb devices` is still empty

Settle whether the Mac sees the hardware at all before touching Android
settings. A phone with USB debugging *off* still enumerates as an MTP or
charging device, so nothing enumerating means the cable or the port, not the OS:

```bash
ioreg -r -c IOUSBHostDevice -l | grep -i '"USB Product Name"'
```

Use that class, **not `ioreg -p IOUSB`** — the `IOUSB` plane is legacy and
returns an empty tree on Apple Silicon no matter what is attached, which reads
exactly like a missing phone. `system_profiler SPUSBDataType` deserves a control
run before you trust its silence too (`SPHardwareDataType` should print ~17
lines; if it does, the tool is not being blocked). A charge-only cable is the
usual answer and is visually identical to a data one.

If enumeration is fine but Developer options is missing, **Build number is
usually nested rather than removed** — Samsung hides it under About phone →
Software information, LG and TCL under Software info, and some OEMs label it
Version or Build version. Carrier-locked prepaid hardware does sometimes ship
without it, but that is rarer than the menu being one level down, and a reboot
has been enough to make it appear. Do not reach for wireless debugging as the
workaround: it is itself a Developer options toggle.

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

### Installing your UI change is not the same as running it

**A downloaded OTA bundle outranks the one in the APK**, so a freshly built,
freshly installed app can serve web assets from days ago and your change is
simply absent from the screen. `BundleStore` serves `files/ui/current` in
preference to `assets/web`, deliberately — its docstring explains that a
bundle which cannot be overridden by the binary is the whole point, because
otherwise a fatal bundle would brick the app in a way no store update could
fix.

This reads exactly like a build problem, and it will send you to check the
staging path, the APK contents and the branch you built from — all of which
are fine. Ask the device instead. One line settles it:

```bash
adb logcat -d -s HomerunBundle:* | tail -3
#  serving bundle 2026-08-13.2   <- an OTA bundle; your change is NOT running
#  serving the shipped bundle    <- the APK's assets; your change IS running
```

Drop back to the assets floor, which is never deleted:

```bash
adb shell "run-as app.gethomerun.mobile.debug rm -rf files/ui/current"
```

`docs/ota-bundles.md` documents pushing to `files/ui/pending` with a MANIFEST
instead, which is the faithful path for testing the updater itself. For merely
iterating on a local UI build it is the wrong tool — a hand-staged bundle gets
serial `0` and any real release outranks it.

**A *pending* bundle is the same trap one launch later.** The updater
downloads into `files/ui/pending` silently while you test, and the *next*
cold start activates it — so the launch you do to test a cold-start flow is
exactly the launch that swaps your build out. If the app shows an "Update
Available" banner, a pending bundle already exists; delete `files/ui/pending`
along with `current` before any test that restarts the app.

Confirm what actually shipped in the APK rather than trusting the staging log,
too — the assets are just a zip entry:

```bash
unzip -p app-debug.apk assets/web/_next/static/chunks/<chunk>.js | grep -c <marker>
```

### Two builds, one scheme, one name

`app.gethomerun.mobile` and `app.gethomerun.mobile.debug` can both be
installed, and both declare the `homerun://` intent filter. A deep link then
raises an **"Open with" chooser listing two entries both labelled "Homerun
Go"** — indistinguishable, so picking is a coin flip, and the wrong choice
hands the callback to an app with no pending session, which drops it in
silence. Check before driving any deep-link flow:

```bash
adb shell "cmd package query-activities -a android.intent.action.VIEW \
  -d 'homerun://auth/callback'" | grep packageName= | sort -u
```

`pm disable-user --user 0 app.gethomerun.mobile` removes the ambiguity without
touching that install's data; `pm enable` restores it. Prefer it to
`adb uninstall`, which wipes everything the other build owned.

## Seeing the screen

```bash
adb shell screencap -p /sdcard/s.png
adb pull /sdcard/s.png ./s.png
```

Then `Read` the pulled file. Pull into your scratchpad directory, not the repo.

**The phone may be in someone's hand.** The physical device is the user's, and
they use it while you work — a screenshot can come back showing the home
screen, the app drawer, or Settings, because they navigated away between your
`am start` and your `screencap`. Read what the screenshot actually shows before
reasoning about it; "the app looks wrong" is sometimes "the app is not on
screen". When that happens, `am start` again rather than concluding anything,
and prefer not to drive the phone at all while the user is plainly on it.

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

### Something that is only on screen for a moment

A splash, a launch animation, a toast, a transition — one screenshot will not
catch it, and screenshot-then-pull is far too slow to catch it repeatedly:
each round trip is most of a second, which is the entire thing you are trying
to see. Keep the loop **on the device** and pull afterwards:

```bash
adb shell 'rm -f /sdcard/f*.png; am force-stop <appId>'
adb shell 'am start -n <appId>/<activity> >/dev/null 2>&1
           for i in 1 2 3 4 5 6 7 8 9 10 11 12; do screencap -p /sdcard/f$i.png; done'
adb pull /sdcard/ <scratchpad>/frames
```

On-device `screencap` runs about four frames a second, so a dozen covers ~3s —
enough for a cold start. Note `am start` and the loop are in **one** `adb shell`
so nothing waits on the host between them, and `force-stop` is in a separate
call because it must complete first.

Then contact-sheet the frames into a single image and `Read` that, rather than
reading twelve screenshots: `Image.paste` in a 6×2 grid with the filename drawn
above each cell tells you which frame showed what. `screenrecord` is the obvious
alternative and is worse here — extracting frames from its mp4 needs ffmpeg,
which is not installed on this Mac.

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
wedged daemon. For a physical device, it usually means unpaired rather than
absent — `adb mdns services` and pair, per the wireless-debugging section.

**A command fails with `more than one device/emulator` for one phone.** It is
attached over both its IP and its mDNS name. `export ANDROID_SERIAL=<ip>:<port>`
— disconnecting the duplicate does not hold.

**A build reports success but the log ends in a stack trace.** Piping to `tail`
replaces the command's exit status with `tail`'s, which is always `0`. Anything
run in the background is reported by that status, so a failed build reads as a
passing one. Capture the real code before the pipe:

```bash
npm run build:android > build.log 2>&1; echo "EXIT=$?"; tail -20 build.log
```

**Install fails with `INSTALL_FAILED_UPDATE_INCOMPATIBLE`.** Signed by a
different key than the installed build — `adb uninstall <appId>` first, which
also wipes its data.

**A UI change is missing from the screen after a clean build and install.** An
OTA bundle in `files/ui/current` is outranking the APK. `adb logcat -d -s
HomerunBundle:*` says which is being served; delete `current` to fall back.

**A deep link raises an "Open with" chooser with two identical entries.** Both
the release and debug builds are installed and both claim the scheme.
`pm disable-user --user 0 <releaseAppId>`.

**An FCM push never arrives after `am force-stop`.** Force-stop puts the app
in Android's *stopped state*, and the platform refuses to deliver FCM to
stopped apps — the send returns 200 and nothing ever reaches the device, which
reads exactly like a broken token. A dead-but-deliverable app is one that was
launched and then killed gently: `am start`, HOME, then `am kill <appId>`
(or swipe it from Recents). Force-stop is for freezing the UI, not for
simulating "the app is not running".

**A browser-based flow stalls on a fresh emulator.** Chrome's first-run
onboarding ("Welcome to Chrome", then a notifications dialog) sits in front of
the Custom Tab and eats taps. Clear it once per AVD.

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

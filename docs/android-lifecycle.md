# Android lifecycle — hosting while the app is away

## Overview

A phone is not a server. Android's model is that an app which is not in front
of the user is a candidate for reclamation, and it acts on that aggressively —
which is fatal to a product whose whole promise is "your friends can play on
your phone". A player who answers a text message must not disconnect everyone.

Two things have to survive being backgrounded, and the second is the one that is
easy to forget:

- **the server**, for as long as the player wants it up
- **the on-stop backup**, which runs for *minutes after* the server has
  stopped. It is uploading the session that is not yet in the repository, so a
  killed backup does not lose a server — it loses the play.

The mechanism is a foreground service. There is no other one: Android offers no
way to ask for "please do not reclaim me" without also telling the user, and the
notification is the price the platform charges. It is not decoration — it is
also the only control a player has while the app is not open.

### What is not required

**There is no "allow this app to run in the background" permission on Android.**
`FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_SPECIAL_USE` and `WAKE_LOCK` are
install-time permissions: declared in the manifest, granted by being declared,
no dialog. What reviews whether the use is legitimate is Google Play, not the
OS — see [Play policy](#play-policy-the-open-item-and-how-to-close-it).

`POST_NOTIFICATIONS` (API 33+) *does* prompt, and gates only the notification's
**visibility**. Denied, the service still runs at foreground importance and the
server still hosts; the player just gets no indicator and no Stop button. It is
requested when the first server starts rather than at launch, so the ask arrives
with a visible reason.

A battery-optimisation exemption (`REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`) is the
prompt people usually mean by "run in the background". It is **not** implemented
and is not needed for a foreground service. It becomes worth considering only if
measurement on OEM builds with aggressive process killers shows sessions dying
anyway, and it carries its own Play-policy restrictions.

## `HostingService` — `android/.../HostingService.kt`

A started (never bound) foreground service in the main app process. Three jobs:

1. **Priority.** Running it raises the process to foreground importance. That is
   the entire point; the notification is a consequence.
2. **The notification**, kept current with the server's state and player count,
   carrying a Stop action and an intent back into the app.
3. **The wake lock.** Separate from job 1 and not implied by it — see below.

It decides nothing. `ServerHost` decides when hosting is happening and starts
and stops the service accordingly; this class renders that state. The split
exists because "is this device still busy" has a non-obvious answer and should
live in one place.

`START_NOT_STICKY`, deliberately: the JVM is a child of this process, so if the
process is killed the server died with it. A service Android restarted by itself
would come up showing a notification for a server that is not running and
holding a wake lock for nothing.

### Why the wake lock is not redundant

A foreground service stops the *process* being reclaimed. It does not stop the
*CPU* suspending once the screen goes off. A suspended CPU is a server that has
stopped ticking: clients time out, and the world stops saving. A
`PARTIAL_WAKE_LOCK` held for the length of the session is what a player is
actually asking for when they put the phone down and keep hosting.

It is taken without a timeout, which lint dislikes and is right here — the
lock's lifetime is the service's, the service's lifetime is the session's, and
both ends are explicit. A timeout would end a session mid-game.

### Swiping the app away does not stop the server

`stopWithTask` is left at its default of false. A player who swipes Homerun Go
out of Recents has not asked their friends to be disconnected, and the
notification's Stop is right there when they do want that. Hosting outliving the
task is the same bargain a music player makes. `onTaskRemoved` logs and does
nothing.

## `ServerHost` — who decides that hosting is happening

`ServerHost.Hosting` is the answer, and its `busy` is the load-bearing part:

```
busy = starting || backingUp || state ∈ { STARTING, RUNNING, STOPPING }
```

Three terms, and two of them are there because the obvious version is wrong.

**`backingUp`** — this is why `busy` is not `runningServerIds.isNotEmpty()`. The
backup runs after the state is already `stopped`. `onStateChanged`'s third
argument says a backup is *starting*, on a state change that says the server has
*stopped*; nothing else afterwards marks the device idle, which is why
[`ServerBackend.onBackupFinished`](#serverbackendonbackupfinished) had to be
added. It fires from `invokeOnCompletion`, so a cancelled backup announces
itself too — a cancellation that stayed silent would pin the process in the
foreground for the life of the process.

**`starting`** — the first stretch of a launch happens before the backend
announces anything. The settings lookup and the backup-lease check are both
network round-trips made with nothing spawned, and a process still merely cached
can be reclaimed during them. So the bridge's start handler calls
`hostingRequested` before those, and it is **paired with `hostingSettled` in
that branch's own `finally`** — not the handler's outer one, which also runs for
the `alreadyRunning` refusal a reconcile-loop start receives when it races the
user's. Settling there would stand the service down underneath a live launch.

The failure `hostingSettled` exists for: a launch refused by the lease, or by a
game type this host cannot run, throws *before* any state is announced. There is
no `stopped` coming, so without it the notification would describe a server that
never started, for as long as the process lived.

### `ServerHost.stop` is the single stop

The notification's Stop and the UI's Stop are the same call. What must not be
re-derived is `graceful`: it is `homerun-core::lifecycle`'s verdict about
whether the engine has a console that can hear `stop` and a world worth saving,
not a preference. A second implementation that guessed it would terminate a JVM
mid-save. `BridgeRouter`'s `native-server-stop` handler is now a thin wrapper
over this.

### `ServerBackend.onBackupFinished`

Added for this milestone. `PumpkinBackend` never invokes it — that backend runs
no on-stop backup, so a stop there really does mean the device is idle.

## Notification icons — `res/drawable-*/ic_notification.png`

Android draws a small icon from its **alpha channel only**, tinted flat. The
launcher icon is an adaptive icon whose background layer is an opaque square, so
passing `applicationInfo.icon` renders a solid blob — observed as a featureless
ring, indistinguishable from every other app's fallback.

So a notification icon is a different asset with different rules: transparent
except the mark, no background layer, 24dp with no safe zone, and the mark
nearly filling the canvas because it is drawn at status-bar size. Both the
hosting notification and the bridge's `push-notification` use it.

It and the launcher icon are generated from the one brand master by
`scripts/generate-icons.py` — see [android-host.md](./android-host.md#the-icons)
for what that does and why the two assets come out different sizes.

`app_name` is **"Homerun Go"** — what the shared UI's own header says. It is the
launcher label, the notification header, and the hosting notification's title
before a server name is known.

## The resume resync

`BridgeRouter.resyncServerState()` runs on `MainActivity.onResume` as well as on
page-ready. The WebView usually survives being backgrounded and receives events
normally, but "usually" is doing real work now that a server keeps running while
the app is away: the render process is the first thing Android reclaims, and a
player returning to a stopped card for a server their friends are on is worth
one event to rule out.

## Play policy: the open item, and how to close it

The service declares `foregroundServiceType="specialUse"`. **Google reviews that
type by hand, and has not reviewed ours.** It is open question 1 in
`plans/android.md`, and the plan's own advice was to submit to an internal track
at M3 to find out early. M3 is done.

### The risk is lower than the plan assumed

**Anvil-MC** (`com.armmc.app`) ships on Google Play doing the same thing —
hosting a Minecraft server on a phone, with Spigot, Paper, Fabric, NeoForge and
custom jars — and its reported permissions include
`FOREGROUND_SERVICE_SPECIAL_USE`, `WAKE_LOCK`, `POST_NOTIFICATIONS` and
`REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`.

That is a precedent for two separate policy questions at once: this foreground
service type for this purpose, and downloading server jars at runtime — the
other item on `plans/android.md`'s M5 list.

Treat it as strong evidence rather than proof: it comes from a permissions
listing rather than from the manifest itself. **Confirm it in thirty seconds**
on any device — Play listing → About this app → App permissions → See more.

### The criterion the declaration has to meet

Google's own wording:

> Google Play will **likely reject** apps using `specialUse` if another
> foreground service type is appropriate for the use case.

So the declaration is not "describe the feature", it is "show that nothing else
fits". The manifest's `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` is written to answer
that and kept to **one line** — XML normalises newlines inside an attribute
value into runs of spaces, and a reviewer reads the value verbatim.

### What the submission needs

Play Console → **Policy → App content → Foreground service types**, per type:

| Field | Ours |
|---|---|
| What the feature does | Runs a Minecraft server other players connect to, for as long as the player keeps it online, and finishes uploading the world to their backup repository afterwards. |
| **Impact if deferred or interrupted** | Every connected player is disconnected mid-game and unsaved play is lost. If interrupted during the post-stop upload, the session that just finished never reaches the backup — so the loss is the play itself, not a restartable transfer. |
| **Demo video** | Required. See below. |

The video is the part nobody expects. Ours is short: open the app → **Start** →
the ongoing notification appears with the server name and player count → leave
the app → return and show the server still running → **Stop** from the
notification → the notification reports the backup, then disappears.

Review takes up to seven days, sometimes longer.

### If it is refused

The fallback is `dataSync`, and its cost is real but smaller than it first
looks. Android 15 permits `dataSync` six hours per 24, then calls
`Service.onTimeout()` — after which there are seconds to `stopSelf()` or the
system throws `RemoteServiceException`. **Bringing the app to the foreground
resets the timer**, so a player who opens Homerun Go during a session rarely hits
it; a phone hosting untouched in a pocket all evening does.

So a refusal degrades the product rather than blocking it — which is worth
knowing before deciding how much to stake on the submission.

## Verified, and not

Verified on an API 35 x86_64 emulator, JVM backend, real gateway tunnel:

| Leg | Evidence |
|---|---|
| Service comes up as a foreground service | `dumpsys activity services`: `isForeground=true foregroundId=1 types=0x40000000` (`SPECIAL_USE`) |
| Process is protected while backgrounded | `dumpsys activity processes`: `fg +50 F/S/FGS`, `curProcState=4`, with the launcher in front |
| The JVM survives backgrounding | child `libjavabin.so` alive at 629 MB RSS with the app at HOME |
| Wake lock held for the session | `dumpsys power`: `PARTIAL_WAKE_LOCK 'homerun:hosting' … LONG` |
| The backup outlives the server | `server exited (code 0, intentional=true)` → `[Backup] Backing up the world…` → `[Backup] Backup complete.`, service up throughout |
| Service stands down afterwards | no `HostingService` record, wake lock released, no orphan `libjavabin.so` |
| Notification renders and Stop works | `Stop tapped in the notification for <id>` followed by a clean graceful exit |

**Not verified:** anything on arm64 or real hardware, Doze with the screen
genuinely off for hours, a multi-hour session, or behaviour on an OEM build with
its own process killer. The emulator does not Doze the way a Pixel does and
never runs out of memory the way a mid-range phone does.

## File map

| File | Role |
|---|---|
| `HostingService.kt` | the foreground service, its notification and the wake lock |
| `ServerHost.kt` | decides `busy`, starts/stops the service, owns the one `stop` |
| `ServerBackend.kt` | `onBackupFinished`, the signal that the device is finally idle |
| `JavaServerBackend.kt` | fires it from the backup job's `invokeOnCompletion` |
| `BridgeRouter.kt` | `hostingRequested`/`hostingSettled` around the start call |
| `MainActivity.kt` | asks for `POST_NOTIFICATIONS` on first host; resume resync |
| `AndroidManifest.xml` | the permissions, and the `specialUse` declaration |
| `res/drawable-*/ic_notification.png` | the monochrome small icon, one per density |
| `res/values/strings.xml` | notification copy, written for a player |

## Triage

**Server dies a minute or two after backgrounding the app.** The service is not
running. Check `dumpsys activity services <appId>` for `isForeground=true`; if
there is no record, `ServerHost.busy` came out false — most likely a state
transition the core vetoed, or a `hostingRequested` with no announcement after
it.

**Hosting works but there is no notification at all.** `POST_NOTIFICATIONS` is
denied. Note the sharp edge: a service that entered the foreground *while* it
was denied keeps its notification attached and **unposted**, and granting the
permission afterwards does not retroactively post it. Only entering the
foreground again does — that is what `ServerHost.refreshHosting()` is for, and
without it the first hosting session on a fresh install has no visible
indicator. `dumpsys activity services` will say `isForeground=true` while
`dumpsys notification` has no record of it, which is the fingerprint.

**Notification icon is a featureless circle or square.** `setSmallIcon` was
given the launcher icon. Small icons are alpha-only; use
`R.drawable.ic_notification`.

**Notification says "Starting…" for ever, no server.** A launch failed before
the backend announced a state and `hostingSettled` did not run. Check that the
start handler's `finally` is inside the `else ->` branch.

**Notification stays after the server stops.** `onBackupFinished` never fired.
It is wired to `invokeOnCompletion`, so it should fire even on cancellation;
if a backend gained a backup path without that call, this is the symptom.

**Server goes unresponsive whenever the screen is off, then recovers.** The wake
lock was not acquired — `HostingService` logs "no wake lock" when that happens.
It looks exactly like a network fault and is not one.

**`ForegroundServiceDidNotStartInTime` crash.** `startForeground` must be called
within five seconds of `startForegroundService`. It is the first statement in
`onStartCommand` for that reason; nothing may be added above it that can throw
or return early.

**Play rejects the build over the service type.** See
[Play policy](#play-policy-the-open-item-and-how-to-close-it). Expect this to need a Console
justification before it needs a code change.

# Remote push on Android

## Overview

How a message sent by the API lands in this phone's tray, and what the host
does and — more importantly — does not own. Firebase Cloud Messaging, behind
the `remotePush` capability, bridge host revision 9.

The split is the whole architecture: **the host owns what must be native**
— the OS permission and the token Firebase mints — and nothing else. The
shared UI registers that token with the API over the user's own JWT
(`POST /api/push/devices/`, `lib/push.ts` in `homerun-app-ui`) and deletes it
on logout, the same host/UI division as social sign-in. No user identity
passes through the host's push path, which is why nothing in it can leak one.

Delivery is mostly not our code either. A message to a *backgrounded* app is
drawn by the system tray from the message's `notification` block — the
process may not even be running — on the `homerun` channel, the same one the
local `push-notification` bridge channel posts on, so the user has exactly
one mute switch for Homerun alerts. A *foregrounded* app receives the message
in code and **shows nothing, deliberately**: the app being open is the
notification surface — the bell and the server card already say it — and a
system banner over a page saying the same thing is noise. "Foreground" means
our activity is actually visible; a minimized app takes the tray path.

The API half is `homerun/docs/push-notifications.md`; the plan and milestones
are [`../plans/push-notifications.md`](../plans/push-notifications.md).

## `PushMessaging.kt`

`PushMessaging` (object) holds the two references the rest of the host needs:
the current router, set and cleared by `MainActivity` with activity lifetime,
and the last token FCM minted. `currentToken()` asks Firebase and resolves
**null rather than throwing** when there is none — an emulator without Play
services stays null forever, and the contract calls that a state, not an
error.

`PushMessagingService` is FCM's entry point. `onNewToken` re-emits as
`push:token-changed`; the UI re-upserts to the API on every firing, because a
rotation the API never hears about is a phone that silently stops receiving.
`onMessageReceived` — foreground only — logs receipt and suppresses the
banner (see Overview). The receipt log is load-bearing twice over: with no
banner it is the only evidence a foreground delivery happened at all, and
"delivered but suppressed" must stay distinguishable from "never delivered".
An earlier version of that line is also what located a dropped permission
grant in minutes after theories had failed.

## The channel, and why `HomerunApplication` creates it

A notification naming a channel that does not exist is **silently dropped**
— and a background push is drawn by the tray before any code in this app has
run. So the `homerun` channel is created at process start
(`BridgeRouter.ensureNotificationChannel`), and the channel id plus the
monochrome `ic_notification` are also declared as manifest `meta-data`
(`com.google.firebase.messaging.default_notification_*`), because the tray
needs both with zero app code involved. The API sender names the channel per
message too; the meta-data is the fallback for anything that does not.

## The bridge handlers — `BridgeRouter.kt`

Three invokes, capability tier `remotePush`:

- `push:permission` — the OS state in the contract's vocabulary. Below API 33
  there is no runtime permission, so only `granted`/`denied` exist. From 33,
  "off with no recorded ask" is `notDetermined` — the one state where a
  prompt is worth showing. "Was it ever asked" is a prefs flag
  (`push-permission-asked`), written for **either** feature's prompt (hosting
  asks for the same permission), because the OS offers no reliable answer and
  a wrong `notDetermined` makes the UI offer a sheet the OS silently
  swallows.
- `push:request-permission` — suspends on the real sheet via `MainActivity`'s
  launcher, no timeout, per the protocol rule. A permission already decided
  resolves immediately with the truth: Android 13+ cannot re-prompt after a
  denial any more than iOS can.
- `push:get-token` — the token or null, null being a state.

`postNotification` lives in the router's companion so the FCM service and the
local `push-notification` channel share one code path — one icon, one
channel, one silence-on-denied rule, whoever the author is.

## The tap — `MainActivity.kt`

FCM stamps `google.message_id` on the launcher intent it fires for a tray tap
and copies the message's `data` keys in as extras; that stamp is the
discriminator, because everything else about the intent looks like an
ordinary launch. `deliverPushTap` emits `push:opened` with `href`/`id`
**through the router's ready-handshake queue** — a cold-start tap arrives
long before the page exists, the same shape as the cold-start deep link. The
`href` is `UserNotification.href`, and the shared UI routes it with the same
`linkTarget` vocabulary as a bell click.

## `google-services.json` — staged by backend, not build type

`stageGoogleServices` (app/build.gradle.kts) copies
`<repo-root>/{staging|prod}-android-google-services.json` into place
following `-PapiUrl`, exactly as the API URL itself does. The pairing that
matters is **app Firebase project ↔ backend FCM credential**: a debug build
against the production API with a staging file mints tokens the prod backend
can never send to, and the failure (`SENDER_ID_MISMATCH`) happens at *send*
time, far from the mistake. Each Firebase project registers both package
names — `app.gethomerun.mobile` and `.debug` — so one file covers both build
types. `docs/building.md` § *Push credentials* has the full flow.

## File map

| File | Role |
|---|---|
| `PushMessaging.kt` | FCM service, token cache, receipt logging |
| `BridgeRouter.kt` | the three `push:*` handlers, `postNotification`, the channel |
| `MainActivity.kt` | permission sheet plumbing, tap → `push:opened` |
| `HomerunApplication.kt` | channel creation at process start |
| `AndroidManifest.xml` | service registration, default channel/icon meta-data |
| `app/build.gradle.kts` | `stageGoogleServices`, the firebase-messaging dependency |

## Triage

**A send succeeds and nothing appears, no log lines at all.** Either the app
was backgrounded and the permission is missing — the tray path needs
`POST_NOTIFICATIONS` on 13+; check `dumpsys package … | grep
POST_NOTIFICATIONS` (reinstalls can drop the grant on an emulator, which is
how this was first diagnosed) — or delivery never happened at all (dead
token, force-stopped app).

**A send succeeds, `HomerunPush: message received … banner suppressed`
logs, no banner.** Working as designed: the app was foregrounded, and a
foregrounded app shows no banner — the page is the surface. If a banner was
expected, the app was open; background the app and resend.

**Nothing arrives after `am force-stop`.** Force-stop puts the app in the
*stopped state* and the platform refuses FCM delivery to it — the send still
returns 200. A dead-but-deliverable app was launched and then killed gently
(`am kill`, or swiped from Recents). See the `android-emulator` skill.

**Token is always null.** No Play services — a plain AOSP emulator image.
The AVD must be built on a `google_apis` image.

**The API deleted the row after one send.** Not a bug: FCM answered
UNREGISTERED (app uninstalled or token rotated away un-re-registered) and the
sender's hygiene removed it. The next app launch re-registers.

**A tap opens the app but no navigation happens.** The message carried no
`href` in `data` — a tap with no link just opens the app, which it did. Or
the page swallowed it: `push:opened` rides the event queue, so check the
`HomerunHost: notification tap:` line fired and what payload it shows.

**`No matching client found for package name`** at build time — the
`.debug` package is missing from that Firebase project; register it and
re-download the JSON.

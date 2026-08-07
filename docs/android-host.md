# The Android host

One WebView running the shared UI, one bridge router behind it. Everything
the user sees comes from `homerun-app-ui`; this repo supplies the platform.

Source: `android/`.

## Overview

The app has no Android UI. `MainActivity` creates a `WebView`, serves the
shared bundle to it over a virtual `https://` origin, injects the host's
capabilities before any page script runs, and hands every `bridge/v1` message
to `BridgeRouter`. That is the entire shell.

The value is in three details that fail silently when they are wrong: how the
bundle is served, when capabilities land, and which thread the bridge runs on.

## Layout

| File | Role |
|---|---|
| `MainActivity.kt` | The shell. Owns the WebView, the asset loader, and page lifecycle. |
| `BridgeRouter.kt` | `bridge/v1` transport plus the dispatch table. |
| `WebBundle.kt` | Serves `assets/web/` over the asset loader's origin. |
| `HostCapabilities.kt` | The Android capability profile, mirroring the UI's constant. |
| `ServerBackend.kt` | The engine interface. Not yet implemented. |
| `HomerunApplication.kt` | Process-wide WebView setup. |

## Serving the bundle — `WebBundle.kt`

The UI is served from `https://appassets.androidplatform.net/`, a domain
reserved by androidx that never resolves in public DNS.

**Not `file://`.** The bundle's scripts are treated as cross-origin from an
opaque `file://` origin. They fail without an error and you get a blank page
with a clean logcat — the single most expensive way to lose an afternoon on
this stack. The `https` origin also gives us working `localStorage` and
`fetch` to the backend for free.

The path handler adds one rule on top of `AssetsPathHandler`: an extensionless
path that misses is retried with `.html` appended. Next.js static export
writes `/dashboard` as `dashboard.html`. Client-side navigation never requests
those — the router pushes history without a fetch — but a reload, a restored
session, or a deep link does. Paths that already have a suffix are not
retried, so a genuinely missing file still reads as missing.

### The asset filter that eats the whole app

`aapt`'s default asset-ignore list contains `<dir>_*`: **every directory whose
name starts with an underscore is dropped from the APK.** Next.js puts the
entire application bundle in `_next/`.

The default therefore produces a build that succeeds, an APK containing the
HTML and none of the JavaScript, and a console full of
`Uncaught SyntaxError: Unexpected token '<'` — the scripts 404 into the SPA's
HTML fallback and the browser tries to parse it as JavaScript.

`app/build.gradle.kts` overrides `ignoreAssetsPatterns` with the aapt default
minus that one entry, and the asset-merge task asserts afterwards that
`web/_next` survived. Do not remove either. This failure is invisible at build
time and reads as a UI bug.

## Capability injection — `MainActivity.bootstrapScript`

The UI resolves capabilities **once, synchronously, at startup**. It cannot
await them (PROTOCOL.md §4.1), so they must exist before the first line of
page script executes.

We use `WebViewCompat.addDocumentStartJavaScript`, which is the real
guarantee: the script runs before any document script, on every navigation,
scoped to the bundle's origin. `onPageStarted` — what most WebView apps use —
only happens to run early; it is a weaker promise and it is used here solely
as a fallback for WebView builds without `DOCUMENT_START_SCRIPT`, with a
warning logged so a blank screen on an old device has an obvious first
suspect.

The script does two things:

```js
window.__homerunCapabilities = { /* every HostCapabilities field */ };
window.__homerunHost.postMessage = (json) => HomerunHost.postMessage(json);
```

The second line is not optional and is easy to miss. `addJavascriptInterface`
publishes the object as `window.HomerunHost`, but the shared transport
detects a host by looking for `window.__homerunHost.postMessage`. Without the
adapter, `getBridge()` finds no host and **throws** — the app renders nothing
at all rather than degrading.

`HostCapabilities.ANDROID` must stay identical to
`ANDROID_PREVIEW_CAPABILITIES` in the UI repo. That constant is what the
conformance manifest was generated from, so drift means the UI calls channels
this host never implements. The values describe the *platform*, not today's
progress: `moddedServers` is true because Android can run a JVM, even while
the backend that would serve it is still being built.

## The bridge — `BridgeRouter.kt`

Implements `shared/conformance/PROTOCOL.md` §3.3. The wire format and the
handshake are the spec's; what is Android-specific is the threading.

### Thread discipline

`postMessage` is reached through `addJavascriptInterface`, so **it runs on a
binder thread** — not the main thread, and not one we own.

| Stage | Thread | Why |
|---|---|---|
| Parse the envelope | binder | Cheap, touches no shared state. |
| Ready flag, event queue | main | Single-threaded by confinement, no locks. |
| Handler work | `lifecycleScope` | A slow handler must never block a binder thread. |
| `evaluateJavascript` | main | The WebView is main-thread-only. |

Blocking the binder thread ANRs the app, and the trace points at the WebView
rather than at the handler that actually did it.

### Answering is mandatory

An invoke with no reply leaves a promise pending forever and the UI hangs with
no clue why. Unimplemented channels therefore reply with an **error**, never
with silence. The 26 channels not yet in the dispatch table are visibly broken
in the UI instead of invisibly stuck.

There is deliberately **no call timeout**. `native-server-start` and modpack
imports legitimately run for minutes. Pending work is cleared when the page
goes away, not on a clock.

### The dispatch table is the to-do list

The `BRIDGE-CHANNELS-BEGIN`/`END` markers are read by
`shared/conformance/check-coverage.js`, which fails on any required channel
without a handler:

```bash
npm run conformance:android
```

It currently reports 22 of 46 — expected, and the list is the work queue.
Only strings in declaration position (`"channel" to handler`) count, so a
string literal inside a handler body cannot masquerade as coverage.

**What conformance does not prove.** It checks that the host implements every
channel the profile *requires*. Nothing checks the other direction — that the
UI never calls a channel the profile omits. That gap is not theoretical: it
cost us the entire login flow (see below). Until there is a lint for it, the
router's "not implemented" warnings are the detector, which is one reason
unimplemented channels log rather than fail quietly.

### The login handshake

`credentials-received` is the one handler where getting the *event* wrong
hangs the app. The UI authenticates against the backend itself and hands the
tokens down; the host stores them and must then emit **`credentials-set`**.
The boot state machine in `pages/index.tsx` waits on that event before routing
to the dashboard, so a handler that stores the credentials and stays quiet
leaves the user on a spinner with nothing in the log.

### Page death

`onRenderProcessGone` fires when the render process is killed — most likely
by memory pressure, which is exactly what hosting a Minecraft server on a
phone causes. The router outlives any single WebView: the activity rebuilds
the view and calls `onPageGone()`, which re-arms the queue and cancels
in-flight handlers whose replies carry ids only the dead page understood.

Queued events are dropped rather than replayed. They describe a timeline the
fresh page never saw; it re-reads current state on mount instead.

## Building and running

```bash
npm run ui:android          # stage the shared UI bundle (required first)
npm run android:emulator    # start the AVD
npm run android:run         # build, install, launch, follow logs
```

`scripts/android-app.js` exists for the two things Gradle cannot do for
itself: pick a JDK it can actually run on (AGP 8.7 wants 17–21; Studio's
bundled JBR is often newer, and the resulting "Unsupported class file major
version" names nothing actionable), and refuse to build when the UI bundle
has not been staged.

Debug builds enable WebView remote debugging — open `chrome://inspect` on the
host machine to get real DevTools against the running app.

### Driving the app without an account

Most of the product sits behind a magic-link login, which needs a real inbox.
To exercise a path without one, talk to the page over the DevTools protocol:

```bash
adb forward tcp:9222 localabstract:webview_devtools_remote_$(adb shell pidof app.gethomerun.mobile.debug)
curl http://127.0.0.1:9222/json          # find the page's webSocketDebuggerUrl
```

Then `Runtime.evaluate` against it. Posting a synthetic envelope through the
real transport is how the post-login path was verified end to end:

```js
window.__homerunHost.postMessage(JSON.stringify({
  v: 1, method: "credentials-received",
  params: { access_token: "test", refresh_token: "test" },
}));
// -> host emits credentials-set -> UI routes to /dashboard
```

This drives the same code path the UI does, so it tests the host rather than
bypassing it.

## Known gaps

| Symptom | Cause |
|---|---|
| **The dashboard renders at desktop proportions** | The shared UI has no phone layout: the sidebar is a fixed width that consumes most of a phone screen and pushes content off it. The largest remaining piece of mobile work, and all of it lives in the UI repo. |
| React error #418 on boot | Hydration mismatch. The UI's SSR stub prerenders with *desktop* capabilities; the client resolves Android ones. Needs a fix in the UI repo, not here. |
| `sentry-ipc://` scheme errors flooding the console | `@sentry/electron` is bundled in the shared UI and tries to reach an Electron main process. Harmless, noisy; belongs to the UI repo. |
| `PostHog was initialized without a token` | The bundle carries no mobile PostHog key yet. |
| `discord-page-update is not implemented` | Desktop-only channel the UI does not capability-gate. Benign. |

## Triage

**Blank white screen, no console errors.** The bundle is not being served.
Check that `assets/web/index.html` exists in the APK, then that the asset
loader's domain matches the URL being loaded.

**`Uncaught SyntaxError: Unexpected token '<'` on every chunk.** `_next/` was
stripped from the APK — see the asset filter section. The merge-task
assertion should have caught this; check it still runs.

**Blank screen and `getBridge: no host detected` in logcat.** The
document-start script did not run, or it ran without setting
`__homerunHost.postMessage`. Confirm `DOCUMENT_START_SCRIPT` is supported on
that WebView build.

**A UI action spins forever.** An invoke with no reply. Find the channel in
logcat — unimplemented ones log a warning — and check `npm run
conformance:android`.

**ANR with a WebView-looking stack.** Something blocked the binder thread.
Handler work belongs on `lifecycleScope`, never inline in `postMessage`.

**`Unsupported class file major version` from Gradle.** JAVA_HOME points at a
JDK newer than 21. Set `HOMERUN_JAVA_HOME` to a 17–21 JDK.

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

**`scripts/check-capabilities.js` enforces that, and it had to.** iOS reads
`profiles.ios.capabilities` out of the vendored manifest at runtime and so
cannot drift; this host transcribes the same record by hand, and a hand copy of
eighteen fields is one that falls behind. It already had — `minigames` was in
the contract from the beginning and simply absent here, unnoticed, because the
UI reads a missing field as `undefined`, `undefined` is falsy, and the contract
happened to say `false`. That is the failure mode worth remembering: a drifted
capability does not break, it takes the wrong branch, and it takes the right one
often enough to look fine.

### Which loaders the UI offers

`serverLoaders` is the eighteenth field, and it exists because `moddedServers`
turned out to be too blunt a question.

A phone runs a JVM, so `moddedServers` is true and the create flow showed its
loader picker — offering all six the desktop offers, **including Spigot**, which
this host refuses at launch because BuildTools *compiles* Spigot on the device
and needs a JDK with `javac` while the staged runtime is pruned to a runtime. A
player could pick it, name a world, wait for the start, and only then be told
no. A refusal is the last line of defence; it should not be the design.

So the host advertises the list and the UI filters on it
(`lib/hostableLoaders.ts`):

```
vanilla  paper  fabric  quilt  neoforge  forge
```

Three properties are worth knowing, because each is a way this could go wrong:

- **The list is generated from the core.** `Loader::hostable()` sits beside the
  `Loader::parse` that does the refusing, and a Rust test reads the shipped
  `bridge-v1.json` off disk and asserts the two agree. `check-capabilities.js`
  compares this host against the *contract*; nothing compared either against the
  code that says no, and that gap is exactly how Spigot came to be offered.
- **Absent means show everything.** A host older than the key never sends it and
  supports the loaders it always did. Filtering it to nothing would be a worse
  bug than the one being fixed. `[]` is a different statement and means none —
  iOS sends that.
- **The comparison is case-insensitive.** The UI's ids are the API's uppercase
  `TYPE` values (`"SPIGOT"`); this list is lowercase, because that is how the
  core spells them. Comparing raw filters everything out, silently.

The same shape of gap is still open one level down: `java_policy` is `Exact` for
Forge and NeoForge, so Forge on 1.20.1 wants Java 17 and this build ships 21 and
25 — and the version list is not filtered per loader, so the UI can still offer
that pair. Closing it needs the bundled-Java list as a capability plus a
version-to-Java mapping.

## Launch colour and the system bars — `MainActivity`, `res/values/`

Three surfaces show before the page does, and all three are
`@color/launch_background` (`#5677DA`, the colour the bundle's splash animation
sits on): the system splash on API 31+, the window from `Theme.Homerun`, and
the WebView itself, which paints **white** until the document has a background
of its own. Missing that last one leaves the flash in place on every launch and
every rebuild after a render-process death.

`values-night/themes.xml` sets the same colours as `values/`. It still exists,
and deleting it would break something unrelated: WebView derives
`prefers-color-scheme` from the theme's `isLightTheme`, so the night parent is
what makes the shared UI's `system` setting follow the device at all.

Everything after that comes from the page, as a colour and not as a theme name:

```
document.documentElement class -> HomerunChrome.backdropChanged("rgb(18, 18, 20)")
```

The watcher reads `getComputedStyle(document.body).backgroundColor` and reports
it verbatim; `applyChrome` paints the container with it and picks the clock's
appearance from its luminance.

**It used to report `light`/`dark` and let the host map that to a hex of its
own, and the hex was wrong** — `#0A0A0A` against the UI's actual `#121214`,
which is close enough to look deliberate and far enough to read as a seam above
the status bar. There is no version of that arrangement that stays correct: the
bundle ships over the air and the host does not. So the host no longer keeps a
copy of the page's palette, and `res/values/colors.xml` deliberately has none.

Reading the page rather than the device is the other half of the point. The UI
is next-themes with `defaultTheme: "system"`, so a player can pin light or dark
and disagree with the phone. The theme XML resolves once, at activity creation,
and `uiMode` is in `configChanges` — so nothing is recreated when the device's
appearance changes either, and `windowLightStatusBar` stays whatever it was.
That is what left a black clock on a black page.

Reporting a colour also gets the launch right without a special case. The
bundle paints `html, body` brand blue and swaps to its own background only when
it adds `app-ready`, so through the whole splash animation the reported colour
*is* the blue and the bars stay blue with it. Luminance, not theme, is what
keeps the clock white on it — brand blue is a dark surface in both themes.

One thing the colours do not do is agree to the last digit: the host launches
on `#5677DA` and the bundle paints `#5778db`. One channel each, invisible in
practice, but they are meant to be the same field and one of them is a typo.

`HomerunChrome` is a second `@JavascriptInterface` object, deliberately not a
bridge channel: this is host chrome, and routing it through `BridgeRouter`
would put a method in the dispatch table that no manifest declares. The watcher
that feeds it is part of `bootstrapScript`, so it is injected at document start
alongside the capabilities.

`applyChrome` sets both the bar colours and the appearance flags. The colours
are what API 34 and below draws behind the clock; from API 35 they are ignored
and the page shows through instead. The appearance flags matter on every
version, which is why they go through `WindowInsetsControllerCompat` rather
than the theme. Both are set **per bar** — a sheet can dim the top of the
screen while leaving its own surface at the bottom, and one flag for both puts
a black clock on a dimmed page.

### The safe area — `holdSystemUiOutOfThePage`, `ChromeInterface.safeArea`

From Android 15 an app targeting SDK 35 is edge-to-edge and cannot opt out.
The window is the whole display, so a WebView filling it draws under the clock
and under the gesture pill. On the test phone that put the "Homerun Go"
wordmark behind the status icons and the pill through the "Create" tab label.

iOS never had this, and not because WKWebView is kinder: the shared UI has a
complete safe-area system — `--safe-top`, `--safe-bottom`, `--safe-left`,
`--safe-right`, and the `pt-safe` / `pb-safe` / `px-safe` classes that consume
them — defined from `env(safe-area-inset-*)`. WKWebView answers those. Android
WebView fills them in from a display **cutout** and never from the bars, so on
Android the UI was asking a question the platform would not answer, and every
answer came back zero.

So the host answers it. `holdSystemUiOutOfThePage` records the insets;
`ChromeInterface.safeArea()` hands them back as CSS pixels, and the
document-start script writes them onto `<html>` as an inline style, which
outranks the `:root` rule. Nothing forks — the same classes that space the UI
on iOS start working here, from the same variables.

**The host must not hold the space itself, and this is the part worth
remembering.** The first version did: inset the WebView by the bars, fill the
gap with two views, colour them from what the page reported it was painting.
It looks right in a screenshot and it is wrong in motion. The page's dim and
the host's strips are painted by two different compositors, so the strips
arrive a frame late and a visible seam opens across the top of the screen every
time a sheet opens. Sampling faster does not fix it — per-frame sampling was
tried and still read as lag. The fix is structural: the bars' space belongs to
the page, so there is one thing painting and nothing to keep in step.

The insets are pulled rather than pushed because a document that has just
started parsing must not paint even once without them; a page already loaded
when the insets change (rotation, a keyboard) gets `__homerunSafeArea()`
called on it instead. `CONSUMED` stops the inset dispatch at the container so
the WebView never sees a cutout of its own — `env(safe-area-inset-*)` stays
zero and the injected variables are the only source of the numbers.

#### The keyboard, and `adjustResize` not doing anything

`android:windowSoftInputMode="adjustResize"` does **not** resize this window.
The platform only insets a window's content while that window fits system
windows, and an edge-to-edge one by definition does not — so the same Android 15
change that created the safe-area problem above also quietly disabled the
keyboard handling. This document and the code both used to assert the opposite,
and excluded `Type.ime()` from the inset pass to avoid double-counting a resize
that was never happening. Nothing counted it at all. The keyboard opened over
whatever was on screen, and `innerHeight` did not move: a sheet with a text
field in it — claiming a guest account, naming a server — put the field the
player was typing into behind the keys.

So the host resizes the WebView itself: the IME inset becomes bottom padding on
the container, whose children are `MATCH_PARENT`. That is deliberately a real
resize rather than another reported variable, because a real resize is what the
shared UI already asks for — its viewport tag carries
`interactive-widget=resizes-content`, whose whole purpose is to shrink the
layout viewport for the keyboard rather than pan it, and which can do nothing
while the window it sits in never changes size. Honour it and `100dvh`,
`bottom-0`, `--visual-vh` and `.pb-keyboard` all come right together, for every
sheet in the bundle. Verified on a Pixel 9 Pro XL: `innerHeight` 997 → 639 with
the keyboard up, and back.

Two consequences worth knowing:

- **`--safe-bottom` goes to zero while the keyboard is up**, because the
  navigation bar it exists to avoid is behind the keys and the WebView now stops
  above them. Left in, every sheet would float 24dp above the keyboard.
- **`adjustResize` stays in the manifest** even though it is inert. Removing it
  leaves the mode unspecified, and the platform may then choose `adjustPan`,
  which slides the whole page up — status bar included.

`onPageFinished` re-reads the safe area for a related reason: an inset change
that lands while a document is loading is pushed with `evaluateJavascript` and
needs `__homerunSafeArea` to already exist. It does not, between a document
starting and its bootstrap running. That was harmless while these numbers never
moved after the first pass; with the keyboard in them, a missed push left the
page believing a keyboard that was already down still covered its bottom 24dp —
which is what reinstalling over a running app with the keyboard open produces.

### The Android 12+ splash, and why it draws no icon

From API 31 the platform shows a splash screen on every cold start whether the
app asks for one or not, and its default content is the launcher icon over the
window background. The bundle then plays its own logo animation — the mark
arriving, settling, lifting — so the default meant a static copy of the mark
appeared for a moment and was replaced by the animated one. The cut between the
two reads as a flash, and it is not something the UI can fix from its side: the
splash is gone before the page exists.

There is no attribute for "no icon". `windowSplashScreenAnimatedIcon` is
pointed at `drawable/splash_icon_none.xml`, a fully transparent shape, which
leaves the splash running with an empty icon slot — so the whole cold start is
one uninterrupted `launch_background` until the animation's first frame.

The two splash attributes sit in the base `values/themes.xml` and its
`values-night` twin rather than in a `values-v31`. Night beats version in
resource-qualifier precedence, so a `values-v31` would simply be passed over on
a dark-mode phone; doing it by qualifier properly would need four theme files.
Declaring the attributes unconditionally is safe — a platform never looks up an
attribute id it does not know — and `tools:targetApi="31"` is there to tell lint
so.

## The icons — `scripts/generate-icons.py`

Both Android icons are generated from one master, `brand/app-icon.png`: a flat
#5677DA tile with the white mark centred on it, carrying the padding the mark
is meant to have. Nothing is traced by hand.

It is vendored rather than read out of the desktop repo, because the two have
already drifted — `homerun-ui/assets/icon.png` is the same mark on a rounded
tile with far less air around it. The padding is load-bearing here (see the
table below), so which master you point at changes how big the icon looks.

The mark is *lifted* off the tile rather than cut out. Every pixel of the
master sits on the line between the tile colour and white, so the red channel
(86 → 255) gives the mark's coverage directly — antialiasing intact, and the
blue dots punched through the crossbars coming out as holes. Blue would work in
principle (218 → 255) and quantises to mush in practice.

What comes out are two assets with genuinely different rules:

| Asset | Canvas | Mark |
|---|---|---|
| `mipmap-*/ic_launcher_foreground.png` | 108dp, middle 72dp shown, 66dp guaranteed | the same share of the *visible* area it holds in the master |
| `drawable-*/ic_notification.png` | 24dp, no safe zone | 20dp — status-bar size, so it nearly fills the canvas |

The adaptive icon's background is not redrawn: `mipmap-anydpi-v26/ic_launcher.xml`
declares it as `@color/launch_background`, which is the master's tile colour
exactly. The script measures the master's tile and refuses to run if the two
have parted company, because the failure is a seam around the mark on every
launcher and nothing else would report it. It is also why the launcher icon,
the window and the splash are all one blue — the app opens on the colour it was
tapped on.

`monochrome` reuses the foreground layer, which works because the foreground
carries no colour of its own, only the mark's coverage, and themed icons are
drawn from alpha.

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

Handlers run on `Dispatchers.IO`, and four hop back with
`withContext(Dispatchers.Main)`: `clipboard-write-text`, `set-appearance`,
`quit-and-install` and `haptic`. The test for a fifth is narrow — a handler
needs the main thread only if it touches the WebView, the window, or a
framework object that builds a `Handler` from the *calling* thread's Looper.
`haptic` qualifies on the first count: `View.performHapticFeedback` is called
on the WebView, and the `webView` field is read *inside* the hop rather than
before it, because it belongs to whichever view currently exists and is
replaced when the render process dies.

### Haptics — `Haptics.kt`

The `haptic` send carries what the user just did, not what the motor should do.
Four of the six patterns map onto `HapticFeedbackConstants` and play through
`View.performHapticFeedback`, which needs no permission and honours Settings →
Sound & vibration → Touch feedback for free.

`success` and `error` have no constant — nothing in that vocabulary means "the
thing you asked for finished" — so those two go through `Vibrator`, which is
the whole reason `VIBRATE` is in the manifest. **Every `Vibrator` call declares
a touch usage** (`VibrationAttributes.USAGE_TOUCH`, or `AudioAttributes` with
`CONTENT_TYPE_SONIFICATION` below API 33). Without that the OS treats the buzz
as an alarm and plays it regardless of the owner's haptics setting, which is a
bug invisible to anyone who has never turned haptics off.

`minSdk` is 26 and most of the useful constants arrived in 30 (one in 34), so
every branch falls back. This is load-bearing: `compileSdk` is 35 so the newer
symbols resolve at compile time, and `abortOnError = false` in the app's lint
config means a missing `Build.VERSION.SDK_INT` guard is **not** a build
failure — it is a `NoSuchFieldError` on a real API-30 phone.

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

### Deep links

`homerun://` invite and join links. The manifest declares the scheme with no
host, so every shape matches and the UI's `parseDeepLink` stays the single
authority on which are meaningful.

**Auth does not come through here.** The magic-link flow generates a
`client_nonce`, mails a link, and then polls `/api/register/token/` until the
backend hands over a token — which happens the moment the link is opened,
anywhere. Login therefore works on a device that has never seen a deep link.
This is worth knowing before anyone plans App Links work for it.

Two delivery paths, and the split is not arbitrary:

| Arrival | Path | Why |
|---|---|---|
| Cold start (`onCreate`) | stored, pulled by `deep-link:consume` | No page exists. An event would flush at the `ready` handshake, which fires *before* the UI subscribes — straight onto the floor. |
| Warm start (`onNewIntent`) | pushed as a `deep-link` event | The UI is mounted and subscribed. |

`launchMode="singleTask"` is what routes a warm link to `onNewIntent` instead
of stacking a second activity — which also keeps a running server's WebView
alive. `setIntent()` in that callback is not optional; without it a later
`getIntent()` returns the launch intent forever.

The pending slot is deliberately **not** cleared on render-process death: a
link that arrived moments before the process died still deserves delivery to
its replacement.

#### The URL quirk that ate every link

`homerun:` is a non-special scheme, and URL implementations disagree about
whether `//` after it introduces an authority:

| Engine | `new URL("homerun://join/CODE")` |
|---|---|
| Node, desktop Chromium | `host: "join"`, `pathname: "/CODE"` |
| Android System WebView | `host: ""`, `pathname: "//join/CODE"` |

`parseDeepLink` read the action from `url.host`, so on Android it returned
`null` for every link — no error, no log, nothing ingested. The host was
delivering correctly the whole time.

Fixed in the shared parser (and its canonical desktop twin) by falling back to
the first path segment when there is no host. Regression tests use the
slashless form `homerun:join/CODE`, which Node parses into the same opaque
shape, so the behaviour is pinned without needing a WebView.

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

**Nothing buzzes.** `adb logcat -s HomerunHaptics` says which of the three it
is. A `performHapticFeedback(...) -> false` line means the owner has touch
feedback switched off, which is the setting working. `no vibrator on this
device` means exactly that, and an emulator always says it. No line at all
means the send never arrived — check the page is on a build new enough to have
haptics, and that `haptics` is true in the injected capabilities.

**It buzzes for taps but not for a completed action, or the reverse.**
`success` and `error` take the `Vibrator` path and the other four take
`performHapticFeedback`; a split like that is one of the two paths failing, not
the channel. The log names which ran.

**It buzzes on a phone whose owner turned haptics off.** A `Vibrator` call lost
its usage attributes. Untagged vibrations are treated as alarms and ignore the
setting.

**`Uncaught SyntaxError: Unexpected token '<'` on every chunk.** `_next/` was
stripped from the APK — see the asset filter section. The merge-task
assertion should have caught this; check it still runs.

**Blank screen and `getBridge: no host detected` in logcat.** The
document-start script did not run, or it ran without setting
`__homerunHost.postMessage`. Confirm `DOCUMENT_START_SCRIPT` is supported on
that WebView build.

**The clock and battery are invisible against the page.** The page's theme
report never arrived. `HomerunChrome` is added to the WebView in
`installWebView`, so a rebuilt WebView that missed it has no watcher at all —
capabilities would be missing too. Nothing polls; a missed report stays missed
until the next page load.

**The page draws under the clock or the gesture pill.** The safe-area
variables did not land. `ChromeInterface.safeArea()` is only useful if the page
reads it, so check `getComputedStyle(document.documentElement)
.getPropertyValue('--safe-top')` in `chrome://inspect` — zero there means the
document-start script did not run (see the `DOCUMENT_START_SCRIPT` triage
above), and a real value there means the screen in question is missing its
`pt-safe`/`pb-safe`, which is a fix in the UI repo.

**A sheet with a text field in it is behind the keyboard.** Ask the page, not
the code: `innerHeight` should drop by the keyboard's height and
`--safe-bottom` should go to `0.00px` while it is up. If neither moved, the IME
inset never arrived — confirm the keyboard really is showing with
`adb shell "dumpsys input_method | grep mInputShown"`, since a WebView that
never got focus shows nothing to resize for. If `innerHeight` moved but the
sheet did not, that is the UI's bound and belongs in the UI repo.

**Content sits 24dp above the keyboard, or clear of a pill that is not there.**
A safe-area push was missed while a document was loading, so the page is holding
numbers from before it existed. `__homerunSafeArea()` from `chrome://inspect`
re-reads them; if that fixes it, `onPageFinished` is not firing.

**Desktop window controls flash at the top right during launch.** Not a host
bug and not fixable here: `useCapabilities()` in the UI initialises its state
to the Electron defaults and only reads the injected capabilities in an
effect, so `CustomTitleBar` renders once, sees `windowChrome: true`, and is
removed on the next commit. `window.__homerunCapabilities` is injected at
document start and is available synchronously — the hook just does not read it
during the first render.

**The app icon flashes on screen before the logo animation.** The system splash
is drawing its default content. Check that both `windowSplashScreenAnimatedIcon`
and `windowSplashScreenBackground` survive in the theme the device actually
resolved — the dark-mode copy in `values-night/themes.xml` is the one that gets
forgotten, and it replaces the light theme wholesale rather than extending it.

**The launcher icon is the wrong blue, or the mark sits crooked in the mask.**
Do not nudge the PNGs. Re-run `python3 scripts/generate-icons.py`; the geometry
is derived from the master, and the adaptive background is a colour resource
that has to stay equal to the master's tile.

**A UI action spins forever.** An invoke with no reply. Find the channel in
logcat — unimplemented ones log a warning — and check `npm run
conformance:android`.

**ANR with a WebView-looking stack.** Something blocked the binder thread.
Handler work belongs on `lifecycleScope`, never inline in `postMessage`.

**`Unsupported class file major version` from Gradle.** JAVA_HOME points at a
JDK newer than 21. Set `HOMERUN_JAVA_HOME` to a 17–21 JDK.

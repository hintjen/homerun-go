# Validating error reporting on iOS

Every unexpected failure — JavaScript, Kotlin, Swift, Rust — should end up in
one `AppErrorReport` table, deduplicated and rate-limited before it leaves the
phone. The Android half is done and verified on hardware. **The iOS half has
never been compiled**, because it was written on Windows.

This runbook is how to prove it works, on a Mac, with a real device.

## What is already proven, and what is not

Knowing the boundary saves you from debugging things that are fine.

| Piece | Where it runs | Status |
|---|---|---|
| Fingerprinting, dedup, rate limiting, redaction | `homerun-core` (Rust) | 661 tests, and verified on a Pixel |
| `POST /api/app-error/`, the model, throttles | Django | tested, deployed to staging |
| Kotlin uncaught → stash → drain | Android | verified on a Pixel |
| Page error boundary, global listeners, API failures | shared UI bundle | built, **never run on any device** |
| Pre-boot JS hook and its handoff | both hosts | verified on a Pixel |
| **Everything Swift** | iOS | **uncompiled** |

The Swift files this touches:

- `AppErrors.swift` — the reporter, the `NSException` handler, the context
- `ExitDiagnostics.swift` — MetricKit; crashes and hangs
- `DebugTriggers.swift` — how you make it fail on purpose (DEBUG only)
- `BridgeController.swift` — the document-start JS hook and its stand-down
- `AppDelegate.swift` — where the three are started

Only `npm run test:swift-syntax` has ever seen them. That **parses** and does
not type-check: Linux Swift has no UIKit, WebKit or MetricKit, so a wrong
argument label or a missing property gets through it untouched.

## Step 0 — make it compile

This is the real work. Everything after it is comparatively quick.

```bash
npm run doctor              # tools first; it names what is missing
npm run build:ios           # stage the UI bundle + build the Rust static library
cd ios && xcodegen generate
open Homerun.xcodeproj
```

Build for a **real device**, not the simulator — MetricKit does not deliver on
the simulator, and neither does a tombstone.

Point it at staging, or nothing you send will land where you can see it:

```
Product ▸ Scheme ▸ Edit Scheme ▸ Run ▸ Arguments ▸ Environment
  HOMERUN_API_URL = https://api.fractalnetworks.co
```

Or `xcodebuild … HOMERUN_API_URL=https://api.fractalnetworks.co`. **A device
that has already run keeps the URL it stored** — delete and reinstall the app,
or the old backend wins. See [building.md](./building.md#ios).

### Things likely to be wrong, in order of likelihood

1. **`ExitDiagnostics.swift`** — MetricKit key names, `MXMetaData` properties,
   and whether `MXCrashDiagnostic.signal` is the `NSNumber?` this assumes. The
   call-stack tree is parsed out of `jsonRepresentation()` as loose JSON, so
   the *keys* (`callStacks`, `threadAttributed`, `callStackRootFrames`,
   `binaryName`, `offsetIntoBinary`, `subFrames`) are guesses from the
   documented schema, not from a payload anyone has looked at.
2. **`MetricKit` may need explicit linking.** Swift usually autolinks system
   frameworks; if not, add it to `ios/project.yml` under the target's
   `dependencies` as `- sdk: MetricKit.framework`.
3. **`AppErrors.swift`'s new `atMs` parameter** — it was threaded through two
   functions by hand.

If a fix is obvious, make it. If a MetricKit key turns out to be wrong, dump
`String(data: tree.jsonRepresentation(), encoding: .utf8)` once and correct the
parser against what actually arrives — that is the one thing this could not be
written against.

## Step 1 — the live path

Prove the reporter can talk to the API at all before testing anything that
involves dying.

```
HOMERUN_DEBUG_ERROR = report
```

Run. Three seconds after launch it sends. Expect in the Xcode console:

```
debug: arming report in 3.0s
debug: firing report
```

Then check for the row (see [Reading the results](#reading-the-results)):

| Column | Expected |
|---|---|
| `source` / `severity` | `host` / `error` |
| `kind` | `DebugTrigger` |
| `platform` | `ios` |
| `deployment` | `staging` |

**If nothing arrives, stop here.** Everything below depends on this working,
and the causes are few: no `HOMERUN_API_URL`, a stored URL from an earlier run,
or the device having no network.

## Step 2 — `NSException`, the stash-and-drain path

```
HOMERUN_DEBUG_ERROR = nsexception
```

The app raises, dies, and **sends nothing** — a dying process cannot finish an
HTTP request, so the crash is written to disk instead. Relaunch the app
normally (clear the environment variable first) and it drains on the way up:

```
sending 1 report(s) from the last run
```

Expect `source: host`, `severity: fatal`, `kind: HomerunDebugException`, and a
real `stack`. Two things worth checking:

- **`session` matches the process that died**, not the one that sent it. The
  stash carries its own context; that is deliberate, and a crash attributed to
  the launch that happened to deliver it would be wrong.
- **`bundle`** — see [the OTA trap](#the-ota-bundle-outranks-the-one-you-built).

## Step 3 — MetricKit

This is the new part, and the part with no Android equivalent to lean on.

### A Swift trap

```
HOMERUN_DEBUG_ERROR = trap
```

`fatalError` does **not** raise an `NSException`. It traps, the process takes a
signal, and the handler from Step 2 never fires. If a row appears for this,
MetricKit is genuinely wired.

### Do not wait a day

MetricKit delivers at most once every 24 hours, on launch. To get a payload
immediately, with the app running from Xcode:

```
Debug ▸ Simulate MetricKit Payloads
```

Expect in the console:

```
subscribed to MetricKit diagnostics
MetricKit payload: N crash(es), M hang(s)
```

The simulated payload is Apple's canned one, so the *contents* are not your
crash — but it proves the subscription, the parse and the send. A real crash
arrives on a later launch.

### A hang

```
HOMERUN_DEBUG_ERROR = hang
```

The main thread sleeps for 12 seconds. Expect `kind: hang`, `severity: error`,
and `extra.hangSeconds`.

> `severity: error`, not `fatal`, and that is not an inconsistency with
> Android's ANR. Android only hears about an ANR once the system has killed the
> process; MetricKit reports any hang past its threshold, recoveries included.

### What a native crash row will and will not have

Do not read a missing symbol as a bug:

- **Frames are `HomerunHost+0x1a2b3c`, not function names.** The dSYM is on the
  build machine. Symbolicate by hand with `atos -o <dSYM path> -l <load address>`.
- **The offsets move every build**, so a native crash regroups on each release.
  `kind` carries the signal (`crash (SIGSEGV)`) precisely so there is a stable
  coarse group underneath.
- **`location`** is the binary it stopped in — the fastest way to tell "ours"
  from "WebKit".

## Step 4 — the page

**This needs the UI branch merged first.** The mobile repo pins
`"homerun-app-ui": "github:hintjen/homerun-app-ui#main"`, and the reporting UI
is on `feat/app-error-reporting`. Until that lands on `main`, the bundle you
build has no page-side reporting at all and Steps 4a and 4b will report
nothing — which is not a bug in your build.

Attach **Safari ▸ Develop ▸ \<device\> ▸ Homerun** to get a console on the
WebView. No app code is needed for any of this.

### 4a — the handoff

```js
setTimeout(() => { throw new Error("deliberate handoff JS error, for verification") }, 0)
```

Expect **exactly one row**: `source: ui`, `kind: Error`, with a real stack.

There must be **no second row with `kind: boot`**. The host injects its own
error listener before the bundle boots, and it stands down once the page sets
`window.__homerunPageErrors`. Two rows here means the stand-down is broken, and
it is the failure that is invisible unless you look for it.

### 4b — the pre-boot hook

```js
window.__homerunPageErrors = false
setTimeout(() => { throw new Error("deliberate pre-boot JS error, for verification") }, 0)
```

Clearing the flag puts the page back in the state it is in while still loading.
Expect a row with `kind: boot`.

You will get **two** rows here — one `boot` from the host hook and one `Error`
from the page, because the page's own listener does not check the flag. That is
the test showing you exactly what the handoff prevents. In reality the flag is
only false before the page's reporter exists, so only one can ever fire.

### 4c — an API failure

Sign in, then in the Safari console request something that 500s, or put the
device on airplane mode and pull to refresh. Expect `source: api`,
`kind: http`, and `http_path` already templated (`/api/server/{id}/`).

**A 401 or 404 will produce nothing. That is deliberate** — 401 is the ordinary
token-refresh path and 404 is a legitimate answer on the polling endpoints.

## Reading the results

Both MCP servers live on the hub, bound to loopback, so start with `ssh hub`.
Staging is `metrics-dashboard-stage-mcp-1` on `127.0.0.1:18765`.

For a one-off read, run the query inside the container — it already holds the
read-only credentials, so nothing has to be copied anywhere:

```sh
ssh hub 'docker exec metrics-dashboard-stage-mcp-1 python -c "
import os, psycopg2
c = psycopg2.connect(host=os.environ[\"DB_HOST\"], port=os.environ.get(\"DB_PORT\", 5432),
                     dbname=os.environ[\"DB_NAME\"], user=os.environ[\"DB_USER\"],
                     password=os.environ[\"DB_PASSWORD\"])
cur = c.cursor()
cur.execute(\"SELECT date_created, source, severity, kind, left(message,60), fingerprint, platform, bundle, left(session,8), coalesce(length(stack),0), extra FROM api_apperrorreport WHERE platform = %s ORDER BY date_created DESC LIMIT 10\", (\"ios\",))
for r in cur.fetchall(): print(r)
"'
```

`metrics-dashboard/docs/staging.md` in the API repo documents the MCP
route in full, including how to speak MCP over HTTP instead.
Without hub access, `GET /api/admin/app-errors/top/` with an admin token
answers the same question more coarsely.

## Things that will otherwise cost you an hour

### The OTA bundle outranks the one you built

The app downloads UI bundles over the air, and an activated one **replaces the
bundle inside the app**. Staging your own UI into the app does not mean the app
is serving it.

Check the `bundle` column: `shipped` means the copy inside the app, anything
else is an OTA bundle. To force the shipped one, delete the app's downloaded
bundle and relaunch — it re-downloads and reclaims on a *later* launch, so you
get one session to work in.

### The reporter deliberately drops repeats

Firing the same trigger twice and seeing one row is **the design working**, not
a lost report. The first sighting of a fingerprint always sends; after that
there is a five-minute cooldown that doubles, up to an hour. The row carries
`occurrences`, and the count is what grew.

To get a fresh row, change the message — that changes the fingerprint.

`SESSION_MAX` is 20 reports per process, so a long debugging session will
eventually go quiet on purpose. Relaunch.

### One row is a group of failures, not one failure

Never `count(*)` this table. Sum `occurrences`. A component throwing on every
React commit produces ~3,600 events a minute from one phone and a single row
saying so.

### `device` and `user` will be NULL before registration

Anonymous reports are accepted on purpose: a crash on the login screen has no
token by definition, and those are the failures nobody can reproduce. A NULL
device is not dirty data.

## What is still not covered after all this

Worth knowing so nobody goes looking:

- **Native frames on Android** — a tombstone is a protobuf we have no schema
  for, so those rows carry the signal and no stack. Play Vitals has the
  symbolicated version.
- **iOS symbolication** — as above; offsets only.
- **401 and 404**, and anything the code catches and handles.
- **The 228 `customToast(…, "error")` call sites**, which are mostly validation
  and would swamp the channel.

## When you are done

Report which steps produced rows and which did not, with the `fingerprint` of
anything unexpected. If a step failed, the useful detail is whether it failed
at the **console log** (the trigger never fired), the **send** (fired, no row),
or the **contents** (row arrived, wrong fields) — those are three different
bugs in three different files.

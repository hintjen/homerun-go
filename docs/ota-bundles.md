# Over-the-air UI bundles

Every screen in this app is the shared web bundle from `homerun-app-ui`, and
both stores explicitly permit replacing that at runtime. `plans/ota-updates.md`
is the design and the policy argument; this file is what exists, how it behaves
on a device, and how to break it on purpose.

**Built so far: all of it, on Android.** The client checks for a bundle, verifies
its signature, downloads it, puts it on screen as soon as the app can take it,
judges it and rolls it back if it fails; the API serves signed manifests; a
workflow publishes them.

What is left is switching it on — the API branch deployed, the AWS credential
proven, and a store release carrying the public key — plus iOS, which is blocked
on Swift there never having been compiled.

The order was deliberate. The fallback is what stops a bad bundle bricking the
app, so it was built and proven on a device before anything could deliver one.

## The number that has to move first

`BRIDGE_HOST_REVISION` — `BridgeRouter.HOST_REVISION` on Android,
`BridgeRouter.hostRevision` on iOS, both currently **6**.

They do not have to move together, and mostly have not: the ledger's entries 2
through 5 are one host at a time catching up with the other. Revision 6 is the
exception — haptics landed on both at once — and it is why Android went from 4
straight to 6, which is legal because revision 5 introduced nothing for it.

`PROTOCOL.md` §7 versions the protocol and says changes are additive, so every
host answers `v: 1` for ever and a bundle cannot tell a January host from a July
one. That is harmless while the bundle and the host ship in one binary. It stops
being harmless the moment a bundle arrives over the air, because from
`CLAUDE.md`:

> **An unanswered invoke hangs a UI promise forever** — that is the worst failure
> mode in this protocol, and it looks like a frozen screen with no error.

The revision is a monotonic integer with three consumers: the update check sends
it so the server never offers a bundle this host cannot run; the client refuses a
bundle whose `minHost` exceeds it; and the UI reads
`window.__homerunHostRevision` and renders a feature gated on a missing channel
as absent rather than as a button that hangs. It is also on `get-app-version`,
alongside the bundle id.

**Bumping it is not optional and not remembered.**
`shared/conformance/host-revisions.json` records what each revision introduced,
per host, and `npm run test:host-revision` compares that against the dispatch
table each router actually declares — read exactly the way `check-coverage.js`
reads it. Three ways to fail, all build failures with the fix named in them:

| Situation | What the check says |
|---|---|
| A channel is answered but never introduced in the ledger | Add a revision *N+1* entry and set this host to *N+1* |
| A channel is answered and the ledger introduces it above this host's revision | Bump — this host is further along than it says |
| The ledger introduces a channel at or below this revision and the host does not answer it | Implement it, or lower the revision |

This is the same discipline as `FFI_ABI_VERSION` against `EXPECTED_ABI`, and for
the same reason: that pair drifted once already, silently, because the only
check ran at runtime in a path nobody was exercising.

**Capabilities are deliberately out of scope.** `plans/ota-updates.md` says "a
channel or a capability", but capabilities are already self-describing —
`__homerunCapabilities` is injected at document start and an old host reports
`backups: false` truthfully, so a bundle gated on one degrades correctly with no
revision involved. Channels are the ones with no way to ask.

## Where a bundle comes from

`BundleUpdater` checks at launch and on resume, throttled to once every six
hours, on `Dispatchers.IO` after the page has been asked to load. It never
blocks startup and every failure in it is survivable — the worst outcome of the
whole class never running is an app a few days out of date.

```
GET {API_URL}/api/mobile/bundle/?platform=android&host=1&app=0.1.0&channel=stable
Authorization: Bearer <device token>
```

The **device** token, not the user's: this is a property of the install, matching
how reporting is signed. `204 No Content` means "nothing for you" — a channel
with no release, or a rollout this device is not in yet — and is not an error.

The reply is a signed manifest:

```json
{
  "bundle":    "2026-08-14.1",
  "url":       "https://cdn.gethomerun.app/ui/2026-08-14.1.zip",
  "sha256":    "…",
  "minHost":   1,
  "serial":    7,
  "platform":  "android",
  "signature": "…"
}
```

Two fields are additions to what `plans/ota-updates.md` first sketched, and both
close a hole rather than add a feature:

- **`serial`** is the ordering, monotonic across releases on a channel. Ids are
  dates and dates do not order reliably across re-cuts. The client refuses
  anything whose serial is not **strictly greater** than what it is running,
  which is what stops a replayed old manifest rolling every device back to a
  version whose bugs an attacker already knows. The cost is that a deliberate
  rollback is a *forward* move: re-publish the older content under a higher
  serial.
- **`platform`** is signed, so an iOS manifest cannot be replayed at Android.

### The judgement is in Rust

Parsing, signature verification and the compare-against-installed all happen in
`homerun_core::bundle`, reached through one FFI call — `bundle.evaluate`. Two
calls would let a host judge a manifest it had not verified, and *that* mistake
has no symptom: everything keeps working, against any manifest anyone serves.
There is deliberately no way to obtain the fields of an unverified manifest.

Android also cannot do this itself. `minSdk` is 26 and the platform gained
Ed25519 at API 33, so verifying in Kotlin would mean bundling a crypto provider
for seven API levels and then writing it again in Swift.

SHA-256 stays in the host — the archive is streamed and every platform has a
correct implementation — but the *comparison* is `bundle.digestMatches`, so it
cannot be written three subtly different ways.

### The key

```
8d44ecfa010fe0136b450baee986a352cd027d3555403f0662dce5eb2ff16f4e
```

Checked into `android/app/build.gradle.kts` as the default for
`BUNDLE_PUBLIC_KEY`. It is public by nature — that is the point of signing
asymmetrically — and checking it in means no build can be made without it.

That matters more than it looks. An **empty** key disables over-the-air updates
entirely (`BundleUpdater` refuses to fetch what it cannot verify, the only safe
reading of "no key configured"), so a release built without the flag would look
completely healthy while silently never updating again — noticeable months
later, if at all. A malformed key fails the Gradle build outright, because a
typo cannot be caught usefully at runtime: the app would reject every manifest
for ever, which is indistinguishable from "nothing has been published".

**Changing it needs a store release.** A device only accepts manifests signed by
the key compiled into *it*, so every device keeps the old key until it updates
through the store. If the private half is ever lost, that is the recovery path —
a new pair plus a store release — and it is slow.

The private half lives in the `ui-bundle-publish` environment's
`HOMERUN_BUNDLE_KEY` secret, readable only by a job that has passed the reviewer
gate, and nowhere else.

**Sequencing:** publishing a bundle before a store release carries this key
reaches nobody. It is inert rather than harmful, but the first release with the
key in it is what switches over-the-air updates on for real.

`scripts/sign-manifest.js` generates the pair and signs manifests. It is the
second implementation of the signed payload format, kept in the repo that holds
the first so a change to one is an obviously incomplete change; a pinned vector
in `bundle.rs` fails if they ever drift.

## On disk

```
files/ui/current      the bundle being served
files/ui/previous     the last one known to have reached __bridge:ready
files/ui/pending      verified, not yet live
files/ui/probation    {"bundle":"<id>","attempts":N}
files/ui/.staging     an archive mid-unpack; never a bundle
(assets/web/)         the floor — never deleted, never overwritten
```

A bundle directory **is** the web root: `index.html` at the top of it, next to a
`bundle.json`:

```json
{ "id": "2026-08-14.1", "minHost": 1, "serial": 7 }
```

That file is written by `BundleStore` from the **signed** manifest, not taken
from inside the archive. The archive's own copy would be transitively signed via
the digest, but two copies of a fact eventually disagree, so only one is
authoritative. A bundle staged by hand has no serial and gets `0`, which any
real release outranks.

The unpack refuses entries that resolve outside the staging directory — an entry
named `../../databases/homerun.db` is a valid zip entry and `File(root, name)`
resolves it happily. It also caps entry count and expanded size: the digest
proves an archive is the one that was signed, not that it is well behaved.

Both files are required. A directory with no manifest is not a bundle with an
unknown name — it is something we did not put there, and serving it would leave
us with a UI we cannot name in a bug report or match against a probation record.
`index.html` is the same completeness marker `scripts/build-ui.js` uses, because
a half-copied export otherwise stages silently and shows a blank screen.

`minHost` is checked here as well as by the manifest server. The server is not
the only thing that can be wrong, and the cost of being wrong is a UI calling
channels this binary has never heard of.

## What happens at launch

`MainActivity.onCreate` calls `BundleStore.activate()`, then every
`installWebView()` calls `BundleStore.resolve()` — both before a WebView exists.

**Activate**, if something is pending: delete `previous`, move `current` to
`previous`, move `pending` to `current`, and write `attempts: 2`. If `current`
cannot be moved aside, the update is refused rather than taken one-way — with no
rollback target it would not be recoverable. Renames within one directory are
the atomic step; a half-unzipped tree never holds the final name.

**Resolve** answers with a directory or with the shipped assets:

1. `current` unusable → demote and ask again.
2. `current` on probation with attempts left → spend one, **write it to disk**,
   and serve.
3. `current` on probation with none left → demote and ask again.
4. Otherwise → serve it.

Demoting deletes `current` and promotes `previous` into its place — moving,
rather than serving `previous` where it lies, so the state converges. Left
alone, a broken `current` would be re-judged every launch for ever and the next
update would be applied on top of it.

`__bridge:ready` clears probation. That handshake is the only health signal
worth trusting: a bundle that throws on its first chunk never reaches it, and
one that does has proved it can run.

Three deliberate choices worth not undoing:

- **The counter is on disk.** The failure it survives is a bundle that kills the
  app before the page can say anything. An in-memory counter dies with the
  process without recording the attempt, so a fatal bundle would retry for ever
  — bricking the app in a way *no store update could fix*, because the broken
  bundle outranks the one in the binary. It is written before the page gets a
  chance to crash, and written atomically, because a torn file reads as absent
  which reads as confirmed.
- **Two attempts, not one.** The first launch after an update is also the launch
  most likely to be killed for reasons that are not the bundle's fault — a
  low-memory kill while a server is starting, the user swiping away mid-splash.
- **Resolve runs for every WebView, not just the first.** A dead render process
  is the strongest evidence a bundle is bad, so a fatal bundle rolls back within
  one session rather than waiting for relaunches.

There is **no per-request fallback** to the shipped copy. It is the
obvious-looking alternative and it is wrong: it would mix two builds, serving one
bundle's HTML against another's chunks. A bundle is usable or it is not, and
that is judged once, before the page loads.

Activation happens only where a page is being created anyway, never under a live
one. Swapping the bundle under a running page cancels whatever bridge call is in
flight, and `native-server-start` runs for minutes. The next section is how a
bundle gets a page created for it without waiting for a launch.

## Applying it: as soon as it arrives

**There is no update prompt on either host, and there never was one that a user
could see the point of.** A bundle that has been fetched, verified and unpacked
goes live immediately: the host promotes it and rebuilds the WebView, the user
sees a splash, and the app comes back on the new UI about a second later. The
shared UI's update card subscribes to `update-available`, so **never emitting
that event is what keeps the card off the screen** — the channel is still
declared, still gated on `autoUpdate`, and simply never fires on mobile.

`MainActivity.applyStagedBundle` and `BridgeController.applyStagedBundle` are
the same function twice. Both are called with a short reason for the log, both
answer "not now" far more often than they act, and both are no-ops when nothing
is staged.

**Two things make now the wrong moment, and both defer rather than cancel:**

- **A bridge call is in flight.** Rebuilding the WebView cancels every handler
  the page owns, and `wait-for-update-check` is *itself* one of them — the
  shared UI awaits it on the mandatory post-login path, so applying underneath
  it would hang login at a spinner with no error. That is the failure mode this
  repo is most careful about, and it would have been introduced by the feature
  meant to keep the UI current.
- **This device is hosting.** A running server survives the swap — that is what
  `ServerHost` is for on Android, and on iOS the engine lives in the backend
  rather than the page — but the console scrollback does not, and interrupting
  someone mid-session to reload the UI is a poor trade for a fix that can wait
  for the stop.

The two hosts draw the second line in the same place through different
questions: Android asks `ServerHost.hosting().busy`, iOS asks
`backend.lifecycle.activeIds()`. The difference that follows is the on-stop
backup: Android's `busy` stays true through it and iOS's does not. That is
deliberate rather than drift — Android's backup is bound up with the foreground
service and the notification, iOS's runs in a detached task that owns no page
state and cannot be disturbed by a reload.

**Every path back to idle asks again**, which is what makes "immediately" true
rather than "usually":

| Trigger | Android | iOS |
|---|---|---|
| The bundle was just staged | `BundleUpdater.onBundleStaged` | the same |
| The last in-flight call finished | `BridgeRouter.onPageIdle` | the dispatch task, when `pending` empties |
| A fresh page finished its handshake | `onPageIdle` from `onReady` | after `flushQueue()` |
| A run ended, or its backup did | `ServerHost.Listener` | `backend.onStateChanged` |
| The app came back to the foreground | `onResume` | `applicationDidBecomeActive` |

And if none of them ever fires, `BundleStore.activate()` at the next launch
still takes it. That path is untouched and remains the floor: **every deferral
is a delay, never a loss.**

`quit-and-install` still works and still applies without quitting. Nothing on
mobile sends it any more, but it costs one handler, it is required by the
`autoUpdate` capability both mobile profiles declare, and it is the manual
override if a screen is ever built for one.

The log is the way to watch this. Every decision is narrated at info level on
`HomerunBundle` / `HostLog.bundle`, and reads either

```
applying 2026-08-14.1 now (it was just staged)
```

or the reason it did not:

```
holding 2026-08-14.1 back (it was just staged): hosting srv_123 (RUNNING)
holding 2026-08-14.1 back (the page went idle): the page is mid-call
```

### Turning it off for a development build

A build whose point is the UI you just staged should not be racing the updater,
and since it now applies immediately the race is one it will usually win. One
flag per platform:

```bash
npm run android:run -- --no-ota      # shorthand for -PotaUpdates=off
xcodebuild … HOMERUN_OTA_UPDATES=0   # defaulted in ios/project.yml
```

Off means **ignore them entirely** rather than "do not fetch". `BundleStore`
promotes nothing, serves the shipped copy, and reports no pending bundle — that
last one matters more than it looks: the applier acts on `pending()`, and a
build that reported a bundle it would then refuse to activate would rebuild its
WebView on every idle moment, for ever. Nothing on disk is touched, so the same
device with the flag back on carries on where it left off.

On by default, debug included — the update path is only ever exercised on a
debug build. A release built this way would look completely healthy and silently
never update again, so `verifyReleaseConfig` refuses one; iOS has no equivalent
gate, which is a reason not to put the setting in a release invocation.

**Note what is *not* the switch.** Both hosts treat a blank signing key as
"off", and that branch is still there and still correct — but Gradle's `prop()`
falls back to the compiled-in default for a blank `-P` override, and the hex
`require` would reject an empty one, so `-PbundlePublicKey=` never disabled
anything. It was documented in the `on-device-build` skill for months as the way
to do this.

## Testing it on a device

No CDN needed — push a directory. On a debug build:

```sh
adb push ./mybundle /data/local/tmp/ota/mybundle
adb shell chmod -R 777 /data/local/tmp/ota
adb shell "run-as app.gethomerun.mobile.debug sh -c \
  'mkdir -p files/ui && rm -rf files/ui/pending \
   && cp -r /data/local/tmp/ota/mybundle files/ui/pending'"
adb shell am start -n app.gethomerun.mobile.debug/app.gethomerun.mobile.MainActivity
adb logcat -s HomerunBundle:*
```

`HomerunBundle` narrates every decision, which is the point — a silent fallback
would be indistinguishable from a network problem.

The four cases worth re-running after touching any of this, all verified on the
emulator when it was built:

| Bundle | Expected |
|---|---|
| `index.html` that sends `__bridge:ready` | activated, served on probation, then `confirmed`; probation file gone |
| The same, pushed while the app is in the foreground | live without relaunching. `adb push` writes `pending` behind the host's back, so nothing announces it — but every trigger in *Applying it* re-asks, and the next bridge call to complete is one. Bring the app back to the foreground if you are impatient |
| `index.html` that throws before `ready` | two probation launches, then `rolling back`, and the previous bundle serves |
| `"minHost": 99` | `refusing it`, pending discarded at the next launch, current untouched. Expect the refusal line more than once: every idle moment re-reads `pending` looking for something to apply, and this is what it finds |
| `current` with `index.html` deleted, no `previous` | `the live bundle is unusable`, then the shipped bundle, and the real UI loads |

The second one is the only test that matters. Everything else here is plumbing.

## Driving the updater without a server

`scripts/sign-manifest.js` plus any static file server is enough to exercise the
whole chain. Debug builds permit cleartext to `10.0.2.2`, `localhost` and
`127.0.0.1` and nowhere else (`app/src/debug/`), so the stand-in endpoint can be
plain HTTP — but the **archive URL must be https**, which the core enforces, so
the zip has to be somewhere real.

```sh
node scripts/sign-manifest.js keygen            # public half -> the build

zip -r bundle.zip .                              # index.html at the ROOT
aws s3 cp bundle.zip s3://homerun-ui-bundles/ui/2026-08-14.1.zip \
  --cache-control "public, max-age=31536000, immutable"

HOMERUN_BUNDLE_KEY=<private> node scripts/sign-manifest.js sign \
  --archive bundle.zip --bundle 2026-08-14.1 --serial 1 \
  --url https://cdn.gethomerun.app/ui/2026-08-14.1.zip --out manifest.json

# serve manifest.json at /api/mobile/bundle, then:
cd android && ./gradlew assembleDebug \
  -PapiUrl=http://10.0.2.2:8787 -PbundlePublicKey=<public>
```

`adb logcat -s HomerunUpdate:V HomerunBundle:V` narrates both halves. The
throttle survives a reinstall, so clear it between runs:
`adb shell run-as app.gethomerun.mobile.debug rm -f shared_prefs/bundle-updater.xml`.

The four checks worth re-running after touching any of this, all verified on the
emulator against the real CDN when it was built:

| Manifest | Expected |
|---|---|
| Correctly signed, newer serial | fetched, staged, and live within a second — the page reloads on its own, with no prompt and no relaunch |
| The same, with a server running | fetched and staged, `holding … back`, then live the moment the server stops |
| A signed field edited in transit | `refusing the manifest: the manifest's signature does not match`, nothing fetched |
| Validly signed, digest of other bytes | downloaded, then `does not match its signed digest; discarding it` |
| Validly signed, serial ≤ installed | `no update: the offered bundle is serial N, older than the installed N` |

The middle two are the ones that matter. A signature that verifies a manifest
nobody can tamper with is the only reason it is safe to take an entire user
interface from a CDN.

## The endpoint that answers

Built, on branch `api/ota-ui-bundles` in the `homerun` repo —
`api/docs/ota-ui-bundles.md` is the server side in full. `UiBundle` is one row
per release per platform; the newest row a device qualifies for resolves.

Two things it does that matter here:

- **It never signs.** Manifests are stored and served exactly as CI signed
  them, so an attacker holding that database cannot forge a bundle — only
  withhold updates or re-offer something genuinely published once, which the
  monotonic serial closes.
- **A serial that does not climb is a `409` at publish**, not a release that
  publishes cleanly and is declined by every device.

## Publishing

`.github/workflows/publish-ui-bundle.yml`, manual dispatch only and gated on
the `ui-bundle-publish` environment. It stages the UI with the same
`build-ui.js` the APK and the IPA use, zips that tree, asks the API what serial
to sign, signs, uploads, and registers the release — for one platform or for
both, chosen with the `platforms` input.

**One archive, two publishes.** `build-ui.js` stages a byte-identical tree into
both hosts' asset directories — same source, same source-map filter, only the
destination differs — so the zip is built once and the same bytes go up under a
key per platform. What cannot be shared is the manifest: `platform`, `url` and
`serial` are all signed fields, all three differ, and the core refuses a
manifest built for the other platform outright (`declines_the_other_platforms_bundle`
in `bundle.rs`). The workflow checks the two staged trees still match and fails
if they ever diverge, because nothing downstream would notice iOS being handed
Android's tree.

`min-host` is one value for the whole run. `BRIDGE_HOST_REVISION` is a single
shared ledger rather than a per-platform counter, and the hosts do not have to
sit at the same revision — they happen to both be at 12 today — so a `min-host`
above the lower of the two quietly excludes that platform instead of failing at
publish time. Check both before raising it; do not trust this number.

### What this runner does not have

The job runs on `[self-hosted, Linux, X64, simrig]`, the org's Debian 12 box,
sharing it with the Pumpkin runner and a production restic stack. It moved
there off `ubuntu-latest` so that shipping a UI does not depend on hosted-runner
billing. A persistent box is not a fresh image:

| Absent | What the workflow does instead |
|---|---|
| `zip`, `unzip` | Builds the archive with `zipfile` in an inline `python3`. `publish-android.yml` already unpacks the same way, for the same reason. |
| `aws` | A root-free AWS CLI v2 in `~/opt/aws-cli`, entry point `~/.local/bin/aws`. The runner service does not source a login shell, so the job adds that directory to `$GITHUB_PATH` itself. |
| `pip`, `ensurepip` | Nothing needs them — and `python3 -m venv` fails outright here, which is why the CLI is the vendored installer rather than a pip install. |

`node` and `npm` are missing from the login shell too, and that one is
harmless: `actions/setup-node@v4` puts them in the runner's tool cache.

A preflight step checks every one of these and fails at the top of the job with
the list, instead of several minutes in. **If the box is ever rebuilt, that
step is what will tell you.** Reinstall the CLI with:

```bash
curl -sSL https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip -o aws.zip
python3 -c 'import zipfile,os;z=zipfile.ZipFile("aws.zip");[os.chmod(z.extract(m,"."),m.external_attr>>16) for m in z.infolist() if m.external_attr>>16]'
./aws/install --install-dir ~/opt/aws-cli --bin-dir ~/.local/bin --update
```

**The home directory persists and is shared**, so no secret may be written to
`~/.ssh`. The deploy key goes to `$RUNNER_TEMP/ssh`, which is the job's alone
and cleared between runs, with `GIT_SSH_COMMAND` pointing git at it — the same
mechanism and the same reasoning as `publish-android.yml`. A key left in
`~/.ssh` would outlive the job that needed it and be readable by the other
runner's jobs.

Four things about it worth not undoing:

- **It pins to `package-lock.json`** (`HOMERUN_UI_NO_UPDATE=1`) rather than
  re-resolving the UI branch. This publishes an interface to every phone and
  the lockfile is the only record of which commit that was.
- **Upload happens before the API is told.** The reverse order leaves a window
  where devices fetch a URL that 403s.
- **It refuses to overwrite an existing archive**, checked through the CDN
  because the upload credential is `PutObject`-only and cannot list the bucket.
  Note the check can say "already published" about an object that has since
  been **deleted from S3**: archives are served `immutable` with a one-year
  TTL, so CloudFront keeps answering from the edge long after the origin is
  empty. That fails in the safe direction — it blocks rather than overwrites —
  but if it ever fires for a key you believe is gone, the fix is a CloudFront
  invalidation, not another upload.

That caching behaviour matters beyond the guard: **deleting a bad archive from
S3 does not stop devices fetching it.** The edge will serve it for a year. The
only thing that actually stops a release is the manifest — set its `rollout` to
0 so it is no longer offered, and publish a replacement at a higher serial for
the devices already on it.

Stage publishes under `ui/stage/`, prod under `ui/`. There is one bucket and one
CloudFront, but stage and prod are separate databases with independent serials —
so both count from 1 and would otherwise want the same key on the same day. The
prefix is what stops one target overwriting bytes the other's signed manifest
already names.

Platforms collide the same way and for the same reason, so each takes a segment
of its own beneath that: `ui/ios/`, `ui/stage/ios/`. Android keeps the bare path
because its archives are already published under it and a signed manifest names
the URL — a published key cannot move.

It cannot run until three things exist: `HOMERUN_BUNDLE_KEY` (generate with
`sign-manifest.js keygen`), the AWS upload credential as repository secrets
*here*, and the API branch deployed.

## Still to build

The update prompt and the iOS half were the two items here. The iOS half is
written; the prompt was built, shipped and then **removed** — see *Applying it:
as soon as it arrives*. An update that costs a second and that nobody can
evaluate is not a decision worth interrupting someone for, and "later" meant
the next launch either way. `quit-and-install` and `wait-for-update-check` are
still answered by both hosts and `update-available` is still declared; nothing
on mobile emits it. On mobile "restart" is a WebView rebuild, about a second,
with the running server surviving it.

**iOS, as of 2026-08-13**, compiles and is proven on the simulator, both
halves. The store: the shipped floor, a hand-placed bundle, activation of a
`pending` one, probation, rollback to `previous` and to the floor, the
`minHost` refusal, an unusable `current`, and apply-on-request without
relaunching. The network: a manifest signed over the real published archive
`ui/stage/2026-08-13.2.zip` fetched, verified, digest-checked, unpacked,
staged, activated and confirmed with the real UI rendering — plus all three
tampered-manifest refusals, with the same messages Android gives.

One caveat left, and it is a smaller one than it was. Until 2026-08-19 the
**real stage API had never served iOS a manifest** — it answered `204` for
`platform=ios` because nothing had ever been published there, which the client
reports correctly as `the server has no bundle for this host`. The publisher
grew an iOS leg that day and `2026-08-19.1` is published to stage at 100%, run
`32295191237`, signed with the production key rather than a throwaway.

So the server half is no longer hypothetical. What remains unobserved is the
last hop: an iOS device pointed at stage actually fetching it. That needs a
device with a valid stage token — the bundle endpoint is authenticated, so it
cannot be checked from a shell — and it is step 4 of `plans/ios-ota.md`, which
carries the rig and the traps.

One bug worth remembering, because the same shape can recur in any port of
this: `BundleStore.activate()` existed and was correct but had **no caller at
launch** — only `quit-and-install`. Every log line said the right thing while
no bundle ever went live. When porting this to a fourth host, check the call
sites before the file contents.

What is genuinely left is switching it on: the API branch deployed, the AWS
credential proven, and a store release carrying the public key.

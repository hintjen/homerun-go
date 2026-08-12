# Over-the-air UI bundles

Every screen in this app is the shared web bundle from `homerun-app-ui`, and
both stores explicitly permit replacing that at runtime. `plans/ota-updates.md`
is the design and the policy argument; this file is what exists, how it behaves
on a device, and how to break it on purpose.

**Built so far: the client half.** A bundle already on disk is activated,
served, judged, and rolled back if it fails. Nothing downloads one yet — no
manifest endpoint, no signature check, no updater. That is deliberate: the
fallback is what stops a bad bundle bricking the app, so it exists before
anything can deliver one.

## The number that has to move first

`BRIDGE_HOST_REVISION` — `BridgeRouter.HOST_REVISION` on Android,
`BridgeRouter.hostRevision` on iOS, both currently **1**.

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

## On disk

```
files/ui/current      the bundle being served
files/ui/previous     the last one known to have reached __bridge:ready
files/ui/pending      verified, not yet live
files/ui/probation    {"bundle":"<id>","attempts":N}
(assets/web/)         the floor — never deleted, never overwritten
```

A bundle directory **is** the web root: `index.html` at the top of it, next to a
`bundle.json`:

```json
{ "id": "2026-08-14.1", "minHost": 1 }
```

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
flight, and `native-server-start` runs for minutes.

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
| `index.html` that throws before `ready` | two probation launches, then `rolling back`, and the previous bundle serves |
| `"minHost": 99` | `refusing it`, pending discarded, current untouched |
| `current` with `index.html` deleted, no `previous` | `the live bundle is unusable`, then the shipped bundle, and the real UI loads |

The second one is the only test that matters. Everything else here is plumbing.

## Still to build

- The manifest endpoint on the API — authenticated, uncached, device-signed.
- Ed25519 verification of the manifest, and the SHA-256 check on the download.
- The updater itself: check on launch and resume, throttled, writing `pending`.
- Uploading bundles to `s3://homerun-ui-bundles/ui/` behind CloudFront, which is
  provisioned (`plans/ota-updates.md`) but has nothing in it.
- The iOS half. `AppSchemeHandler.resolve(path:in:)` is the same one-function
  seam; `BundleStore` is the shape to copy.

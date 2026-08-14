# Publishing Homerun Go to Google Play

What it takes to get a build in front of testers, and the things about Play
that are not discoverable from the Console.

For *building* the artifact see [`building.md`](building.md) — particularly
the two rules about cgo and page alignment, both of which shipped broken once.

---

## The account

| | |
|---|---|
| Developer account | **Ethereal Network Sciences PBC**, Organization |
| Package | `app.gethomerun.mobile` |
| Store name | Homerun Go — must match `app_name`, which the first upload did not |
| Track | Internal testing since 2026-08-13 |

Play App Signing is enabled: **Google holds the key that signs what users
install.** Ours is only an *upload* key, proving an upload came from us. That
distinction matters when something goes wrong — losing the upload key is a
support request, not an app that can never be updated again.

## The upload key

`android/upload.jks` with `android/keystore.properties` beside it. Both are
gitignored (`*.jks`, `keystore.properties`) and neither is backed up anywhere
else, which is worth fixing.

```
CN=Homerun Go, O=Hintjen, C=US
RSA 2048, PKCS12, valid to 2053   (Play requires past 22 Oct 2033)
SHA-256  50:DF:E7:57:6C:52:A0:F9:18:45:31:5B:96:F5:3E:57:
         A3:15:34:D6:BB:A7:F0:3D:7C:C7:F2:DA:F3:BD:FD:98
```

Store and key passwords are the same value, so CI needs one secret for both.

Regenerating it is nearly free *before* the first upload and awkward after:
from then on every upload must be signed by the same key, and changing it
means an upload-key reset request to Google — a few business days, and an
owner-level action.

## Publishing by hand

```bash
npm run build:android:release
cd android && ./gradlew :app:bundleRelease -Pabi=arm64-v8a \
  -PversionCode=N -PversionName=X.Y.Z
```

Then Play Console → Testing → Internal testing → **Create new release**, drop
in `app/build/outputs/bundle/release/app-release.aab`, and **Start rollout**.

> **Saving is not rolling out.** An app sits at "Draft" until a release has
> actually been rolled out, and until then no tester can install anything and
> the Play Developer API refuses uploads. This is the single most common way
> to be stuck.

Testers opt in through the link under Testing → Internal testing → Testers →
*Copy link*, opened on the phone with the same Google account. Internal
testing propagates in minutes; a 404 on that link is almost always the wrong
account rather than propagation.

**The bundle is arm64-only, so an x86_64 emulator cannot install it** — Play
serves per-device splits and correctly reports no compatible build. Testing a
store build needs a real phone. For the emulator, keep using `android:run`,
which installs `app.gethomerun.mobile.debug` under its own package id.

## Publishing from CI

`.github/workflows/publish-android.yml`, manual dispatch, gated on the
`android-publish` environment. It stages with the same
`npm run build:android:release` a human runs, so the two cannot drift.

Secrets it needs, all on that environment:

| Secret | |
|---|---|
| `ANDROID_KEYSTORE_BASE64` | `base64 -w0 android/upload.jks` (PowerShell: `[Convert]::ToBase64String([IO.File]::ReadAllBytes($p))`) |
| `ANDROID_KEYSTORE_PASSWORD` | either password from `keystore.properties`; they are identical |
| `PLAY_SERVICE_ACCOUNT_JSON` | Google Cloud service account key, granted *Release to testing tracks* |
| `UI_REPO_DEPLOY_KEY` | read-only deploy key on `homerun-app-ui` |
| `WIREPROXY_DEPLOY_KEY` | read-only deploy key on `wireproxy-fork` |

Two deploy keys against one host means an SSH `Host` alias per key — GitHub
resolves the repository *from the key*, so they cannot both be `github.com`.

**No required reviewers are configured**, deliberately, while the audience is
a handful of internal testers. Turn them on before the first production
release: the upload key lives in these secrets, so past that point "can push
to this repo" silently means "can ship to every user."

### Known gaps

- **CI has never completed a run.** It fails in `cargo` fetching the private
  `hintjen/Pumpkin` and `hintjen/rustic-fork` dependencies: cargo's bundled
  libgit2 does not use the SSH config the workflow writes. The fix is
  `CARGO_NET_GIT_FETCH_WITH_CLI=true` plus deploy keys for those two repos.
- **The first release must be manual.** The Play Developer API refuses uploads
  for a package with no rolled-out release, so CI cannot bootstrap an app.

## What Play asks for

**Service accounts are no longer set up in Play Console.** The "API access"
page has been retired — create the account entirely in Google Cloud (IAM →
Service Accounts, no project roles needed), enable the *Google Play Android
Developer API*, then invite its email under Play Console → Users and
permissions and grant *Release to testing tracks* on this app only. Nothing
needs linking. Permission changes can take up to 24 hours, so a 401 on a first
run is more likely propagation than misconfiguration.

**Foreground service type.** `specialUse` is reviewed by hand and takes up to
seven days. The declaration text and demo-video script are in
[`android-lifecycle.md`](android-lifecycle.md#play-policy-the-open-item-and-how-to-close-it) —
use them rather than improvising, because the criterion is "show nothing else
fits", not "describe the feature".

**Account deletion.** Play requires it reachable both in-app and from a web
URL. The API side is `POST /api/user/delete/` plus an emailed confirmation.

**Data safety.** Declare email, device identifiers, crash logs, PostHog
analytics, and the player names and Mojang UUIDs that ride along with stats.

## Deadlines

| | |
|---|---|
| **targetSdk 36** for new apps and updates | **31 Aug 2026** — the repo is on 35 |
| 16 KB page size for targetSdk 35+ | already in force (1 Nov 2025, extended to 31 May 2026) |
| Upload key validity past 22 Oct 2033 | satisfied to 2053 |

## Costs

Google charges nothing beyond the one-time $25 registration: internal testing
is free and unlimited, with no per-upload fee and no review queue.

The cost is GitHub Actions, because this is a **private repo in an
organization**. A run is 20–25 minutes, and the archived artifacts are ~110 MB
against 500 MB of included storage — which is why retention is 7 days rather
than 30. While iterating on a bug, build locally: it is free, faster, and
produces the identical signed artifact.

`versionCode` is one-way. Play rejects one it has seen, so every upload burns a
number for ever. CI defaults to `1000 + run_number`, which clears the low
numbers used by hand-built uploads.

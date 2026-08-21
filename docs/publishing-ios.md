# Publishing Homerun to TestFlight

## Overview

Getting an iOS build in front of testers. The build procedure itself is in
[`../plans/ios-ota.md`](../plans/ios-ota.md) § *Publishing a build* and
[`building.md`](./building.md); this is the delivery half — signing, App
Store Connect, the CI workflow, and the runner it depends on.

The shape mirrors [`publishing-android.md`](./publishing-android.md) where
the platforms agree, and this file is written as the delta where they do not.

## The account and the team

The Apple Developer team is Hintjen PBC, `35DS8JGY4Y` — checked into
`ios/project.yml` as `DEVELOPMENT_TEAM`, because a Team ID is public in every
signed binary and leaving it to each machine's Xcode to guess is how a build
ends up personally signed. The app's bundle id is `app.gethomerun.ios`.

Signing is **automatic**. There is no upload keystore to guard, no
`keystore.properties` equivalent: Apple mints the certificate and profile on
demand. The credential that authorises that from CI is an **App Store Connect
API key** (Users and Access → Integrations), which replaces both the keystore
and Play's service-account JSON in the Android story. Its `.p8` downloads
once and never again — same rule as the APNs key, keep the original safe.

Do not turn signing off to sidestep a failure. An unsigned app has no
application identifier, the Keychain rejects every write with
`errSecMissingEntitlement (-34018)`, and the symptom surfaces much later as
an app that behaves as though it were never signed in.

## Publishing by hand

The commands are in `plans/ios-ota.md` and they are what the workflow runs:

```sh
npm run ui:ios && npm run rust:ios && node scripts/build-wireproxy.js ios
cd ios && xcodegen generate
xcodebuild -scheme HomerunHost -sdk iphoneos -configuration Release archive \
  -archivePath build/HomerunHost.xcarchive
xcodebuild -exportArchive -archivePath build/HomerunHost.xcarchive \
  -exportOptionsPlist ExportOptions.plist -exportPath build/export
```

`ios/ExportOptions.plist` is committed with `destination: export`, so a hand
run produces an `.ipa` on disk and can never accidentally ship; uploading is
the deliberate act of flipping that key to `upload` (the workflow does it
with `plutil`) or dragging the `.ipa` into the Transporter app.

Two identities per upload, both numeric and period-separated (Apple rejects
"beta" the same way Play does):

| Field | Meaning | Rule |
|---|---|---|
| `MARKETING_VERSION` | what players see | bump when it means something |
| `CURRENT_PROJECT_VERSION` | the build number | unique per upload, `yyyymmddNN` |

## Publishing from CI — `publish-ios.yml`

Manual dispatch only, like Android's. A routine run needs **no inputs**: the
marketing version comes from `project.yml`, the build number is generated
(`yyyymmddNN`, `NN` from the run number), and the defaults are production API
+ TestFlight. `destination: ipa-only` is the smoke-build escape hatch — sign
everything, upload nothing, attach the `.ipa` as a run artifact.

Secrets live on the `ios-publish` environment:

| Secret | What it is |
|---|---|
| `ASC_API_KEY_ID` | the API key's Key ID |
| `ASC_API_ISSUER_ID` | the issuer id from the same page |
| `ASC_API_KEY_P8` | the `.p8` contents, verbatim |
| `IOS_GOOGLE_SERVICES_PLIST_PROD` | `GoogleService-Info.plist`, base64 — optional |
| `IOS_GOOGLE_SERVICES_PLIST_STAGING` | same, for the staging Firebase project |

The Firebase plists are chosen **by backend, not build type** — the pairing
that matters is Firebase project ↔ backend FCM credential
([`building.md`](./building.md) § *Push credentials*). Missing is legal: the
build runs with push inert and the workflow says so in a warning.

The reviewer gate on the environment is **off**, the same deliberate trade as
Android — turn on Required reviewers before the first App Store release.
Unlike Play, there is no track choice to constrain: an upload only ever
reaches TestFlight, and promoting to the App Store is a decision taken in
App Store Connect, not one a workflow typo can take.

### The runner is our Mac, and that is a real dependency

iOS builds cannot leave macOS, so the workflow runs on the org's self-hosted
runner (`runs-on: [self-hosted, macOS]` — fractals-Mac-Pro). The machine
persists between runs, which is what makes warm builds fast (cargo and Go
caches survive) and is also why the workflow shreds the API key and the
Firebase plist in an `always()` step: nothing here "gets destroyed anyway".

What the machine must hold, checked every run by `node scripts/doctor.js ios`
before anything expensive starts:

- Xcode (the workflow selects it with `DEVELOPER_DIR`; global `xcode-select`
  stays on CommandLineTools)
- `xcodegen`, `go`, `cmake` from Homebrew; `gomobile` installed and inited
- rustup with `aarch64-apple-ios`
- `~/.cargo/config.toml` with `git-fetch-with-cli = true`
- the wireproxy fork at `~/src/wireproxy-fork` (the workflow fast-forwards
  it each run, and clones it if missing)
- a GitHub credential in the **macOS keychain** that reads the private
  hintjen repos over https. Apple's git wires the `osxkeychain` helper in via
  `/Library/Developer/CommandLineTools/usr/share/git-core/gitconfig` — a
  Homebrew-installed git would not read that file, so if `brew install git`
  ever lands on this machine, the credential helper must be configured for it
  explicitly.
- a **logged-in GUI session**. The runner is a LaunchAgent; at the login
  window there is no unlocked keychain and signing hangs. A dedicated CI Mac
  wants automatic login enabled.

There are no deploy-key steps, unlike `publish-android.yml`: the machine's
keychain credential covers all three private repos at once.

### Known gaps

- **The app record does not exist yet.** Create `app.gethomerun.ios` in App
  Store Connect once, by hand; until then an upload has nothing to attach to.
  Unlike Play there is no "first upload by hand" rule — the first build can
  come through the workflow.
- **The `ios-publish` environment and its secrets are not created yet** —
  Settings → Environments, then the five secrets above.
- **Cloud signing is unproven on this team.** `-allowProvisioningUpdates`
  with the API key should mint the distribution certificate itself; if the
  archive step fails asking for a signing identity, create one Apple
  Distribution certificate in Xcode on any team machine once, or in the
  developer portal, and keep it in the runner's login keychain.

## What Apple asks for

The review questions worth having written down before submitting — argued in
full in [`../plans/ios-ota.md`](../plans/ios-ota.md) § *The App Store
question* and [`../plans/ios-background-execution.md`](../plans/ios-background-execution.md):

- **Guideline 3.3.2 (downloaded code)**: the OTA UI bundle rides WebKit's
  explicit carve-out, and the shipped bundle is a permanent floor so the
  submitted app is complete on its own.
- **Backgrounding**: iOS cannot host in the background and the app does not
  pretend to; have the story from the background-execution sweep to hand.
- Encryption export: the app uses standard TLS only —
  `ITSAppUsesNonExemptEncryption` belongs in `Info.plist` to skip the
  per-build questionnaire.

## Triage

**The archive step hangs, or fails in codesign with no useful error** — the
Mac is at the login window, or the login keychain is locked. Log the session
in (`ssh` is not enough for the keychain) and re-run.

**`No signing certificate "iOS Distribution" found`** — cloud signing did not
mint one; see Known gaps above.

**`error: exportArchive: The data couldn’t be read` naming ExportOptions** —
the `plutil -replace` ran against a stale copy, or the plist was hand-edited
into invalid XML. `plutil -lint ios/ExportOptions.plist`.

**Upload rejected: `The bundle version must be higher than the previously
uploaded version`** — the generated `yyyymmddNN` collided (two runs whose run
numbers are 100 apart on one day, or a manual upload used a bigger number).
Pass `buildNumber` explicitly.

**`Firebase is not configured` in the app log from a CI build** — the plist
secret for that backend is unset; the workflow warned about it at the
*Stage the Firebase config* step. Push is inert, everything else works.

**npm ci fails resolving `homerun-app-ui`, or cargo fails on a repository you
do not recognise** — the keychain GitHub credential expired or was revoked.
`printf 'protocol=https\nhost=github.com\n\n' | git credential fill` on the
runner shows which account it is; refresh it with `gh auth login` as that
account.

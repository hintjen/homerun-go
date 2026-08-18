# The Minecraft account on mobile

Which Minecraft player this phone belongs to, and the two independent ways it
finds out.

Both hosts implement this. Android arrived first (host revision 8) and iOS
followed (revision 10); they run the same flow against the same core, so
everything below is true of both unless it names a platform.

Minigame stats are keyed on a Minecraft **uuid**. Every read of them takes one
as input — the player profile, the leaderboard's "You" row — and the only way to
obtain one was a Microsoft sign-in no mobile host could perform. So the phone's
Minigames Hub was not broken; it was structurally unable to show anybody their
own numbers, for ever.

Two things fixed that, and they are worth keeping distinct because they fail
differently and most users only ever meet the first:

| | Where the uuid comes from | Needs |
|---|---|---|
| **The link** | The API, from an account already linked on the desktop app | A Homerun login, nothing else |
| **The sign-in** | A Microsoft OAuth flow on the phone | `minecraftAccount` capability |

## 1. The link — no sign-in at all

`GET /api/minecraft-account/` answers "which Minecraft account is mine" from the
JWT. That endpoint had to be added: every other read on the API is keyed on a
uuid, which quietly assumes the caller has just finished a sign-in and is
holding one. A phone has not, and the link was almost certainly made on a
different machine months ago.

The UI reads it in `useMinecraftAccount` and exposes it as `linkedAccount`, kept
separate from `credentials` because it is **an identity, not a credential**:
there is no token behind it and nothing to sign out of, and a surface that
offered to would be offering something it cannot do. `account` is the derived
"whichever we have", and it is what every display surface uses.

This is the path most users take. It needs no host capability and no Microsoft
round trip.

## 2. The sign-in — OAuth device code

`minecraft:auth:get-profile` / `:login` / `:logout`, plus the `:ready` and
`:signed-out` events. No new channels: all five already existed in `bridge/v1`
and were re-tiered onto the `minecraftAccount` capability, so this was a host
learning to answer what the contract already described. Host revision 8.

`MinecraftAuth.kt` and `MinecraftAuth.swift` perform the calls; every request
body, response shape and error message is `homerun_core::minecraft::account`,
because the chain is five calls deep with a documented trap at nearly every
step and both hosts have to make the same calls. What differs between them is
only what only they can do:

| | Android | iOS |
|---|---|---|
| Session storage | `SecretStore` — SharedPreferences under a Keystore AES-GCM envelope | Keychain, via `TokenStore.minecraftSession` |
| Transport | `HttpURLConnection` on `Dispatchers.IO` | `URLSession`, inside an `actor` |
| Opening the approval page | `ACTION_VIEW` intent | `UIApplication.open`, hopped to the main actor |

### Why iOS does not use ASWebAuthenticationSession

It uses one for `auth:web-session` and not for this, which reads as an
inconsistency until you notice they are different flows. That channel runs a
*redirect* and has to capture a callback URL, which is what
`ASWebAuthenticationSession` exists for. Device code has no callback at all:
the browser is only where the user approves, and the answer arrives on a poll
over a separate connection. An auth session would put a "Sign In" consent
prompt in front of the user for nothing, and then sit there with nothing to
close it.

### Why device code and not a redirect

The desktop signs in with the **public Xbox client id** `00000000402b5328` —
public, secretless, and already approved for the Minecraft API, which is the
property that matters. Its only registered redirect is a Microsoft-hosted page
the desktop watches a `BrowserWindow` navigate to.

A phone cannot watch that. Intercepting a redirect to a domain we do not own
needs an App Link we cannot verify, and the alternative — an embedded WebView —
is what Microsoft asks people not to do and takes the user's existing session
away from them.

Device code needs none of it: ask for a short code, open the user's real browser
at `microsoft.com/link?otc=<code>` with the code already filled in, and poll.

### The two failure modes that are not failures

Both were found on a real device and both look like a broken sign-in:

- **A poll that returns HTTP 400.** `authorization_pending` and `slow_down`
  arrive that way. A host that read the status instead of the body would report
  a working sign-in as broken, once every five seconds.
- **A poll that does not complete at all.** While the browser is in front, the
  app is a cached process and Android cuts its DNS — polls fail with
  `No address associated with hostname`. The first version treated one failed
  poll as a failed sign-in, so switching to the browser ended the sign-in the
  user was halfway through. They approved successfully and came back to the
  button they had already pressed.

So a failed poll is retried rather than fatal, and requests retry the transport
three times. Only an answer, the code expiring, or ~4 minutes of solid silence
ends it. **A consequence worth knowing: the sign-in completes when the user
returns to the app, not the instant they approve.** That is inherent to Android
restricting a backgrounded app's network, and the flow is built around it.

### No token reaches the WebView

`Session::redacted` is the only shape allowed across the bridge. The contract's
`accessToken` / `refreshToken` fields exist for the desktop's client launcher,
which actually starts a game; no phone surface reads any of them. They go over
as `"0"` — the same placeholder the desktop uses for offline mode — and the real
tokens stay in [`SecretStore`](../android/app/src/main/java/app/gethomerun/mobile/SecretStore.kt).
A test asserts the real values cannot appear in the serialized output.

---

## Using our own Azure app instead

Not required, and not on anyone's critical path. It buys exactly two things:
**our name on the consent screen** instead of "Minecraft Launcher", and a
one-tap redirect instead of a device code. No new capability.

An Azure registration alone is not enough — `api.minecraftservices.com` returns
**403** until Microsoft approves the client id behind it. Registration and
approval are both free; the cost is waiting, and applications sometimes go
unanswered.

### Registering the app

`portal.azure.com` → Microsoft Entra ID → App registrations → New registration.
Register it under the **company** account: the client id becomes a production
credential for the desktop app and both phone platforms.

| Field | Value | Why |
|---|---|---|
| Name | `Homerun` | Users read this on the consent screen. It is the string that replaces "Minecraft Launcher". |
| Supported account types | **Personal Microsoft accounts only** | Players sign in with personal accounts. An organisational option fails later with errors that never mention this screen — the Xbox Live scope must be requested against the consumers tenant, and a tenant id or the `common` endpoint simply errors. |
| Redirect URI | leave blank | Added below, where the platform type can be set. |

Copy the **Application (client) ID** off the overview blade. That GUID is what
gets submitted, and what goes in the code afterwards.

### Redirect URIs

Authentication → Add a platform → **Mobile and desktop applications**. The blade
offers three checkboxes and a free-text field; only one needs anything typed.

| Option | Do | Why |
|---|---|---|
| *(text field)* `homerun://auth/minecraft` | **Add** | Android and iOS. The scheme is already declared in `AndroidManifest.xml`, and `BridgeRouter` already routes `homerun://auth/` separately from product deep links — plumbing from the social-login work. |
| `login.live.com/oauth20_desktop.srf (LiveSDK)` | **Keep checked** | Azure pre-checks it. It is exactly the redirect the desktop Electron app already navigates to and watches for, so the desktop switch stays "change the client id". |
| `…/common/oauth2/nativeclient` | Leave | The v1-era equivalent of the LiveSDK entry. No reason to carry both. |
| `msal<guid>://auth` | Leave | For apps that let Microsoft's MSAL library run the OAuth. Ours is hand-rolled, so MSAL never runs and nothing would arrive there. |

No loopback URI is needed.

### Two more settings

- **Allow public client flows → Yes** (Authentication → Advanced settings). A
  secret shipped in an app binary is not a secret, and this toggle is also what
  enables the device code grant.
- **Add no API permissions.** `XboxLive.signin` does not appear under "Add a
  permission" — it is attached to the client id by the Minecraft review, not by
  us. A correctly configured app with the right Graph permissions still returns
  403 until the review lands, which is the single most common way this is
  misdiagnosed.

### The review

The form is <https://aka.ms/mce-reviewappid>. Framing that matters for Homerun
specifically:

- **Lead with the desktop launcher.** It genuinely starts Minecraft on the
  user's machine — the conventional case reviewers are used to approving. The
  phone's use is identity only, which alone is a thinner story.
- **Say it is commercial, up front.** Homerun sells hosting. Reviewers care, and
  discovering it themselves goes worse than being told.
- **Be specific about scope.** Sign-in identifies the player and launches the
  game; tokens stay in platform keystores on the device.
- **Note the brand position.** The product is not named after Minecraft and does
  not present itself as a Mojang product.

Expect weeks to months, and up to 24 hours for an approval to take effect.

### What changes in the code

Less than the whole chain, more than just swapping a constant — the endpoint
host changes too:

| | Today (public Xbox client) | With an approved registration |
|---|---|---|
| Authorize | `login.live.com/oauth20_connect.srf` | `login.microsoftonline.com/consumers/oauth2/v2.0/authorize` |
| Token | `login.live.com/oauth20_token.srf` | `login.microsoftonline.com/consumers/oauth2/v2.0/token` |
| Client id | `00000000402b5328` | the registration's GUID |

- `AUTH_HOST` in `minecraft/account.rs` becomes configurable rather than a
  private constant.
- The scope is unchanged: `XboxLive.signin offline_access`.
- Everything after the first token — Xbox Live, XSTS, `login_with_xbox`, the
  profile fetch — is untouched.
- `authorize_url` and `redeem_request` already exist in the core with tests, and
  are unused by the shipping path.

Microsoft has moved this process more than once. Where the form disagrees with
anything here, the form wins.

## Where the pieces are

| | |
|---|---|
| `rust/homerun-core/src/minecraft/account.rs` | The whole chain as pure functions, plus the XSTS refusal messages |
| `android/.../MinecraftAuth.kt` | Android: performs the calls, polls, stores the session |
| `android/.../BridgeRouter.kt` | Android: the three invokes and two events |
| `ios/HomerunHost/MinecraftAuth.swift` | iOS: the same, as an actor over `URLSession` |
| `ios/HomerunHost/BridgeRouter+Session.swift` | iOS: the three invokes and two events |
| `ios/HomerunHost/FFI/Core.swift` | iOS: the `minecraft.account.*` core wrappers |
| `homerun-app-ui/hooks/useMinecraftAccount.ts` | `credentials`, `linkedAccount`, `account`, `canSignIn` |
| `homerun-app-ui/docs/minecraft-account-mobile.md` | The cross-repo brief this was built from |
| `api/docs/minigame-stats.md` | `MicrosoftAccount`, and what linking inherits |

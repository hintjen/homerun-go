# Sign in with Google and Apple

## Overview

A player taps *Continue with Google*, a browser opens, and they come back
signed in. This documents the mobile half of that: one bridge channel and its
two host implementations.

The subsystem exists because of a single external constraint. **Google refuses
to authenticate inside an embedded WebView** — requests whose user agent is
`android.webkit.WebView` or `WKWebView` are rejected with
`disallowed_useragent`, enforced since 2023. Both hosts are exactly those two
classes. The sign-in therefore *cannot* happen in our WebView, and the host
has to open a real browser on the page's behalf and tell it where the browser
landed.

Keycloak brokers both providers, so what comes back is an ordinary Keycloak
token for the `homerun` client — indistinguishable at the API from a
magic-link login. That is why the API needed a new endpoint but no new
authentication path, and why the hosts need no provider-specific code at all.

## The split — why the host owns almost nothing

The host's entire job is *open this URL in a real browser, tell me where it
landed*. Everything else — the PKCE verifier and challenge, the authorize URL,
`state`, the code exchange — lives in the shared UI's `lib/socialAuth.ts`.

That line is deliberate and worth defending. The OAuth logic is identical on
every platform, so putting it in the hosts would mean writing it three times,
in Swift, Kotlin and Electron, and testing it in none of them. Putting it in
the UI means one implementation, one test suite, and a fix that ships over the
air. The hosts hold only the part that genuinely differs: which API opens a
browser and how the redirect comes home.

A useful consequence: a provider can be added or reconfigured entirely in
Keycloak and the UI. Neither host mentions Google or Apple anywhere.

```
UI: "Continue with Google"
 ├─ builds the authorize URL, PKCE S256, kc_idp_hint=google   (lib/socialAuth.ts)
 ├─ invoke auth:web-session { url, callbackScheme }           ← the only host call
 │   └─ HOST opens a browser
 │       └─ Keycloak → Google → Keycloak
 │           first broker login: create, or link by verified email
 │           └─ redirect homerun://auth/callback?code=…
 │   └─ HOST captures it, returns { success, callbackUrl }
 ├─ checks `state`, exchanges code + verifier at Keycloak directly
 ├─ POST /api/register/social/ { access_token, client_nonce }
 └─ polls register/token/ for the Matrix triple, then composes the session
```

The code exchange goes straight to Keycloak rather than through our API: the
`homerun` client is public and holds no secret, the UI already refreshes
against Keycloak, and an API hop would put our backend in the middle of a flow
it has no part in.

## `auth:web-session` — the channel

```ts
"auth:web-session": {
  params: { url: string; callbackScheme: string };
  result:
    | { success: true;  callbackUrl: string }
    | { success: false; error: string; canceled?: boolean };
}
```

Added at **host revision 7**, recorded in
[`shared/conformance/host-revisions.json`](../shared/conformance/host-revisions.json).
The revision bump is mandatory, not bookkeeping — an over-the-air bundle that
calls a channel an older host has never heard of hangs a UI promise for ever,
which is this protocol's worst failure mode. See
[`ota-bundles.md`](./ota-bundles.md).

`canceled` is a separate flag rather than an error string because dismissing a
sign-in sheet is an ordinary outcome, not a failure. The UI shows nothing at
all for it; every other `success: false` gets its `error` put in front of the
player.

**This call has no timeout**, in keeping with the host rule that nothing does.
A sign-in legitimately takes minutes — a password manager, a 2FA prompt on
another device, a forgotten password. It ends when the callback arrives, when
the user backs out, or when the page dies.

## Android — `BridgeRouter.kt`

### Opening the browser

`openInBrowser` fires a plain `ACTION_VIEW` carrying the two Custom Tabs
extras **set literally, by string key**:

```kotlin
putExtra("android.support.customtabs.extra.SESSION", null as android.os.Bundle?)
putExtra("android.support.customtabs.extra.TITLE_VISIBILITY", 1)
```

`androidx.browser` is not on this host's classpath and this avoids adding it.
A browser that understands the extras opens a Custom Tab; one that does not
opens a normal tab and the flow still works. If `startActivity` throws, it
falls back to `openExternal` and only then reports failure — a device with no
browser at all is the one case that cannot be served.

### Capturing the callback

On Android a Custom Tab redirect **is** an Intent, so it lands in the same
deep-link plumbing as invite and join links. `deliverDeepLink` therefore
checks for an auth callback *first*:

```kotlin
fun deliverDeepLink(url: String) {
    if (completeAuthSession(url)) return
    emit("deep-link", listOf(JsonPrimitive(url)))
}
```

**The order is load-bearing.** `lib/deepLink.ts` returns `null` for intents it
does not recognise, so an auth callback emitted as a normal deep link would be
parsed, rejected and dropped in silence while the sign-in call waited for
ever. The `homerun://auth/` prefix (`AUTH_CALLBACK_PREFIX`) keeps auth clear
of the `join`/`server`/`play`/`web-share-join` namespace so the two can never
be confused.

### One session at a time

`pendingAuthSession` is an `AtomicReference<CompletableDeferred<String>?>`.
There is one browser and one user, so a second `auth:web-session` while one is
outstanding cancels the first — its callback could never be routed anywhere
now that this one owns the slot.

### Cancellation, and the 700 ms grace

**A dismissed Custom Tab reports nothing at all.** There is no cancel
callback, no result code, no lifecycle event that distinguishes it. The only
evidence a user backed out is that we are visible again with a session still
outstanding — which is why `MainActivity.onResume` calls
`router.onForegrounded()`.

The complication is that returning *via the callback* also resumes the
activity. So `onForegrounded` waits `AUTH_CANCEL_GRACE_MS` (700 ms) and only
declares the session dismissed if nothing has claimed it by then, using
`compareAndSet` so a callback arriving inside the window wins the race
outright rather than being merely likely to.

Too short and a real sign-in reports itself cancelled on a slow device; too
long and a user who backed out stares at a spinner. 700 ms is the compromise,
and it is the number to reach for if either symptom appears.

## Android — `MainActivity.kt`

One line, `router.onForegrounded()` in `onResume`, and the whole cancel path
depends on it. Removing it does not break the happy path — a successful
sign-in still completes — so nothing obvious fails. What breaks is a user who
backs out of the browser: their session never resolves, the button spins for
ever, and the only way out is force-stopping the app.

## iOS — `BridgeRouter+AppShell.swift`

iOS uses `ASWebAuthenticationSession`, which is a better fit than Android's
arrangement in one specific way: **it captures its own callback in a
completion handler without involving the OS URL router.** The iOS deep-link
path never sees an auth callback, so the comment there saying auth does not
come through it stays true.

Three details that break silently if changed:

**`_ = session` inside the completion handler.** The session is captured only
to stop ARC releasing it mid-flight. Without it the sheet closes on its own,
part-way through, and the user sees the sign-in vanish with no error.

**`prefersEphemeralWebBrowserSession = false`.** This shares Safari's cookie
jar. Set it to `true` and the user is asked to sign in to Google again even
when Safari already knows them — which is most of the value of not using a
WebView in the first place.

**`AuthPresentationAnchor` is file-scope, not a property.** Swift extensions
cannot add stored properties, and the router's conformance lives in an
extension. It holds no state worth isolating; every sign-in presents from the
same key window.

Cancellation needs no grace period here: `ASWebAuthenticationSessionError`
with `.canceledLogin` says so explicitly, which is the part Android has to
infer.

## PKCE, and why the digest is computed in JavaScript

`codeChallengeFor` in the UI hashes with `@noble/hashes` rather than
`crypto.subtle`. The reason is a platform constraint that belongs in this doc
even though the code does not live in this repo:

**`crypto.subtle` exists only in a secure context, and WKWebView does not
treat a custom scheme as one.** iOS serves the bundle from `homerun-app://`,
so `subtle` is `undefined` there and S256 was simply impossible. Apple's
escape hatch is the private `_registerURLSchemeAsSecure`, which is not
something to ship to an app store.

The alternative was `plain` PKCE, which against a custom-scheme redirect is
the exact interception hole PKCE exists to close — so it was never one.

It is deliberately *not* "use `subtle` when available, fall back otherwise":
that is two code paths where the untested one runs on the platform that needs
it. Android serves over an `https://` virtual host and would be fine either
way, but runs the same code, which is what makes the Android test meaningful
evidence for iOS. `getRandomValues` is unaffected — only `subtle` is gated.

Verified on a device: Keycloak independently computed the same digest from a
verifier generated in the WebView. A mismatch surfaces at the token exchange
as `invalid_grant`, with nothing before that point hinting at it.

## What Keycloak must already be true

The hosts assume realm state they cannot check. All of it is on the `homerun`
client or the provider, and all of it is recorded with its verification in
[`../plans/social-login.md`](../plans/social-login.md):

| Setting | Value | Why |
|---|---|---|
| Valid redirect URI | `homerun://auth/callback` | else the authorize request 400s before a browser ever opens |
| PKCE challenge method | `S256` | a public client with a custom-scheme redirect is interceptable without it |
| Provider alias | `google`, `apple` | `kc_idp_hint` is a hardcoded constant in the UI; a mismatch silently lands on Keycloak's own login page |
| Trust Email | on | removes Keycloak's own verification mail; both providers verify |
| First broker login flow | the silent duplicate | otherwise an existing email gets an *"account already exists"* interstitial |

## File map

| File | Role |
|---|---|
| `android/.../BridgeRouter.kt` | the handler, `pendingAuthSession`, `completeAuthSession`, `onForegrounded`, `openInBrowser` |
| `android/.../MainActivity.kt` | `router.onForegrounded()` in `onResume` — the whole cancel path |
| `ios/HomerunHost/BridgeRouter+AppShell.swift` | `authWebSession`, `AuthPresentationAnchor` |
| `ios/HomerunHost/BridgeRouter.swift` | dispatch entry, `hostRevision` |
| `shared/conformance/host-revisions.json` | revision 7 ledger entry |
| `lib/socialAuth.ts` *(UI repo)* | PKCE, authorize URL, callback parsing, code exchange |
| `lib/bridge/channels.ts` *(UI repo)* | the channel contract |

## Triage

**The sign-in spins for ever after the browser closes.** The callback was not
routed to the waiting session. On Android, check `AUTH_CALLBACK_PREFIX` still
matches the `redirect_uri` the UI sends, and that `deliverDeepLink` tries
`completeAuthSession` *before* emitting. `HomerunBridge: auth:web-session ->
callback received` is the line that proves capture.

**An "Open with" chooser appears listing two identical "Homerun Go" entries.**
Both the release and debug builds are installed and both claim `homerun://`.
Picking wrong hands the callback to an app with no pending session, which
drops it in silence. `pm disable-user --user 0 app.gethomerun.mobile` for
testing; see [the emulator skill](../.claude/skills/android-emulator/SKILL.md).

**A sign-in reports itself cancelled although the user completed it.**
`AUTH_CANCEL_GRACE_MS` elapsed before the redirect arrived — a slow device or
a slow network. Raise it.

**Backing out of the browser leaves the button spinning.**
`router.onForegrounded()` is not being called from `onResume`.

**The exchange fails with `invalid_grant`.** The verifier and challenge
disagree. Almost always the digest: check `codeChallengeFor` against the RFC
7636 vector in `lib/__tests__/socialAuth.test.ts`.

**Google shows `disallowed_useragent`.** The URL opened in the WebView rather
than a browser — `openInBrowser` fell through, or the UI navigated instead of
invoking the channel.

**The social buttons are missing after a clean build and install.** Not this
subsystem: an over-the-air bundle in `files/ui/current` outranks the APK's
assets. `adb logcat -d -s HomerunBundle:*` says which is being served. See
[`ota-bundles.md`](./ota-bundles.md).

**iOS: the sheet opens and closes immediately.** The `ASWebAuthenticationSession`
was released mid-flight — the `_ = session` capture is gone.

**iOS: the user is asked to sign in to Google every time.**
`prefersEphemeralWebBrowserSession` is `true`.

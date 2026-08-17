# Homerun host bridge — `bridge/v1`

The contract between the **shared Homerun UI** (a static web bundle) and
whatever **host** embeds it: the Electron main process today, the iOS and
Android WebView hosts next.

Normative sources, in precedence order:

1. `channels.ts` — the typed channel inventory (params + results + event
   payloads). **The types are the contract.**
2. `requirements.ts` — which channels a host must implement, given its
   capabilities.
3. This document — the wire protocol and lifecycle rules.

If this document and `channels.ts` disagree, `channels.ts` wins.

---

## 1. Operations

Three, and only three:

| Operation | Direction | Correlated | Shape |
|---|---|---|---|
| **invoke** | UI → host → UI | yes (`id`) | request with one payload, one response |
| **send** | UI → host | no | fire-and-forget, one payload |
| **event** | host → UI | no | broadcast, N positional args |

There is no host→UI request: the host can never call the UI and await an
answer. Where the desktop app needs that (`get-api-url`), the host emits
an **event** and the UI answers with a **send**. Keep that pattern.

## 2. Envelopes (WebView hosts)

JSON objects. Electron does not use these literally — see §3.1.

**UI → host**

```jsonc
// invoke
{ "v": 1, "id": "42", "method": "native-server-start", "params": { "serverId": "s1", "config": { "name": "My Server", "memoryMb": 2048 } } }
// send (no id)
{ "v": 1, "method": "minimize-window", "params": null }
```

**host → UI**

```jsonc
// success
{ "v": 1, "id": "42", "result": { "success": true, "alreadyRunning": false, "aborted": false } }
// failure
{ "v": 1, "id": "42", "error": { "message": "port 25565 already in use", "code": "PORT_IN_USE" } }
// event — args is ALWAYS an array (some v1 events are multi-arg)
{ "v": 1, "event": "native-server-log", "args": [ { "serverId": "s1", "line": "Done (3.2s)!" } ] }
```

Rules:

- `id` is an opaque string chosen by the UI. Echo it exactly. Never
  invent one; never reuse one.
- `params` is **one value or null** — never a positional array. (The
  desktop preload only ever forwarded one argument; the contract is
  written to that reality.)
- `args` is **always an array**, even for the single-payload majority.
  Its length and order must match the event's tuple in `channels.ts`.
- `v` is the protocol major version. A host receiving an unknown `v`
  must respond with an error, not guess.
- Unknown `method` → respond with an error. Never drop it silently: a
  dropped invoke hangs a UI promise forever.

## 3. Transport bindings

### 3.1 Electron

The envelope is implicit: `ipcRenderer.invoke(channel, payload)` already
*is* request/response, and `webContents.send(channel, ...args)` already
is an event. The UI's `BridgeTransport` maps straight onto it. No JSON
envelope is constructed. This binding already ships.

### 3.2 iOS (WKWebView)

- UI → host: `window.webkit.messageHandlers.homerun.postMessage(envelope)`
- host → UI: `webView.evaluateJavaScript("window.__homerunHost.receive(<json>)")`

`<json>` must be a **single JSON literal**, produced by a serializer —
never string interpolation of user data. Escape `U+2028` and `U+2029`
(legal in JSON, fatal in JavaScript source).

Serve the bundle from a custom scheme (`homerun-app://`), not `file://`.
Vite/Next emit `<script type="module" crossorigin>`, and module fetches
from an opaque `file://` origin fail silently — you get a blank page.

### 3.3 Android (WebView)

- UI → host: `HomerunHost.postMessage(String json)` via
  `addJavascriptInterface`
- host → UI: `webView.evaluateJavascript("window.__homerunHost.receive(" + json + ")", null)`

Same single-literal and escaping rules. Serve via `WebViewAssetLoader`.
`addJavascriptInterface` methods run on a **binder thread** — hop to the
main thread before touching the WebView, and never block the binder
thread on server work.

## 4. Bootstrap and lifecycle

### 4.1 Capabilities are injected before the app runs

The UI resolves capabilities **once, synchronously, at startup** — it
cannot await them. The host must define, at document-start (iOS:
`WKUserScript` at `.atDocumentStart`; Android:
`WebViewClient.onPageStarted` / a document-start script):

```js
window.__homerunCapabilities = { platform: "ios", serverBackends: ["pumpkin"], /* …every field in HostCapabilities… */ };
```

Every field in `HostCapabilities` must be present. Missing fields are a
host bug, not a default.

**With one qualification, because capabilities are additive.** A host built
before a field existed cannot send it, so every new field needs a defined
meaning for *absent*, chosen so that an older host stays correct:

| Kind of field | Absent must mean | Why |
|---|---|---|
| A boolean that reveals a feature | `false` | The surface hides. `minigames` and `nativeShare` both rely on this. |
| An allowlist that *narrows* something | **no narrowing** | The host supports what it always did. Filtering on a list it never sent would retract working features. `serverLoaders` is this. |

`serverLoaders` is worth naming because it is the second kind and the two read
in opposite directions. It lists which Minecraft loaders the host can host, and
absent means show them all, while `[]` is a real answer meaning none. Getting
that backwards would have hidden every loader from every host older than the
field — a worse bug than the one it was added to fix, which was Android offering
Spigot and then refusing it at launch.

So: a *new* host must send every field. A *reader* must still define what
absence means, and the type says which fields can be absent.

### 4.2 The `ready` handshake

Events emitted before the page's JS is listening are lost. So:

1. Host starts with an internal event queue; everything emitted goes
   into it.
2. UI calls `send("__bridge:ready")` once its subscriptions are wired.
3. Host flushes the queue in order, then switches to direct delivery.

`__bridge:ready` is protocol-level, not a product channel — it is
deliberately absent from `channels.ts`.

### 4.3 The WebView process can die

On iOS, `webViewWebContentProcessDidTerminate` fires when the content
process is jetsammed — likely on a phone also running a Minecraft
server. Android can destroy and recreate the WebView too.

Required behavior:

- Reload the page.
- **Re-arm the queue** and wait for a fresh `ready` — the new page has
  none of the old page's state.
- Fail every pending invoke for the dead page (see §5), so no promise
  outlives the page that created it.
- Keep **zero** per-page state in the host beyond that queue. A reload
  must fully resynchronise from `invoke` calls the new page makes.

### 4.4 Backgrounding

iOS suspends JS timers and, eventually, the app. A host that cannot keep
the server running in the background declares
`backgroundExecution: false`; the UI is responsible for saying so to the
user. The host must still deliver a coherent state on resume — emit
`native-server-state-changed` for anything that changed while suspended
rather than letting the UI's cached state rot.

## 5. Errors, timeouts, and long calls

- Every invoke gets exactly one response. A host that neither resolves
  nor rejects is the single worst failure mode in this protocol.
- Errors carry a human-readable `message`; `code` is optional and
  machine-readable. The UI surfaces `message` directly, so write it for
  a player, not a log.
- **There is no blanket call timeout.** Several v1 invokes legitimately
  run for minutes (`native-server-start`, `minecraft:install`,
  `move-installation`, `import-minecraft-modpack`). A fixed timeout — the
  iOS prototype used 10s — would break them. Long operations report
  progress via events and resolve when genuinely done.
- Pending calls are cleared only on page teardown (§4.3), by rejecting
  them.
- A capability-gated channel that gets called anyway (a UI bug, or a
  host that declared a capability it lacks) must return an error. Never
  a silent success.

## 6. What a host must implement

`requirements.ts` maps every channel to one of:

| tier | meaning |
|---|---|
| `core` | implement always |
| `capability` | implement iff you declare that capability |
| `backend` | implement iff you run that server backend |
| `desktop-only` | Electron only; mobile may error |

`desktop-only` entries flagged `ungated: true` are reachable from UI that
is **not yet capability-gated** (Discord surfaces, the firewall port
toggle). Until that gating lands, a mobile host should return a benign
response rather than an error — the per-channel `note` says which.

The `native-server-*` family is the **local server backend interface**.
The name is desktop-legacy; on mobile it is Pumpkin or the Android JVM
backend. Implementing that family plus the `core` tier is what makes a
host able to host a server.

## 7. Versioning

Within v1, **additive only**:

- new channels — fine
- new **optional** fields on params/results/events — fine
- new capabilities, defaulting to off — fine

Breaking changes — renaming or removing a channel, changing a field's
type, making an optional field required, changing an event's arity —
require `v: 2`. Hosts must reject envelopes whose `v` they do not
implement.

Because three repositories depend on these strings, treat a rename as a
breaking change even when it looks cosmetic.

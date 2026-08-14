# iOS Bridge

## Overview

The bridge is how the shared UI talks to the device. It implements the WebView
half of `bridge/v1` — the contract in
[`../shared/conformance/PROTOCOL.md`](../shared/conformance/PROTOCOL.md), whose
client half is `webviewTransport.ts` in the UI repo and is shared verbatim with
Android.

Three operations exist, and no others: the UI **invokes** and gets exactly one
answer, the UI **sends** and gets nothing, the host **emits** events. There is
no host-to-UI request.

The single worst thing this layer can do is fail to answer an invoke. The UI
promise never settles, and the screen simply stops — no error, no spinner
state, nothing in a log. Everything below that looks defensive is really about
that one failure mode.

## Transport — `ios/HomerunHost/BridgeController.swift`

UI to host is a `WKScriptMessageHandler` named `homerun`. WKWebView
structured-clones the posted envelope, so `message.body` arrives as a
dictionary and no string parsing is involved. (Registering that handler is also
what makes the UI *choose* the WebView transport — it checks for
`window.webkit.messageHandlers.homerun`.)

Host to UI is `evaluateJavaScript("window.__homerunHost.receive(<json>)")`.

> **Load-bearing: `<json>` is one JSON literal from a serializer, and U+2028 /
> U+2029 are escaped.** Both characters are legal inside a JSON string and
> neither is legal inside a JavaScript source line. A world name or chat
> message containing one turns the injected call into a syntax error, and the
> reply never arrives — the hanging-promise failure, triggered by data. All
> encoding goes through `BridgeEnvelope.jsLiteral`, which is the only place a
> reply string is built.

Everything in the controller runs on the main thread. Message-handler
callbacks arrive there, and every WebKit API it touches requires it, so the
class is `@MainActor`; handlers that need to work elsewhere hop off it
themselves.

### The weak message-handler proxy — `WeakScriptMessageHandler.swift`

`WKUserContentController` retains its handlers **strongly**. The controller is
owned by the configuration, the configuration by the WebView, and the handler
owns the WebView — so registering `self` leaks the whole graph, including the
content process, for the life of the app. On a phone that is also running a
Minecraft server, that is memory nobody can spare.

The proxy holds the real handler weakly and forwards to it. It is three lines
and it is not optional.

### Encoding — `BridgeEnvelope.swift`

`JSONSerialization`, not `Codable`. `params` and `result` are arbitrary JSON by
design; modelling them with `Codable` needs a wrapper enum and a boxing
round-trip at every hop to describe values the host mostly passes straight
through.

Envelope shapes are in PROTOCOL.md §2. Two details worth repeating because
getting them wrong is quiet rather than loud:

- `params` is **one value or null** — never a positional array.
- `args` on an event is **always an array**, matching the event's tuple, even
  when there is one payload.

## The ready handshake and the event queue

Events emitted before the page's listeners exist are lost. So the host starts
in a queuing state, and the UI calls `send("__bridge:ready")` once its
subscriptions are wired; the host then flushes in order and switches to direct
delivery (PROTOCOL.md §4.2).

`__bridge:ready` is protocol-level and deliberately absent from `channels.ts`.
It is handled in `BridgeController`, **not** in the router's table — putting it
there would declare it as a channel to the conformance checker.

## Process-death recovery

> **Load-bearing, and not a defensive extra.** The WebView content process is
> killed under memory pressure, and on this device the memory-hungry thing is
> the server the user is deliberately running. This path executes in normal
> operation.

On `webViewWebContentProcessDidTerminate` (and on any main-frame navigation
start, so a manual reload behaves identically) the host:

1. bumps a **generation** counter,
2. cancels and clears every pending invoke,
3. re-arms the event queue,
4. reloads.

The host keeps **no other per-page state**. That is what lets a reload fully
resynchronise: the fresh page re-invokes for everything it needs.

The generation counter exists for a specific race. The UI's call ids restart at
1 with each page, so a reply from a call made by the *old* page would resolve
an unrelated promise in the new one, with someone else's data. Replies carry
the generation they were issued under and are dropped if it no longer matches.

## No call timeout

> **Load-bearing.** There is no blanket timeout anywhere in this layer.

`native-server-start` and `import-minecraft-world` legitimately run for
minutes. The prototype's 10-second timeout would break both. Pending calls are
cleared when the **page** dies, not on a clock — that is the only event that
actually means the caller is gone.

## Dispatch and the channel table — `BridgeRouter.swift`

`shared/conformance/check-coverage.js` reads the block between the
`BRIDGE-CHANNELS-BEGIN` / `BRIDGE-CHANNELS-END` markers and counts a channel
only where the string sits in **declaration position** — `"channel": handler`
in Swift, `"channel" to handler` in Kotlin. It does not parse either language,
which is what lets one script check both.

Two rules follow, and both are about keeping the gate honest:

- **Keep the table a dictionary literal of method references.** The block is
  the dispatch table itself, so it contains handler bodies too; matching every
  quoted string would let a literal buried in a body pass as a handler for a
  channel nobody implemented. A router that registers channels some other way
  reads as having *no* handlers — the safe direction to fail, but it stops CI
  dead until the table is put back in the expected shape.
- **Do not park unimplemented channels there** pointing at a stub that throws.
  That turns the gate green while leaving the work undone. The failing list
  from `npm run conformance:ios` *is* the to-do list.

An unknown method still gets an answer — an error, never silence:

```
{ "v":1, "id":"42", "error": { "message":"Homerun for iOS cannot do that yet (…).",
                               "code":"UNKNOWN_METHOD" } }
```

A visible failure beats a frozen screen.

### Errors are player-facing

`error.message` is rendered in the app. Write it for someone who wants to play
Minecraft: "Another server is already running" — not "EADDRINUSE". `BridgeError`
carries the message and an optional machine-readable `code`; mapping happens in
one place in `BridgeController`.

## JavaScript diagnostics

Two things WKWebView does silently, both handled in `BridgeController`:

- **Uncaught JS errors are invisible from the native side.** A bundle that
  fails to boot looks exactly like a bridge that is broken. A document-start
  user script forwards `error` and `unhandledrejection` to the host, which logs
  them.
- **`alert`, `confirm` and `prompt` are dropped** unless the UI delegate
  implements them — the call never returns to the page. `WKUIDelegate` backs
  them with `UIAlertController`.

## Haptics — `HapticsPlayer.swift`

The `haptic` send carries a word for what the user just did — `selection`,
`navigate`, `commit`, `success`, `warning`, `error` — never an instruction for
the Taptic Engine. The page cannot play these itself: WKWebView does not
implement `navigator.vibrate` at all, so without this channel the phone is
silent.

The mapping from those six words to generators is
`homerun-app-ui/lib/haptics.ts` (`HAPTIC_MAPPINGS`), which names itself the
specification for this host. `docs/style.md` §16 there says which surfaces may
send which, and the UI rate-limits to one every 50 ms *before* sending, so this
host does not debounce.

- **The generators are held, not made per call.** Creating a
  `UIFeedbackGenerator` warms the engine, and paying that spin-up at the instant
  of the tap is latency on the one thing whose whole value is arriving *with*
  the touch. Each play re-`prepare()`s for the next.
- **An unknown pattern is dropped, not raised.** `bridge/v1` is additive, so a
  seventh pattern has to reach an older host as silence. Throwing would be
  invisible anyway — a send has no `id`, so `respond` returns before the error
  reaches anyone.
- **Nothing here needs a view.** That is why it is a `@MainActor enum` called
  straight from the handler rather than routed through `BridgeEventSink`: that
  protocol exists for the things that need the WebView.

## Channel map

Extended at M2 as handlers land. Today: `get-app-version`.

## File map

| File | Role |
|---|---|
| `ios/HomerunHost/BridgeController.swift` | Transport, queue, handshake, recovery, error mapping |
| `ios/HomerunHost/BridgeRouter.swift` | The channel table CI reads, and the handlers |
| `ios/HomerunHost/BridgeEnvelope.swift` | Envelope encode/decode, the U+2028/9 escaper |
| `ios/HomerunHost/WeakScriptMessageHandler.swift` | Breaks the message-handler retain cycle |
| `ios/HomerunHost/HapticsPlayer.swift` | The six patterns, and the generators they play on |

## Triage

**A screen is frozen with no error and no spinner.** An invoke went
unanswered. Check the router table for the channel the screen needs — if it is
missing, the handler is missing; if it is present, the handler threw somewhere
that escaped the reply path.

**Replies stop arriving after a while, then the app is killed.** The WebView
leaked because the message handler was registered strongly. Use
`WeakScriptMessageHandler`.

**Everything works until a player uses an unusual character, then that one call
hangs.** U+2028/U+2029 in the payload. The escaping in
`BridgeEnvelope.jsLiteral` was bypassed by building a reply string somewhere
else.

**Events fire but the UI never sees the early ones.** They were emitted before
`__bridge:ready` and the queue was already flushed — or the queue was not
re-armed after a reload. Both are `resetForNewPage`.

**After a content-process kill, a promise resolves with the wrong data.** The
generation check was skipped on that reply path.

**`npm run conformance:ios` passes but the screen still hangs.** Something in
the marker block is not a real handler — a stub, or a stray string literal.

**A long-running start fails after a fixed interval.** Someone added a timeout.
There is none by design.

**Nothing buzzes.** Three innocent explanations before it is a bug, in the order
worth checking: the Simulator has no Taptic Engine and never will; Low Power
Mode silences every generator; and Settings → Sounds & Haptics → System Haptics
is a switch the owner may have turned off. All three are correct behaviour the
host must not route around. If none of them apply, look for
`haptic sent an unknown pattern` in the log — that is the page and the host
disagreeing about the vocabulary, which means the contract needs re-syncing.

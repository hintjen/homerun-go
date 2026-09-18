# Who terminates TLS for the device websocket

## Overview

The dashboard reaches a phone's console over `wss://`, and something has to hold
the certificate for that. Until now it was always the phone: a Let's Encrypt
certificate per device, ordered by the supervisor, behind a gateway that passes
`:443` through untouched. That ran into a wall nobody on the phone could see.
Let's Encrypt allows **fifty new certificates a week per registered domain**,
every device's hostname is under one domain, and new installs passed fifty a
week. A device past the cap gets no certificate, serves no `wss://`, does not
crash and shows the user nothing.

So the gateway learned to terminate TLS itself, with one certificate for its own
hostname, and relay the decrypted websocket to the device over the WireGuard
tunnel it already has. The design, the gateway half and the API half are in the
`homerun` repo at `api/docs/plans/device-websocket-gateway-tls.md`; Homerun
Desktop's half is `deviceWebsocket/wsTlsMode.ts`. This is the phones' half.

**Nothing was removed.** The device-side certificate path is whole and is what a
link runs whenever the API does not say otherwise.

| | Gateway mode | Device mode |
|---|---|---|
| Who holds the certificate | the region's gateway, one for its own hostname | this phone, one per device hostname |
| What the dashboard dials | `wss://ws-<region>.…/d/<device id>` | `wss://<device fqdn>` |
| Tunnel forwards | WG `4000` → the **plaintext** socket | WG `8443` → the TLS listener, WG `8080` → the ACME challenge listener |
| ACME on the phone | never | ordered and renewed by `device_ws/tls.rs` |
| Reachable from the internet | nothing, except through the gateway | the TLS listener, through the gateway's passthrough |

In both modes the phone still validates the caller's Keycloak token as the first
frame and asks the API what that caller may touch. The gateway relays; it
authorises nothing.

## The decision — `rust/homerun-core/src/device_ws/mod.rs`

The API decides per `link_up`. The phone's part is three small things, all in
the core so Kotlin and Swift cannot disagree:

- **`link_up_request()`** is the `POST` body: `{ "ws_tls": "gateway" }`. It says
  this build *can* run in gateway mode. It is a capability, not a demand, and an
  API older than the field ignores it.
- **`TlsMode::from_reported`** reads the answer off the result's `ws_tls`, and is
  **downgrade-only**: exactly `"gateway"` is gateway mode, and `"device"`, any
  other value, a non-string and *no field at all* are device mode. Load-bearing:
  the gateway only relays to port 4000 if the API provisioned that route, so a
  phone that talked itself into gateway mode would bring up a tunnel nothing
  sends to.
- **`DeviceLink::needs_certificate()`** is false in gateway mode *even though the
  link still has an `fqdn`*. Nothing routes that name to the phone any more, so
  an order for it cannot validate — and every attempt would spend the rate limit
  this mode exists to stop spending.

`LISTEN_WS = 4000` is the gateway's number, like `LISTEN_HTTPS` and
`LISTEN_HTTP`: the `svc_port` the API registers for the device's route
(`gateway_provision.DEVICE_WS_PORT`) and the desktop's `DEVICE_WS_TUNNEL_PORT`.
`gateway_tunnel_config` renders the one forward, byte-exact against the
desktop's.

## The socket — `rust/homerun-supervisor/src/device_ws/mod.rs`

`Config.gateway_tls` (`gatewayTls` in the JSON a host passes) switches the
supervisor's socket:

- **Nothing is ordered, whatever else the config carries.** Checked in `start`
  rather than trusted to the host leaving `fqdn` out.
- **No `cert-misconfigured` report.** A named device started without a challenge
  port is normally the host's mistake and is reported as fatal. In gateway mode
  it is correct, and reporting it would file a fatal row for every healthy phone.
  This is the branch most likely to be "simplified" away.
- **The plaintext listener takes the remote connections.** It was "the app's own
  UI, and nothing else"; in gateway mode every dashboard arrives there too,
  already decrypted. `serve` demands a token first either way.
- The TLS listener still binds, so the reply still carries `tlsPort`, and takes
  no connections.

Absent is false, so a host built before the field behaves exactly as before.

## The hosts — `DeviceWebsocket.kt`, `DeviceWebsocket.swift`

Both do the same three things and nothing else: send the core's request body,
skip the challenge port and pass `gatewayTls` when the link says so, and ask the
core for the gateway tunnel config with **`wsTarget` = the plaintext port**.

That last one is the trap, and it is the mirror image of the old one. In device
mode, forwarding at the plaintext port fails every handshake, because a
ClientHello arrives at a websocket server. In gateway mode, forwarding at the
TLS port fails every connection, because a websocket upgrade arrives at a
listener with no certificate, which drops it — and the dashboard sees a 503 from
the gateway. `deviceWs.tunnelConfig` refuses both kinds of target at once for
this reason: a host that sends both does not know which mode it is in.

No bridge channel changed, so `BRIDGE_HOST_REVISION` did not move. No `homerun_*`
export changed either — `gatewayTls` is a field in a JSON argument — so
`FFI_ABI_VERSION` did not move.

## What has and has not been verified

- The core and the dispatch arms are tested in Rust, including every non-gateway
  value reading as device mode and the byte-exact render.
- The FFI crate typechecks with the `device-ws` feature on.
- **The Kotlin and the Swift have not been compiled or run.** They were written
  on a machine with neither an Android SDK nor Xcode. `ios/coretest` carries
  three checks for the new surface; run it on a Mac before trusting the Swift.
- End to end was proven with Homerun Desktop against the staging gateway: a
  websocket upgrade to the gateway URL returned `101`, then the *device's* own
  `Authentication timed out` frame and close code `4001`.

## File map

| File | Role |
|---|---|
| `rust/homerun-core/src/device_ws/mod.rs` | `TlsMode`, `link_up_request`, `needs_certificate`, `LISTEN_WS`, `gateway_tunnel_config` |
| `rust/homerun-supervisor/src/core_dispatch.rs` | `deviceWs.linkUpRequest`; `deviceWs.tunnelConfig` choosing a shape by target |
| `rust/homerun-supervisor/src/device_ws/mod.rs` | `Config.gateway_tls`: no order, no misconfiguration report |
| `rust/homerun-supervisor/src/lib.rs` | reads `gatewayTls` off the host's JSON |
| `android/…/Core.kt`, `ios/…/FFI/Core.swift` | `DeviceLink.gatewayTls` / `wsUrl`, the two new wrappers |
| `android/…/DeviceWebsocket.kt`, `ios/…/DeviceWebsocket.swift` | the branch in `bringUp` |
| `android/…/HomerunApi.kt`, `ios/…/HomerunAPI.swift` | the `link_up` body |
| `ios/coretest/main.swift` | the three checks to run on a Mac |

## Triage

**The dashboard console cannot connect, and the phone's log says
`tlsMode=gateway`.** The certificate is the gateway's, so nothing on the phone is
at fault for a certificate error. A `503` from the gateway URL means the route
exists and the phone is not answering on WG port 4000: check the tunnel is up,
and that the forward's target is the **plaintext** port. A `404` means the
gateway has no route for that device id: the API did not provision one, or the
link was re-provisioned in device mode since.

**The log says `tlsMode=device` on a build that should be in gateway mode.**
That is the API's answer, not a fault here. Its kill switch is set, the region's
gateway is not opted in, or the API predates the field. The phone is doing
exactly what it should.

**A phone in gateway mode is still ordering certificates.** It should be
impossible: `start` refuses when `gatewayTls` is set. If it happens, the host is
not passing the flag — look for a `DeviceLink` built without reading `tls_mode`.

**`app errors` shows `cert-misconfigured` from phones in gateway mode.** The
`None if gateway_tls` arm in `device_ws::start` has been removed or reordered
below the general `None` arm.

**One phone asks for gateway mode and the other does not.** Both must send
`Core.deviceWsLinkUpRequest()`. A host that still posts an empty body stays in
device mode for ever, silently, and keeps spending the rate limit.

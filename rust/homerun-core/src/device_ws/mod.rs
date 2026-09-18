//! The device websocket: reaching a device directly, rather than through the API.
//!
//! The dashboard streams a server's console and sends RCON by connecting to
//! `wss://<device-fqdn>` — a socket the device itself serves. This module holds
//! the decisions in that; the socket, the TLS and the ACME client are effects
//! and live outside the core.
//!
//! Reference: `src/electron/deviceWebsocket/` in the `homerun` repo —
//! `index.ts` for the bring-up order and `wireproxy.ts` for the config this
//! module renders. `plans/device-websocket.md` in this repo is the plan.
//!
//! # A device link is not a server link
//!
//! [`crate::link`] reads a tunnel off a *server* record, where it arrives
//! nested under `config.links[]`. A device link comes from
//! `POST /api/device/<id>/link_up/`, is polled by task id, and arrives **flat**
//! — `native_config` beside `fqdn` at the top level. Same `Link` afterwards,
//! different envelope, so the two parsers stay separate rather than one
//! growing a mode flag.
//!
//! # The numbers that must not change
//!
//! [`LISTEN_HTTPS`], [`LISTEN_HTTP`] and [`LISTEN_WS`] are **the gateway's**,
//! not ours. It sends traffic to those ports on the WireGuard interface,
//! whatever the device happens to bind locally — exactly the rule
//! [`crate::tunnel`] documents for a server's ports, and it fails the same way
//! if "corrected": a config that loads, connects, and is unreachable.
//!
//! # Who terminates TLS
//!
//! Two answers, negotiated per `link_up` and decided by the API — see
//! [`TlsMode`]. The device asks for the gateway to do it
//! ([`link_up_request`]); the API says which one this link got; the device only
//! ever *downgrades* to terminating TLS itself. The reason the gateway mode
//! exists at all is a rate limit: a certificate per device, all under one
//! registered domain, ran into Let's Encrypt's fifty new certificates a week,
//! and a device past the cap has no `wss://` and nothing on screen to say so.
//! The design is `api/docs/plans/device-websocket-gateway-tls.md` in the
//! `homerun` repo; the desktop's half is `deviceWebsocket/wsTlsMode.ts`.

/// The frames themselves, and the order they are allowed in.
pub mod protocol;
/// The header the legacy gateway writes ahead of the TLS ClientHello.
pub mod proxy_protocol;

use crate::tunnel::{Config, Forward, Link};
use serde::{Deserialize, Serialize};

/// Gateway `:443` — TLS, which the device terminates itself.
pub const LISTEN_HTTPS: u16 = 8443;

/// Gateway `:80` — the ACME HTTP-01 challenge, and nothing else.
///
/// Gateway v2 forwards only `/.well-known/acme-challenge/`; the legacy plane
/// forwards all of `:80`. Either way this port exists to prove a hostname, so
/// nothing else should ever be served on it.
pub const LISTEN_HTTP: u16 = 8080;

/// The gateway's relay — a **plaintext** websocket, already decrypted.
///
/// In [`TlsMode::Gateway`] the gateway terminates TLS with its own certificate
/// and forwards what is inside to this port on the WireGuard interface. It is
/// the `svc_port` the API registers for the device's `ws_route` service
/// (`gateway_provision.DEVICE_WS_PORT`), and the desktop's
/// `DEVICE_WS_TUNNEL_PORT`. Three places, one number.
pub const LISTEN_WS: u16 = 4000;

/// Who terminates TLS for this device's websocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// The gateway holds the certificate — one per gateway, for its own
    /// hostname — and relays the decrypted websocket to [`LISTEN_WS`]. The
    /// device obtains no certificate, answers no ACME challenge, and has
    /// nothing reachable from the internet except through the gateway.
    Gateway,
    /// The device holds its own Let's Encrypt certificate behind the gateway's
    /// SNI passthrough: [`LISTEN_HTTPS`] and [`LISTEN_HTTP`]. What every link
    /// was before the gateway learned to terminate, and still what a link is
    /// whenever the API says anything other than `"gateway"`.
    Device,
}

impl TlsMode {
    /// Read the API's answer. **Downgrade-only.**
    ///
    /// Exactly `"gateway"` is gateway mode. `"device"`, a value this build has
    /// never heard of, a non-string, and no field at all — which is what an API
    /// older than the feature sends — are all device mode. There is no way to
    /// be talked *into* gateway mode by anything but the API saying so, because
    /// the gateway only routes to [`LISTEN_WS`] if the API provisioned that
    /// route; a device that assumed it would bring up a tunnel nothing sends to.
    pub fn from_reported(value: Option<&serde_json::Value>) -> Self {
        match value.and_then(|v| v.as_str()) {
            Some("gateway") => TlsMode::Gateway,
            _ => TlsMode::Device,
        }
    }
}

/// What a host sends as the body of `POST /api/device/<id>/link_up/`.
///
/// `ws_tls: "gateway"` says this build *can* run with the gateway terminating
/// TLS. It is a capability, not a demand: the API decides per link-up, gated on
/// its own kill switch and on the region's gateway being opted in, and answers
/// in the result ([`DeviceLink::tls_mode`]). An API that predates the field
/// ignores it.
///
/// Here rather than in each host so that Kotlin and Swift cannot come to send
/// different things — one of them asking and the other not is a phone that
/// silently keeps ordering certificates.
pub fn link_up_request() -> serde_json::Value {
    serde_json::json!({ "ws_tls": "gateway" })
}

/// What `GET /api/device/<id>/link_up/?result=<task>` returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLink {
    pub link: Link,
    /// The device's public hostname: the ACME identifier, the TLS SNI, and
    /// what the dashboard dials. Absent means the API has not named this
    /// device yet, which is a link that can carry traffic but cannot be
    /// reached by name — see [`DeviceLink::can_serve_tls`].
    pub fqdn: Option<String>,
    /// True when the consolidated gateway provisioned this link.
    pub gateway_v2: bool,
    /// Who terminates TLS — the API's answer, read by [`TlsMode::from_reported`].
    pub tls_mode: TlsMode,
    /// The `wss://` address the dashboard is given for this device, when the
    /// API says. In gateway mode it is on the gateway's hostname with an opaque
    /// path token, **not** [`Self::fqdn`]. Informational: nothing here dials it.
    pub ws_url: Option<String>,
}

impl DeviceLink {
    /// Whether a certificate can be obtained for this link.
    ///
    /// ACME needs a hostname to prove. Without one the tunnel still comes up
    /// and can serve plaintext, which is what the desktop degrades to.
    pub fn can_serve_tls(&self) -> bool {
        self.fqdn.is_some()
    }

    /// Whether **this device** should obtain a certificate and answer the ACME
    /// challenge.
    ///
    /// Only in device mode, and only with a hostname to prove. In gateway mode
    /// the answer is no even though the link still has an `fqdn`: nothing
    /// routes that name to this device any more, so an order for it could not
    /// validate, and every attempt would spend a slot of the rate limit this
    /// mode exists to stop spending.
    pub fn needs_certificate(&self) -> bool {
        self.tls_mode == TlsMode::Device && self.can_serve_tls()
    }

    /// Whether connections on [`LISTEN_HTTPS`] arrive behind a PROXY v1 header.
    ///
    /// The legacy plane is nginx, which prefixes one; the v2 gateway is
    /// HAProxy configured with `real_ip_mode=none`, which does not. Getting
    /// this wrong is not a warning — the header lands where a TLS ClientHello
    /// is expected, so the handshake fails on every connection.
    pub fn expects_proxy_protocol(&self) -> bool {
        !self.gateway_v2
    }
}

/// Pull the link out of a `link_up` result body.
///
/// Returns `None` while the task is still running: the API answers with a body
/// that has no `native_config` yet, which is the normal state for the first
/// several seconds and is not an error to report.
///
/// Every key field must be present. A half-written config would otherwise
/// surface a minute later as an unexplained handshake timeout, which is the
/// most expensive possible way to learn it.
pub fn from_link_up_body(body: &serde_json::Value) -> Option<DeviceLink> {
    let native = body.get("native_config").filter(|v| !v.is_null())?;

    let field = |name: &str| {
        native
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let address = field("address");
    Some(DeviceLink {
        // Corroborated two ways, as the desktop does: the API says so, or the
        // link carries a per-peer /32, which only v2 allocates.
        gateway_v2: body.get("gateway_version").and_then(|v| v.as_u64()) == Some(2)
            || address.is_some(),
        link: Link {
            client_privkey: field("client_privkey")?,
            gateway_pubkey: field("gateway_pubkey")?,
            link_address: field("link_address")?,
            address,
            allowed_ips: field("allowed_ips"),
        },
        fqdn: body
            .get("fqdn")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tls_mode: TlsMode::from_reported(body.get("ws_tls")),
        ws_url: body
            .get("ws_url")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// The tunnel config for a device websocket.
///
/// `https_target` is whatever is listening for TLS locally — the cert manager
/// normally, or the plaintext websocket itself when there is no certificate and
/// the device is serving degraded.
///
/// `http_target` is the ACME challenge listener, and `None` omits the forward
/// entirely: with no certificate to obtain there is nothing to answer on `:80`,
/// and a forward to a closed port is a worse answer than no forward.
pub fn tunnel_config(link: Link, https_target: u16, http_target: Option<u16>) -> Config {
    let mut forwards = vec![Forward::tcp(LISTEN_HTTPS, https_target)];
    if let Some(target) = http_target {
        forwards.push(Forward::tcp(LISTEN_HTTP, target));
    }
    Config { link, forwards }
}

/// The tunnel config for a device whose TLS the **gateway** terminates.
///
/// One forward: the gateway's relay port to the plaintext websocket. No
/// [`LISTEN_HTTPS`], because no ClientHello will ever arrive; no
/// [`LISTEN_HTTP`], because there is no certificate to prove a name for.
///
/// `ws_target` is the **plaintext** listener — the same one the app's own UI
/// dials over loopback. Pointing this at the TLS listener instead is the
/// mirror image of the mistake [`tunnel_config`]'s callers are warned about:
/// the gateway sends a websocket upgrade, the TLS listener has no certificate
/// in this mode and drops every connection, and the dashboard sees a 503.
pub fn gateway_tunnel_config(link: Link, ws_target: u16) -> Config {
    Config {
        link,
        forwards: vec![Forward::tcp(LISTEN_WS, ws_target)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn native() -> serde_json::Value {
        json!({
            "client_privkey": "PRIV",
            "gateway_pubkey": "PUB",
            "link_address": "gw.example.com:51820"
        })
    }

    #[test]
    fn a_pending_task_is_not_an_error() {
        // The API answers 200 with no config while the task runs. Reporting a
        // failure here would abandon a link_up that was about to succeed.
        assert_eq!(from_link_up_body(&json!({ "fqdn": "d.example.com" })), None);
        assert_eq!(from_link_up_body(&json!({ "native_config": null })), None);
    }

    #[test]
    fn a_half_written_config_is_refused() {
        for missing in ["client_privkey", "gateway_pubkey", "link_address"] {
            let mut n = native();
            n.as_object_mut().unwrap().remove(missing);
            assert_eq!(
                from_link_up_body(&json!({ "native_config": n })),
                None,
                "a config missing {missing} must not be accepted — it fails later, as a handshake timeout"
            );
        }
    }

    #[test]
    fn blank_strings_count_as_missing() {
        let mut n = native();
        n["client_privkey"] = json!("   ");
        assert_eq!(from_link_up_body(&json!({ "native_config": n })), None);
    }

    /// Separate from the rejection tests on purpose: those cover *refusing* an
    /// unusable field, this covers *normalising* a usable one. Breaking either
    /// rule leaves the other's test green, which is what makes them two tests
    /// rather than one written twice.
    #[test]
    fn surrounding_whitespace_is_stripped_not_carried_into_the_config() {
        let mut n = native();
        n["client_privkey"] = json!("  PRIV\n");
        n["link_address"] = json!(" gw.example.com:51820 ");
        let link =
            from_link_up_body(&json!({ "fqdn": " d.example.com ", "native_config": n })).unwrap();

        // A key with a trailing newline renders an INI line wireproxy rejects,
        // and an fqdn with a space is an ACME identifier that never validates.
        assert_eq!(link.link.client_privkey, "PRIV");
        assert_eq!(link.link.link_address, "gw.example.com:51820");
        assert_eq!(link.fqdn.as_deref(), Some("d.example.com"));
    }

    #[test]
    fn the_link_is_read_flat_not_from_a_server_body() {
        // Guards the difference this module exists for: a server's link is
        // nested under config.links[], a device's is at the top level.
        let device = from_link_up_body(&json!({
            "fqdn": "d.example.com",
            "native_config": native()
        }))
        .expect("a flat body is the device shape");
        assert_eq!(device.link.client_privkey, "PRIV");
        assert_eq!(device.fqdn.as_deref(), Some("d.example.com"));

        let server_shaped = json!({ "config": { "links": [ { "native_config": native() } ] } });
        assert_eq!(from_link_up_body(&server_shaped), None);
    }

    #[test]
    fn a_link_with_no_fqdn_cannot_serve_tls() {
        let link = from_link_up_body(&json!({ "native_config": native() })).unwrap();
        assert!(
            !link.can_serve_tls(),
            "no hostname, nothing for ACME to prove"
        );

        let blank = from_link_up_body(&json!({ "fqdn": "  ", "native_config": native() })).unwrap();
        assert!(!blank.can_serve_tls(), "a blank fqdn is not a hostname");
    }

    #[test]
    fn v2_is_recognised_by_either_signal() {
        let by_version = from_link_up_body(&json!({
            "gateway_version": 2,
            "native_config": native()
        }))
        .unwrap();
        assert!(by_version.gateway_v2);

        let mut n = native();
        n["address"] = json!("10.8.0.7/32");
        let by_address = from_link_up_body(&json!({ "native_config": n })).unwrap();
        assert!(
            by_address.gateway_v2,
            "only v2 allocates a per-peer /32, so its presence is the same claim"
        );

        let legacy = from_link_up_body(&json!({ "native_config": native() })).unwrap();
        assert!(!legacy.gateway_v2);
    }

    #[test]
    fn only_the_legacy_plane_sends_a_proxy_header() {
        let legacy = from_link_up_body(&json!({ "native_config": native() })).unwrap();
        assert!(
            legacy.expects_proxy_protocol(),
            "nginx prefixes PROXY v1; not stripping it breaks every TLS handshake"
        );

        let v2 = from_link_up_body(&json!({
            "gateway_version": 2,
            "native_config": native()
        }))
        .unwrap();
        assert!(
            !v2.expects_proxy_protocol(),
            "HAProxy runs real_ip_mode=none; stripping a header that is not there eats the ClientHello"
        );
    }

    #[test]
    fn without_a_cert_manager_there_is_no_challenge_forward() {
        let link = from_link_up_body(&json!({ "native_config": native() }))
            .unwrap()
            .link;
        let config = tunnel_config(link, 4000, None);
        assert_eq!(config.forwards.len(), 1);
        assert_eq!(config.forwards[0].listen_port, LISTEN_HTTPS);
        assert_eq!(
            config.forwards[0].target_port, 4000,
            "degraded serving points :443 straight at the plaintext socket"
        );
    }

    /// Byte-exact against `generateDeviceWireproxyConfig` in the desktop's
    /// `deviceWebsocket/wireproxy.ts`. The gateway is the same one, so a
    /// difference here is a bug by definition — the same standard
    /// `crate::tunnel` holds itself to.
    #[test]
    fn renders_what_the_desktop_renders() {
        let link = from_link_up_body(&json!({
            "fqdn": "d.example.com",
            "native_config": native()
        }))
        .unwrap()
        .link;

        assert_eq!(
            tunnel_config(link, 8444, Some(8081)).render(),
            "[Interface]\n\
             PrivateKey = PRIV\n\
             Address = 10.0.0.2/24\n\
             MTU = 1280\n\
             \n\
             [Peer]\n\
             PublicKey = PUB\n\
             Endpoint = gw.example.com:51820\n\
             AllowedIPs = 10.0.0.1/32\n\
             PersistentKeepalive = 30\n\
             \n\
             [TCPServerTunnel]\n\
             ListenPort = 8443\n\
             Target = 127.0.0.1:8444\n\
             \n\
             [TCPServerTunnel]\n\
             ListenPort = 8080\n\
             Target = 127.0.0.1:8081\n"
        );
    }

    #[test]
    fn a_v2_link_carries_its_allocated_addresses_into_the_config() {
        let mut n = native();
        n["address"] = json!("10.8.0.7/32");
        n["allowed_ips"] = json!("10.8.0.1/32");
        let link = from_link_up_body(&json!({ "native_config": n }))
            .unwrap()
            .link;

        let rendered = tunnel_config(link, 8444, Some(8081)).render();
        assert!(rendered.contains("Address = 10.8.0.7/32"));
        assert!(rendered.contains("AllowedIPs = 10.8.0.1/32"));
        assert!(
            !rendered.contains("10.0.0.2/24"),
            "the legacy fallback must not survive alongside an allocated address"
        );
    }

    // --- who terminates TLS ------------------------------------------------

    fn linked(extra: serde_json::Value) -> DeviceLink {
        let mut body = json!({ "fqdn": "d.example.com", "native_config": native() });
        for (k, v) in extra.as_object().unwrap() {
            body[k.as_str()] = v.clone();
        }
        from_link_up_body(&body).unwrap()
    }

    #[test]
    fn the_api_saying_gateway_is_the_only_way_into_gateway_mode() {
        assert_eq!(linked(json!({ "ws_tls": "gateway" })).tls_mode, TlsMode::Gateway);
    }

    /// Every one of these is a real state: an explicit refusal, an API older
    /// than the field, and the values a future or confused API might send. A
    /// device that read any of them as gateway mode would bring up a tunnel on a
    /// port nothing is sent to and serve no console at all.
    #[test]
    fn anything_else_is_device_mode() {
        assert_eq!(linked(json!({})).tls_mode, TlsMode::Device, "an older API sends no field");
        for other in [json!("device"), json!("GATEWAY"), json!("wildcard"), json!(""), json!(2), json!(true), json!(null)] {
            assert_eq!(
                linked(json!({ "ws_tls": other })).tls_mode,
                TlsMode::Device,
                "{other} must not be read as gateway mode"
            );
        }
    }

    #[test]
    fn a_gateway_mode_device_orders_no_certificate_even_though_it_has_a_name() {
        let device = linked(json!({ "ws_tls": "gateway" }));
        assert!(device.can_serve_tls(), "the link still carries its fqdn");
        assert!(
            !device.needs_certificate(),
            "nothing routes that name here any more, so an order cannot validate — and each \
             attempt spends the rate limit this mode exists to stop spending"
        );
        assert!(linked(json!({})).needs_certificate());
        assert!(linked(json!({ "ws_tls": "device" })).needs_certificate());
    }

    #[test]
    fn device_mode_without_a_name_still_needs_no_certificate() {
        let unnamed = from_link_up_body(&json!({ "native_config": native() })).unwrap();
        assert_eq!(unnamed.tls_mode, TlsMode::Device);
        assert!(!unnamed.needs_certificate());
    }

    #[test]
    fn the_public_address_is_carried_but_blank_is_absent() {
        let url = "wss://ws-us-east-2.example.com/d/0f8fad5b-d9cb-469f-a165-70867728950e";
        assert_eq!(
            linked(json!({ "ws_tls": "gateway", "ws_url": format!(" {url} ") })).ws_url.as_deref(),
            Some(url)
        );
        assert_eq!(linked(json!({ "ws_url": "  " })).ws_url, None);
        assert_eq!(linked(json!({})).ws_url, None);
    }

    #[test]
    fn the_mode_crosses_the_boundary_as_a_lowercase_word() {
        // Hosts read this key out of JSON. A rename here that the hosts do not
        // follow is a phone that reads "absent" and runs device mode for ever.
        let value = serde_json::to_value(linked(json!({ "ws_tls": "gateway" }))).unwrap();
        assert_eq!(value["tls_mode"], "gateway");
        assert_eq!(serde_json::to_value(linked(json!({}))).unwrap()["tls_mode"], "device");
    }

    #[test]
    fn both_hosts_ask_for_the_same_thing() {
        assert_eq!(link_up_request(), json!({ "ws_tls": "gateway" }));
    }

    /// Byte-exact against `generateDeviceWireproxyConfig` with
    /// `{ kind: "gateway" }` in the desktop's `deviceWebsocket/wireproxy.ts`,
    /// and the port against the API's `gateway_provision.DEVICE_WS_PORT`.
    #[test]
    fn gateway_mode_renders_one_forward_to_the_plaintext_socket() {
        let link = linked(json!({ "ws_tls": "gateway" })).link;
        assert_eq!(
            gateway_tunnel_config(link, 41873).render(),
            "[Interface]\n\
             PrivateKey = PRIV\n\
             Address = 10.0.0.2/24\n\
             MTU = 1280\n\
             \n\
             [Peer]\n\
             PublicKey = PUB\n\
             Endpoint = gw.example.com:51820\n\
             AllowedIPs = 10.0.0.1/32\n\
             PersistentKeepalive = 30\n\
             \n\
             [TCPServerTunnel]\n\
             ListenPort = 4000\n\
             Target = 127.0.0.1:41873\n"
        );
    }

    #[test]
    fn gateway_mode_never_forwards_the_tls_or_challenge_ports() {
        let rendered = gateway_tunnel_config(linked(json!({ "ws_tls": "gateway" })).link, 4000).render();
        assert!(!rendered.contains("ListenPort = 8443"));
        assert!(!rendered.contains("ListenPort = 8080"));
        assert_eq!(rendered.matches("[TCPServerTunnel]").count(), 1);
    }

    #[test]
    fn the_relay_port_is_the_one_the_api_registers() {
        assert_eq!(LISTEN_WS, 4000);
    }
}

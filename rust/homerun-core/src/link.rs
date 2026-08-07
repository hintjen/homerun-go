//! Reading the gateway's tunnel credentials off a server record, and knowing
//! when they are the dead set from last time.
//!
//! Reference: `pollForNativeConfig` in `src/electron/nativeServerManager.ts`.

use crate::wireproxy::Link;
use serde::{Deserialize, Serialize};

/// A link as the API serialises it, plus the one field that changes how
/// staleness is judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolledLink {
    pub link: Link,
    /// True when `provisioner == "gateway2"`.
    pub is_gateway2: bool,
}

/// Pull the tunnel out of a `GET /api/server/<id>/` body.
///
/// Returns `None` when the gateway has not written one yet, which is the
/// normal state for the first seconds after a server is marked running — the
/// API provisions the peer asynchronously.
///
/// All three of the key fields must be present. A half-written config would
/// otherwise surface as an unexplained handshake timeout a minute later,
/// which is the hardest possible way to find out.
pub fn from_server_body(body: &serde_json::Value) -> Option<PolledLink> {
    let link = body.get("config")?.get("links")?.as_array()?.first()?;

    let native = link.get("native_config").filter(|v| !v.is_null())?;
    let field = |name: &str| {
        native
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    Some(PolledLink {
        link: Link {
            client_privkey: field("client_privkey")?,
            gateway_pubkey: field("gateway_pubkey")?,
            link_address: field("link_address")?,
            address: field("address"),
            allowed_ips: field("allowed_ips"),
        },
        is_gateway2: link.get("provisioner").and_then(|p| p.as_str()) == Some("gateway2"),
    })
}

/// Is this link usable for the launch that is starting now?
///
/// The legacy provisioner mints a fresh keypair every session, so a config
/// still identical to the one seen before launch is the **previous, dead**
/// set — using it fails the handshake ten times and then stops the server.
///
/// Gateway v2 reuses credentials deliberately across suspend and resume, so
/// for those the check is skipped. Without that exception a v2 link would be
/// judged stale on every single start and poll until timeout.
pub fn is_usable(polled: &PolledLink, before_launch: Option<&Link>) -> bool {
    if polled.is_gateway2 {
        return true;
    }
    match before_launch {
        Some(stale) => &polled.link != stale,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(native: serde_json::Value, provisioner: &str) -> serde_json::Value {
        json!({ "config": { "links": [ {
            "provisioner": provisioner,
            "native_config": native
        } ] } })
    }

    fn full_native() -> serde_json::Value {
        json!({
            "client_privkey": "PRIV",
            "gateway_pubkey": "PUB",
            "link_address": "minecraft.example.com:51820",
            "address": "10.100.0.3/32",
            "allowed_ips": "10.100.0.1/32"
        })
    }

    #[test]
    fn reads_a_complete_gateway_v2_link() {
        let polled = from_server_body(&body(full_native(), "gateway2")).unwrap();
        assert!(polled.is_gateway2);
        assert_eq!(polled.link.link_address, "minecraft.example.com:51820");
        assert_eq!(polled.link.address.as_deref(), Some("10.100.0.3/32"));
    }

    #[test]
    fn a_legacy_link_carries_no_addresses() {
        let native = json!({
            "client_privkey": "PRIV", "gateway_pubkey": "PUB",
            "link_address": "gw:51820"
        });
        let polled = from_server_body(&body(native, "legacy")).unwrap();
        assert!(!polled.is_gateway2);
        assert_eq!(polled.link.address, None);
        assert_eq!(polled.link.allowed_ips, None);
    }

    #[test]
    fn not_yet_provisioned_reads_as_absent() {
        assert!(from_server_body(&body(serde_json::Value::Null, "gateway2")).is_none());
        assert!(from_server_body(&json!({ "config": { "links": [] } })).is_none());
        assert!(from_server_body(&json!({ "config": {} })).is_none());
        assert!(from_server_body(&json!({})).is_none());
    }

    /// Every one of these would otherwise become a handshake timeout minutes
    /// later, with nothing pointing at the cause.
    #[test]
    fn a_half_written_config_is_not_a_link() {
        for missing in ["client_privkey", "gateway_pubkey", "link_address"] {
            let mut native = full_native();
            native.as_object_mut().unwrap().remove(missing);
            assert!(
                from_server_body(&body(native, "gateway2")).is_none(),
                "accepted a config with no {missing}"
            );
        }
    }

    #[test]
    fn blank_strings_count_as_missing() {
        let mut native = full_native();
        native["client_privkey"] = json!("   ");
        assert!(from_server_body(&body(native, "gateway2")).is_none());
    }

    /// The legacy plane regenerates per session, so an unchanged config is the
    /// dead one.
    #[test]
    fn legacy_rejects_the_pre_launch_config() {
        let polled = from_server_body(&body(full_native(), "legacy")).unwrap();
        assert!(!is_usable(&polled, Some(&polled.link.clone())));
    }

    #[test]
    fn legacy_accepts_a_freshly_provisioned_config() {
        let polled = from_server_body(&body(full_native(), "legacy")).unwrap();
        let old = Link {
            client_privkey: "OLD".into(),
            ..polled.link.clone()
        };
        assert!(is_usable(&polled, Some(&old)));
    }

    /// Without this exception a v2 link polls until timeout on every start.
    #[test]
    fn gateway_v2_accepts_an_unchanged_config() {
        let polled = from_server_body(&body(full_native(), "gateway2")).unwrap();
        assert!(is_usable(&polled, Some(&polled.link.clone())));
    }

    #[test]
    fn a_first_ever_launch_has_nothing_to_compare_against() {
        let polled = from_server_body(&body(full_native(), "legacy")).unwrap();
        assert!(is_usable(&polled, None));
    }
}

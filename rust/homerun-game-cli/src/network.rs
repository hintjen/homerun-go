//! Bounded probe inventory, shared with the lifecycle's independent network watcher.
use crate::protocol::codes;
use homerun_core::engine::{
    ports::{classify_audited, classify_binding, Binding},
    GameDescriptor,
};
use homerun_supervisor::platform::{self, DynamicPorts, Listening};
use serde::Serialize;
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

#[derive(Clone, Default)]
pub struct Audit {
    pub strict: bool,
    data: Arc<Mutex<Record>>,
    /// The OS's dynamic UDP ranges, read on first use and only by the strict
    /// audit: `launch` has no use for them.
    dynamic: Arc<OnceLock<DynamicPorts>>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    samples: u64,
    inspection_error: Option<String>,
    listeners: Vec<Endpoint>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    pid: u32,
    protocol: homerun_core::tunnel::Protocol,
    address: std::net::IpAddr,
    port: u16,
    declared: bool,
    confined: bool,
    /// Undeclared UDP on a port the OS chose: recorded, not refused.
    ephemeral: bool,
    first_seen_ms: u128,
    last_seen_ms: u128,
    samples: u64,
}

impl Audit {
    pub fn clear(&self) {
        *self.data.lock().unwrap() = Record::default();
    }
    pub fn report(&self) -> serde_json::Value {
        let mut report = serde_json::json!({"scope":if cfg!(windows) { "owned-process-tree" } else { "server-pid" }, "pollIntervalMs":250,
            "timingOrigin":"network watcher start, before child spawn", "inventory":*self.data.lock().unwrap()});
        if self.strict {
            let dynamic = self.dynamic_ports();
            let range =
                |r: platform::PortRange| serde_json::json!({"first": r.first, "last": r.last});
            report["ephemeralUdp"] = serde_json::json!({"ipv4": range(dynamic.ipv4),
                "ipv6": range(dynamic.ipv6), "source": dynamic.source});
        }
        report
    }
    fn dynamic_ports(&self) -> DynamicPorts {
        *self.dynamic.get_or_init(platform::udp_dynamic_ports)
    }
    pub fn inspection_failed(&self, error: &str) {
        self.data.lock().unwrap().inspection_error = Some(error.into());
    }
    pub fn check(
        &self,
        d: &GameDescriptor,
        sockets: &[(u32, Listening)],
        elapsed: Duration,
    ) -> Result<(), (&'static str, String)> {
        let mut record = self.data.lock().unwrap();
        record.samples += 1;
        let mut refusal = None;
        for (pid, socket) in sockets {
            let binding = if self.strict {
                let dynamic = self.dynamic_ports();
                let range = match socket.address {
                    std::net::IpAddr::V4(_) => dynamic.ipv4,
                    std::net::IpAddr::V6(_) => dynamic.ipv6,
                };
                classify_audited(
                    d,
                    socket.protocol,
                    socket.port,
                    socket.address,
                    &(range.first..=range.last),
                )
            } else {
                classify_binding(d, socket.protocol, socket.port, socket.address)
            };
            let declared = !matches!(binding, Binding::Undeclared | Binding::Ephemeral);
            if self.strict {
                if let Some(row) = record.listeners.iter_mut().find(|r| {
                    r.pid == *pid
                        && r.protocol == socket.protocol
                        && r.port == socket.port
                        && r.address == socket.address
                }) {
                    row.last_seen_ms = elapsed.as_millis();
                    row.samples += 1;
                } else {
                    if record.listeners.len() >= 4096 {
                        record.inspection_error = Some("Listener inventory limit reached.".into());
                        return Err((
                            codes::PORT_INSPECTION_FAILED,
                            "The game's network activity could not be fully inspected.".into(),
                        ));
                    }
                    record.listeners.push(Endpoint {
                        pid: *pid,
                        protocol: socket.protocol,
                        address: socket.address,
                        port: socket.port,
                        declared,
                        confined: socket.is_confined(),
                        ephemeral: binding == Binding::Ephemeral,
                        first_seen_ms: elapsed.as_millis(),
                        last_seen_ms: elapsed.as_millis(),
                        samples: 1,
                    });
                }
            }
            match binding {
                Binding::PrivateExposed(name) => refusal = Some((codes::PORT_EXPOSED,
                    format!("This game opened its private \"{name}\" port to other computers. Homerun is terminating it to protect that port; unsaved progress may be lost."))),
                Binding::Undeclared if self.strict && !socket.is_confined() => refusal = Some((codes::PORT_EXPOSED,
                    format!("This game opened undeclared {:?} port {} to other computers. Verification failed and Homerun is terminating the game.", socket.protocol, socket.port))),
                _ => {}
            }
        }
        refusal.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use homerun_core::tunnel::Protocol;
    use platform::PortRange;

    fn audit() -> Audit {
        let audit = Audit {
            strict: true,
            ..Audit::default()
        };
        let range = PortRange {
            first: 49152,
            last: 65535,
        };
        audit
            .dynamic
            .set(DynamicPorts {
                ipv4: range,
                ipv6: range,
                source: "test",
            })
            .unwrap();
        audit
    }

    fn descriptor() -> GameDescriptor {
        serde_json::from_value(serde_json::json!({"id":"g","ports":[
            {"name":"game","proto":"udp","port":7777,"expose":true},
            {"name":"rcon","proto":"tcp","port":25575,"expose":false}
        ]}))
        .unwrap()
    }

    fn socket(protocol: Protocol, address: &str, port: u16) -> (u32, Listening) {
        (
            1,
            Listening {
                protocol,
                port,
                address: address.parse().unwrap(),
            },
        )
    }

    fn row(audit: &Audit, port: u16) -> serde_json::Value {
        audit.report()["inventory"]["listeners"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["port"] == port)
            .cloned()
            .expect("socket absent from evidence")
    }

    #[test]
    fn an_os_assigned_udp_socket_is_recorded_not_refused() {
        let audit = audit();
        for address in ["0.0.0.0", "::"] {
            let sockets = [
                socket(Protocol::Udp, "0.0.0.0", 7777),
                socket(Protocol::Udp, address, 56372),
            ];
            assert_eq!(audit.check(&descriptor(), &sockets, Duration::ZERO), Ok(()));
        }
        let row = row(&audit, 56372);
        assert_eq!(row["declared"], false);
        assert_eq!(row["confined"], false);
        assert_eq!(row["ephemeral"], true);
        assert_eq!(audit.report()["ephemeralUdp"]["source"], "test");
        assert_eq!(audit.report()["ephemeralUdp"]["ipv4"]["first"], 49152);
    }

    #[test]
    fn everything_else_undeclared_or_private_is_still_refused() {
        for (protocol, address, port) in [
            // A fixed UDP port someone chose is a service.
            (Protocol::Udp, "0.0.0.0", 27015),
            (Protocol::Udp, "::", 49151),
            // A wildcard TCP listener is a service whatever its port.
            (Protocol::Tcp, "0.0.0.0", 56372),
            (Protocol::Tcp, "::", 65535),
            // A private port on a wide interface, as before.
            (Protocol::Tcp, "0.0.0.0", 25575),
        ] {
            let audit = audit();
            let refused = audit.check(
                &descriptor(),
                &[socket(protocol, address, port)],
                Duration::ZERO,
            );
            assert_eq!(
                refused.unwrap_err().0,
                codes::PORT_EXPOSED,
                "{protocol:?} {address}:{port}"
            );
            assert_eq!(row(&audit, port)["ephemeral"], false);
        }
    }

    #[test]
    fn the_launch_path_is_unchanged() {
        let audit = Audit::default();
        let sockets = [
            socket(Protocol::Udp, "0.0.0.0", 56372),
            socket(Protocol::Tcp, "0.0.0.0", 56373),
        ];
        assert_eq!(audit.check(&descriptor(), &sockets, Duration::ZERO), Ok(()));
        assert!(
            audit.dynamic.get().is_none(),
            "launch must not read the dynamic range"
        );
        let private = [socket(Protocol::Tcp, "0.0.0.0", 25575)];
        assert_eq!(
            audit
                .check(&descriptor(), &private, Duration::ZERO)
                .unwrap_err()
                .0,
            codes::PORT_EXPOSED
        );
    }
}

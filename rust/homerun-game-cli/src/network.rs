//! Bounded probe inventory, shared with the lifecycle's independent network watcher.
use crate::protocol::codes;
use homerun_core::engine::{
    ports::{classify_binding, Binding},
    GameDescriptor,
};
use homerun_supervisor::platform::Listening;
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
pub struct Audit {
    pub strict: bool,
    data: Arc<Mutex<Record>>,
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
    first_seen_ms: u128,
    last_seen_ms: u128,
    samples: u64,
}

impl Audit {
    pub fn clear(&self) {
        *self.data.lock().unwrap() = Record::default();
    }
    pub fn report(&self) -> serde_json::Value {
        serde_json::json!({"scope":if cfg!(windows) { "owned-process-tree" } else { "server-pid" }, "pollIntervalMs":250,
            "timingOrigin":"network watcher start, before child spawn", "inventory":*self.data.lock().unwrap()})
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
            let binding = classify_binding(d, socket.protocol, socket.port, socket.address);
            let declared = binding != Binding::Undeclared;
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

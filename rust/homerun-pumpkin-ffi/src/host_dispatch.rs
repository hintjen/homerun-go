//! The calls that need a socket, on the same wire as the ones that do not.
//!
//! # Why this exists beside `core_dispatch`
//!
//! [`crate::core_dispatch`] is pure by policy — it opens nothing and spawns
//! nothing, which is what keeps `homerun-core` testable on any machine. But a
//! few things a host needs are *decisions wrapped around one effect*, where
//! the decision is the whole difficulty and the effect is a dozen lines.
//! Measuring the gateway is the case in point: the Server List Ping codec is
//! subtle enough that the desktop got it wrong twice, and the socket around it
//! is a connect, a write and a read.
//!
//! Writing that socket once per platform would mean the interesting half is
//! shared and the half that can hang is not. So it lives here, in the crate
//! that is already allowed to have effects, and reaches both platforms through
//! the entry points they already use — Android's `nativeCall`, iOS's
//! `homerun_core_call`. **No new export, so no ABI change**: `call` simply
//! answers the methods it knows and hands everything else to the core.
//!
//! Nothing stateful belongs here. If a call needs to be remembered between
//! invocations it belongs in the supervisor, or as state the host holds.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use homerun_core::minecraft::slp;

use crate::core_dispatch;

/// How long a measurement may take in total.
///
/// A **deadline**, not an idle timeout. The desktop sets `socket.setTimeout`,
/// which resets on every byte — so a peer dribbling one byte a second keeps
/// the measurement alive indefinitely, and on a phone that is a wake lock held
/// by a stranger.
const DEADLINE: Duration = Duration::from_secs(5);

/// How much to read at once. A status response is a few kilobytes of MOTD and
/// player sample; the reader caps the total.
const CHUNK: usize = 4 * 1024;

/// Dispatch one call, answering what needs an effect and delegating the rest.
pub fn call(method: &str, args: &str) -> String {
    let handled = match method {
        "net.gatewayPing" => Some(
            std::panic::catch_unwind(|| gateway_ping(args))
                .unwrap_or_else(|_| Err(format!("the native host panicked handling \"{method}\""))),
        ),
        "server.statsPoll" => Some(
            std::panic::catch_unwind(|| stats_poll(args))
                .unwrap_or_else(|_| Err(format!("the native host panicked handling \"{method}\""))),
        ),
        _ => None,
    };

    match handled {
        Some(Ok(value)) => json!({ "ok": true, "value": value }).to_string(),
        Some(Err(error)) => json!({ "ok": false, "error": error }).to_string(),
        None => core_dispatch::call(method, args),
    }
}

/// How long to wait for a running server to answer a poll.
///
/// A console reply comes back in well under a second, or the command was
/// shadowed by a plugin and is never coming.
const POLL_TIMEOUT: Duration = Duration::from_secs(3);

/// The two things a stats report needs from the server itself.
///
/// One call rather than two round trips through the host, because the answers
/// must not reach the console and only the supervisor can withhold them — see
/// [`crate::server::Ask`]. A host that did this for itself would be filtering
/// a buffer the UI has already read from.
///
/// Both fields are independently optional: a shadowed `/list` should not cost
/// the gametime, and an unrecognised gametime reply should not cost the
/// roster.
fn stats_poll(args: &str) -> Result<Value, String> {
    use homerun_core::minecraft::jar::Loader;
    use homerun_core::reporting::stats;

    let args: Value = serde_json::from_str(args).map_err(|e| format!("bad arguments: {e}"))?;
    let loader = Loader::parse(args.get("loader").and_then(Value::as_str))
        .map_err(|e| format!("\"server.statsPoll\": {e}"))?;
    let timeout = args
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(POLL_TIMEOUT);

    let host = crate::server::host();

    // A linked engine holds the player list already, so asking the console for
    // it would be a slower way to get a worse answer — and on Pumpkin, an
    // empty one. Only an engine that cannot name identities falls through to
    // the round trip below; see `Engine::roster_is_authoritative`.
    //
    // Pinned, because a Bukkit-family plugin shadowing `/list` or `/time` is
    // the ordinary reason these come back unreadable.
    let roster = host.reportable_roster().or_else(|| {
        host.ask(
            crate::server::Ask::Roster,
            &stats::pinned(stats::LIST_UUIDS, loader),
            timeout,
        )
        .and_then(|reply| stats::parse_list_uuids(&reply))
    });

    let age = host
        .ask(
            crate::server::Ask::Age,
            &stats::pinned(stats::GAMETIME, loader),
            timeout,
        )
        .and_then(|reply| stats::parse_server_age(&reply));

    Ok(json!({
        "roster": roster,
        "ageSecs": age,
    }))
}

/// Round-trip time to a Minecraft server, in milliseconds.
///
/// Answers `null` rather than an error for every ordinary failure — an
/// unreachable gateway, a peer that is not speaking the protocol, a timeout.
/// This is one optional field on a stats report, and a report that refuses to
/// be built because a measurement failed is worse than a report with a hole in
/// it.
///
/// The address must be the gateway's public one (`link::public_address`), not
/// the port the server listens on locally: the number worth having is what a
/// player's latency would be, through the tunnel, not how fast this device can
/// reach itself.
fn gateway_ping(args: &str) -> Result<Value, String> {
    let args: Value = serde_json::from_str(args).map_err(|e| format!("bad arguments: {e}"))?;
    let host = args
        .get("host")
        .and_then(Value::as_str)
        .ok_or("\"net.gatewayPing\" needs a host")?;
    let port = args
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .ok_or("\"net.gatewayPing\" needs a port")?;

    let deadline = args
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(DEADLINE);

    Ok(measure(host, port, deadline)
        .map(Value::from)
        .unwrap_or(Value::Null))
}

fn measure(host: &str, port: u16, deadline: Duration) -> Option<f64> {
    let started = Instant::now();
    // `checked_sub` rather than a subtraction: a socket call given a zero
    // timeout blocks forever, so an expired deadline must end the attempt
    // rather than begin an unbounded one.
    let remaining = || {
        deadline
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
    };

    // Resolution counts against the deadline, and on a phone that has just
    // woken up it is the slowest part.
    let address = (host, port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&address, remaining()?).ok()?;
    // Nagle would hold the tiny handshake back waiting for more to send.
    let _ = stream.set_nodelay(true);

    stream.set_write_timeout(remaining()).ok()?;
    stream.write_all(&slp::open(host, port)).ok()?;

    let mut reader = slp::Reader::new();
    let mut buffer = [0u8; CHUNK];
    // Started when the ping goes out, not when the connection does: this is a
    // round trip, not a session. The handshake before it includes the server
    // building its whole status response, which is not latency.
    let mut sent_at = None;

    loop {
        loop {
            match reader.step() {
                slp::Step::NeedMore => break,
                slp::Step::SendPing => {
                    stream.set_write_timeout(remaining()).ok()?;
                    let at = Instant::now();
                    stream.write_all(&slp::ping(0)).ok()?;
                    sent_at = Some(at);
                }
                slp::Step::Pong => {
                    return sent_at.map(|at| at.elapsed().as_secs_f64() * 1_000.0);
                }
                // Not a Minecraft server, or not one we understand. Failing
                // now is the point of the core telling malformed apart from
                // incomplete — the desktop cannot, and waits out its timeout.
                slp::Step::Malformed => return None,
            }
        }

        stream.set_read_timeout(remaining()).ok()?;
        match stream.read(&mut buffer) {
            // The peer closed without answering.
            Ok(0) => return None,
            Ok(read) => reader.feed(&buffer[..read]),
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anything the core already answers must still be answered, unchanged —
    /// this layer is a prefix, not a replacement.
    #[test]
    fn unknown_methods_fall_through_to_the_core() {
        let reply = call("state.exit", r#"{"intentional":true,"code":0}"#);
        assert!(reply.contains("\"ok\":true"), "{reply}");
        assert!(reply.contains("stopped"), "{reply}");

        let reply = call("no.such.method", "{}");
        assert!(reply.contains("\"ok\":false"), "{reply}");
    }

    /// A measurement that cannot be made is a null field, not a failed call.
    /// The reserved TEST-NET-1 address is unroutable by definition, so this
    /// exercises the failure path without depending on the network.
    #[test]
    fn an_unreachable_gateway_is_a_null_measurement() {
        let reply = call(
            "net.gatewayPing",
            r#"{"host":"192.0.2.1","port":25565,"timeoutMs":250}"#,
        );
        assert!(reply.contains("\"ok\":true"), "{reply}");
        assert!(reply.contains("\"value\":null"), "{reply}");
    }

    /// The whole path against a server that behaves: connect, handshake,
    /// status, ping, pong.
    ///
    /// The status response is written **in the same flush as the pong would
    /// be** if the peer were fast, which is the shape that produced the
    /// desktop's ~0 ms readings — a reader that keeps draining after it
    /// decides to send finds the next packet already buffered and calls it the
    /// answer. A measurement that comes back as 0 here is that bug.
    #[test]
    fn a_server_that_answers_is_measured() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");

            // Drain the handshake and status request, then answer both the
            // status and — deliberately early — nothing else.
            let mut scratch = [0u8; 512];
            let _ = socket.read(&mut scratch);

            let mut status = Vec::new();
            let body = br#"{"description":"a test"}"#;
            let mut payload = slp::varint(0x00);
            payload.extend_from_slice(&slp::varint(body.len() as u32));
            payload.extend_from_slice(body);
            status.extend_from_slice(&slp::varint(payload.len() as u32));
            status.extend_from_slice(&payload);
            socket.write_all(&status).expect("status");

            // Wait for the client's ping and echo it back, which is what a
            // real server does. The delay is what the measurement measures.
            let mut ping = [0u8; 32];
            let read = socket.read(&mut ping).expect("ping");
            std::thread::sleep(Duration::from_millis(30));
            socket.write_all(&ping[..read]).expect("pong");
        });

        let measured = measure("127.0.0.1", port, Duration::from_secs(5));
        server.join().expect("server thread");

        let ms = measured.expect("a well-behaved server was not measured");
        // The server sleeps 30 ms before answering, so a reader that mistook
        // the buffered status bytes for the pong would report far less.
        assert!(
            ms >= 25.0,
            "measured {ms} ms — the pong was read before it was sent"
        );
        assert!(ms < 2_000.0, "measured {ms} ms on loopback");
    }

    #[test]
    fn a_call_missing_its_address_says_so() {
        let reply = call("net.gatewayPing", r#"{"port":25565}"#);
        assert!(reply.contains("\"ok\":false"), "{reply}");
        assert!(reply.contains("needs a host"), "{reply}");
    }
}

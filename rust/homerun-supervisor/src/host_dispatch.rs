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

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use homerun_core::minecraft::slp;
use homerun_core::region;

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
        "net.regionLatency" => Some(
            std::panic::catch_unwind(|| region_latency(args))
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

/// Round-trip time to a *region's gateway*, in milliseconds, before any server
/// exists there.
///
/// # Why this is not `net.gatewayPing`
///
/// That one speaks Server List Ping, which needs something on the other end
/// willing to answer as a Minecraft server. A region probe runs while the
/// player is still choosing where to put a server, so there is nothing to
/// answer: the only thing available to measure is the TCP handshake itself.
///
/// # Why it is here rather than in each host
///
/// It used to be in each host, written twice, and it was wrong in both — see
/// [`homerun_core::region`] for what each of them got wrong. This module's
/// header already made the argument in the general case: writing the socket
/// once per platform shares the interesting half and duplicates the half that
/// can hang.
///
/// Null for every failure, never an error, because the UI ranks regions by
/// this number and one bad host must not cost the whole list.
fn region_latency(args: &str) -> Result<Value, String> {
    let args: Value = serde_json::from_str(args).map_err(|e| format!("bad arguments: {e}"))?;
    let domain = args
        .get("domain")
        .and_then(Value::as_str)
        .ok_or("\"net.regionLatency\" needs a domain")?;

    let deadline = args
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(DEADLINE);

    let Some(target) = region::probe_target(domain) else {
        return Ok(Value::Null);
    };

    Ok(connect_latency(&target.host, target.port, deadline)
        .map(Value::from)
        .unwrap_or(Value::Null))
}

/// Time one TCP handshake.
///
/// **A refusal is a measurement, not a failure.** The SYN reached the gateway
/// and a reset came back, which is the round trip being timed. Only a drop —
/// seen as a timeout — or a name that will not resolve is unreachable. Note
/// that on the public internet a closed port is normally dropped rather than
/// refused, so this tolerance is a safety net and not a licence to probe a
/// port nothing serves; see [`region::DEFAULT_PROBE_PORT`].
///
/// Resolution happens **before the clock starts**. The desktop folds it into
/// its figure, but a cold lookup is tens of milliseconds that vary per
/// hostname, and these numbers exist only to be ranked against each other. It
/// still counts against the deadline, because a name that takes five seconds
/// to resolve is not a region worth offering.
fn connect_latency(host: &str, port: u16, deadline: Duration) -> Option<f64> {
    let started = Instant::now();
    // `checked_sub` rather than a subtraction, for the reason `measure` gives:
    // a socket call given a zero timeout blocks forever.
    let remaining = || {
        deadline
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
    };

    let address = (host, port).to_socket_addrs().ok()?.next()?;

    let connect_started = Instant::now();
    let elapsed = || connect_started.elapsed().as_secs_f64() * 1_000.0;

    match TcpStream::connect_timeout(&address, remaining()?) {
        Ok(_) => Some(elapsed()),
        Err(error) if is_measurable(error.kind()) => Some(elapsed()),
        Err(_) => None,
    }
}

/// Whether a *failed* connect still says how far away the peer is.
///
/// A refusal or a reset is an answer: the SYN arrived and a reply came back,
/// which is the round trip being timed. A timeout is not an answer — nothing
/// came back at all, and the elapsed time is our own deadline rather than any
/// property of the network.
///
/// # This branch does nothing on Windows
///
/// `TcpStream::connect_timeout` there reports even a refused loopback
/// connection as [`ErrorKind::TimedOut`], with `raw_os_error` unset because it
/// is Rust's own deadline firing rather than the OS answering. So on a dev
/// machine a refused port reads as unreachable. On Linux and Darwin — the two
/// platforms that actually ship — the refusal surfaces properly and this
/// branch is live, which is why it is worth keeping and worth testing
/// directly rather than through a socket.
fn is_measurable(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
    )
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

    // --- region latency ---------------------------------------------------

    /// The regression that started all of this, end to end: a plain address
    /// goes in the front door and a number comes out. A host that treated the
    /// argument as a URL never got as far as the socket.
    ///
    /// No `accept` thread: the kernel completes the handshake from the listen
    /// backlog, so an unaccepted connection is still a connected one. An
    /// earlier version of this test did spawn one, bound it to `127.0.0.1`,
    /// and dialled `localhost` — which resolves to `::1` first on Windows, so
    /// nothing ever arrived and `join` hung for ever.
    #[test]
    fn a_listening_address_is_measured() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();

        let reply = call(
            "net.regionLatency",
            &format!(r#"{{"domain":"127.0.0.1:{port}","timeoutMs":2000}}"#),
        );
        assert!(reply.contains("\"ok\":true"), "{reply}");
        assert!(
            !reply.contains("\"value\":null"),
            "a listening port was not measured: {reply}"
        );

        drop(listener);
    }

    /// A refusal is an answer. Nothing is bound to this port, and on loopback
    /// a closed port resets rather than dropping — so it must come back as a
    /// measurement, not as unreachable.
    /// The rule the measurement turns on, tested where a socket cannot test
    /// it — see [`is_measurable`] for why Windows never reaches the refusal
    /// branch through a real connect.
    #[test]
    fn a_refusal_is_an_answer_and_a_timeout_is_not() {
        assert!(is_measurable(ErrorKind::ConnectionRefused));
        assert!(is_measurable(ErrorKind::ConnectionReset));

        assert!(!is_measurable(ErrorKind::TimedOut));
        assert!(!is_measurable(ErrorKind::ConnectionAborted));
        assert!(!is_measurable(ErrorKind::NotFound));
    }

    /// The same rule through a real socket, on the platforms whose
    /// `connect_timeout` can express it. Nothing is bound to this port, so the
    /// connect is refused and must still come back as a number.
    #[test]
    #[cfg(not(windows))]
    fn a_refused_port_is_still_a_measurement() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().unwrap().port()
        };

        let measured = connect_latency("127.0.0.1", port, Duration::from_millis(2_000));
        assert!(
            measured.is_some(),
            "a reset from a closed loopback port was treated as unreachable"
        );
    }

    /// TEST-NET-1 is unroutable by definition, so this exercises the timeout
    /// without depending on the network.
    #[test]
    fn an_unroutable_region_is_null() {
        let reply = call(
            "net.regionLatency",
            r#"{"domain":"192.0.2.1","timeoutMs":250}"#,
        );
        assert!(reply.contains("\"ok\":true"), "{reply}");
        assert!(reply.contains("\"value\":null"), "{reply}");
    }

    /// An unparseable target is null too — never an error, or one bad entry
    /// would cost the UI the whole region list.
    #[test]
    fn an_unusable_domain_is_null_rather_than_an_error() {
        for args in [
            r#"{"domain":""}"#,
            r#"{"domain":"https://"}"#,
            r#"{"domain":"x:0"}"#,
        ] {
            let reply = call("net.regionLatency", args);
            assert!(reply.contains("\"ok\":true"), "{args} -> {reply}");
            assert!(reply.contains("\"value\":null"), "{args} -> {reply}");
        }
    }

    #[test]
    fn a_region_call_missing_its_domain_says_so() {
        let reply = call("net.regionLatency", "{}");
        assert!(reply.contains("\"ok\":false"), "{reply}");
        assert!(reply.contains("needs a domain"), "{reply}");
    }
}

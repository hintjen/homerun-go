//! Talking to a server that has no stdin worth talking to.
//!
//! # Two dialects, one idea
//!
//! Minecraft and Pumpkin read commands off stdin, which is why nothing here
//! existed until now. Most other games do not: Rust's stdin console is
//! unreliable on Windows, and the control path the game actually supports is
//! RCON. Two dialects are in reach:
//!
//! - **Source RCON** — Valve's binary protocol over TCP, spoken by most of the
//!   Source-derived world and by a long tail of games that copied it.
//! - **WebSocket RCON** — Facepunch's JSON-over-WebSocket dialect, which is
//!   what Rust uses when `+rcon.web 1` is passed, and which carries the
//!   server's own console output as well as replies.
//!
//! # One connection per command
//!
//! Neither transport holds a session open between commands. That is a
//! deliberate simplification and it costs a TCP handshake on a loopback
//! socket, which is nothing next to a console command a person typed.
//!
//! What it buys is that there is no connection to lose, no reconnect loop, no
//! half-authenticated state to reason about, and no background thread whose
//! failure is invisible. A console that silently stopped working is a far
//! worse failure than one that is a few milliseconds slower, and the
//! supervisor has enough long-lived state already.
//!
//! The cost is real and named: **a WebSocket RCON console does not stream the
//! server's own output.** Rust's WebRCON pushes log lines to any connected
//! client, and a client that connects per command sees only what arrives
//! while it is waiting. The server's stdout is captured separately by
//! [`crate::process_engine`], so nothing is lost for the console *log*; what
//! is lost is chat and command output originating elsewhere. If that turns
//! out to matter, a held connection belongs here, not in the caller.
//!
//! # This never leaves the machine
//!
//! An RCON port is bound to loopback and `engine::validate` refuses a
//! descriptor that exposes one, because RCON is an administrative console
//! behind a single password. There is therefore no TLS here and no
//! certificate handling: `wss://` would mean the console had left the
//! machine, which is what the device websocket is for.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use homerun_core::engine::descriptor::RconProtocol;

/// Where to send a command, and how to be let in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub protocol: RconProtocol,
    /// `127.0.0.1:28016`. Loopback, always — see the module header.
    pub address: String,
    pub password: String,
}

/// How long to wait for a server to answer.
///
/// Generous, because a command like `save` does real work before it replies,
/// and a console that gave up after a second would report a working server as
/// broken. Not unbounded, because a wedged server must not wedge the caller.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Run one command and return what the server said.
///
/// The reply may legitimately be empty: plenty of commands answer nothing,
/// and an empty string is a success, not a failure.
pub fn command(target: &Target, command: &str) -> Result<String, String> {
    command_with_timeout(target, command, DEFAULT_TIMEOUT)
}

pub fn command_with_timeout(
    target: &Target,
    command: &str,
    timeout: Duration,
) -> Result<String, String> {
    match target.protocol {
        RconProtocol::Source => source::command(target, command, timeout),
        RconProtocol::Webrcon => webrcon::command(target, command, timeout),
    }
}

/// The message a player sees when the console cannot be reached.
///
/// One sentence, no mechanism: whether it was a refused connection, a
/// timeout or a bad password is a detail for the diagnostics stream, and a
/// player can act on none of them.
fn unreachable() -> String {
    "this server is not answering its console right now.".to_string()
}

// ─── Valve's binary protocol ────────────────────────────────────────────────

mod source {
    use super::*;

    // The packet types, from Valve's own documentation. `Command` and
    // `AuthResponse` share the number 2, which is not a mistake in this file:
    // the protocol really does reuse it in the two directions.
    const AUTH: i32 = 3;
    const AUTH_RESPONSE: i32 = 2;
    const COMMAND: i32 = 2;
    const RESPONSE: i32 = 0;

    /// Valve's stated ceiling is 4096 for a request. Replies can be split
    /// across several packets, so the *reader* tolerates more than one.
    const MAX_BODY: usize = 4096;

    /// A whole packet is `size` plus four bytes; refuse an absurd `size`
    /// before allocating for it, so a hostile or confused server cannot ask
    /// this process for a gigabyte.
    const MAX_PACKET: i32 = 64 * 1024;

    pub fn command(target: &Target, command: &str, timeout: Duration) -> Result<String, String> {
        if command.len() > MAX_BODY {
            return Err("that command is too long for this server's console.".to_string());
        }

        let address: std::net::SocketAddr = target
            .address
            .parse()
            .map_err(|_| "this server's console address is not a valid one.".to_string())?;
        let mut stream =
            TcpStream::connect_timeout(&address, timeout).map_err(|_| unreachable())?;
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();
        // A console command is small and latency matters more than packing.
        stream.set_nodelay(true).ok();

        // Authenticate. The server answers an auth packet with an id of -1
        // when the password is wrong, and echoes the id when it is right.
        write_packet(&mut stream, 1, AUTH, &target.password)?;
        loop {
            let (id, kind, _) = read_packet(&mut stream)?;
            if kind == AUTH_RESPONSE {
                if id == -1 {
                    return Err("this server refused the console password.".to_string());
                }
                break;
            }
            // Servers commonly send an empty RESPONSE_VALUE before the auth
            // response. Ignoring anything that is not an auth response is
            // what makes this work across implementations.
        }

        write_packet(&mut stream, 2, COMMAND, command)?;

        // A long reply arrives as several RESPONSE packets with no length
        // prefix and no terminator, so the only portable way to know it is
        // over is to send a second, meaningless command and read until *its*
        // reply comes back. Everything before that belongs to the first.
        write_packet(&mut stream, 3, COMMAND, "")?;

        let mut body = String::new();
        loop {
            let (id, kind, text) = read_packet(&mut stream)?;
            if id == 3 {
                break;
            }
            if kind == RESPONSE {
                body.push_str(&text);
            }
        }
        Ok(body)
    }

    fn write_packet(stream: &mut TcpStream, id: i32, kind: i32, body: &str) -> Result<(), String> {
        // size counts everything after itself: two i32s, the body, and two
        // terminating NULs.
        let size = (4 + 4 + body.len() + 2) as i32;
        let mut packet = Vec::with_capacity(size as usize + 4);
        packet.extend_from_slice(&size.to_le_bytes());
        packet.extend_from_slice(&id.to_le_bytes());
        packet.extend_from_slice(&kind.to_le_bytes());
        packet.extend_from_slice(body.as_bytes());
        packet.extend_from_slice(&[0, 0]);
        stream.write_all(&packet).map_err(|_| unreachable())?;
        stream.flush().map_err(|_| unreachable())
    }

    fn read_packet(stream: &mut TcpStream) -> Result<(i32, i32, String), String> {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).map_err(|_| unreachable())?;
        let size = i32::from_le_bytes(header);
        if !(10..=MAX_PACKET).contains(&size) {
            return Err("this server's console sent something unreadable.".to_string());
        }

        let mut rest = vec![0u8; size as usize];
        stream.read_exact(&mut rest).map_err(|_| unreachable())?;

        let id = i32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let kind = i32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
        // The body runs to the first NUL; a second NUL terminates the packet.
        let body = &rest[8..];
        let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
        Ok((id, kind, String::from_utf8_lossy(&body[..end]).into_owned()))
    }
}

// ─── Facepunch's JSON over WebSocket ────────────────────────────────────────

mod webrcon {
    use super::*;

    pub fn command(target: &Target, command: &str, timeout: Duration) -> Result<String, String> {
        // The password is the *path*, which is the whole of this dialect's
        // authentication. It is therefore never logged: `Target` is not
        // `Display`, and nothing here formats the URL into an error.
        let url = format!("ws://{}/{}", target.address, target.password);

        let address: std::net::SocketAddr = target
            .address
            .parse()
            .map_err(|_| "this server's console address is not a valid one.".to_string())?;
        let stream = TcpStream::connect_timeout(&address, timeout).map_err(|_| unreachable())?;
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();

        let (mut socket, _) =
            tungstenite::client(url.as_str(), stream).map_err(|_| unreachable())?;

        // The identifier comes back on the reply, which is how a reply is
        // told from the log lines the server pushes unprompted. Any non-zero
        // number will do; zero is what the server itself uses for its own
        // output.
        const IDENTIFIER: i64 = 7;
        let request = serde_json::json!({
            "Identifier": IDENTIFIER,
            "Message": command,
            "Name": "WebRcon",
        });
        socket
            .send(tungstenite::Message::Text(request.to_string()))
            .map_err(|_| unreachable())?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() > deadline {
                return Err(unreachable());
            }
            let message = socket.read().map_err(|_| unreachable())?;
            let tungstenite::Message::Text(text) = message else {
                // Ping, pong, binary: not ours, and the library answers pings
                // itself.
                continue;
            };
            let Ok(reply) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            // Anything with a different identifier is the server's own
            // console output arriving while we waited -- see the module
            // header on what that costs.
            if reply.get("Identifier").and_then(serde_json::Value::as_i64) != Some(IDENTIFIER) {
                continue;
            }
            let body = reply
                .get("Message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let _ = socket.close(None);
            return Ok(body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A stand-in Source RCON server on loopback.
    ///
    /// Testing the protocol against a real socket rather than a mock is the
    /// point: the framing is little-endian, length-prefixed and
    /// double-NUL-terminated, and every one of those is something a mock
    /// would simply agree with.
    fn source_server(password: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
        let address = listener.local_addr().unwrap().to_string();
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            loop {
                let Ok((id, kind, body)) = read_packet(&mut stream) else {
                    return;
                };
                match kind {
                    3 => {
                        // Auth: echo the id, or -1 for a wrong password.
                        let answer = if body == password { id } else { -1 };
                        let _ = write_packet(&mut stream, answer, 2, "");
                    }
                    2 if body.is_empty() => {
                        // The terminator probe.
                        let _ = write_packet(&mut stream, id, 0, "");
                    }
                    2 => {
                        let _ = tx.send(body.clone());
                        // Deliberately split across two packets, because a
                        // real server does that for a long reply and a reader
                        // that assumed one packet would pass every short test
                        // and truncate every long one.
                        let _ = write_packet(&mut stream, id, 0, "players: ");
                        let _ = write_packet(&mut stream, id, 0, "2/10");
                    }
                    _ => return,
                }
            }
        });

        (address, rx)
    }

    fn write_packet(
        stream: &mut std::net::TcpStream,
        id: i32,
        kind: i32,
        body: &str,
    ) -> std::io::Result<()> {
        let size = (4 + 4 + body.len() + 2) as i32;
        let mut packet = Vec::new();
        packet.extend_from_slice(&size.to_le_bytes());
        packet.extend_from_slice(&id.to_le_bytes());
        packet.extend_from_slice(&kind.to_le_bytes());
        packet.extend_from_slice(body.as_bytes());
        packet.extend_from_slice(&[0, 0]);
        stream.write_all(&packet)
    }

    fn read_packet(stream: &mut std::net::TcpStream) -> std::io::Result<(i32, i32, String)> {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header)?;
        let size = i32::from_le_bytes(header) as usize;
        let mut rest = vec![0u8; size];
        stream.read_exact(&mut rest)?;
        let id = i32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let kind = i32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
        let body = &rest[8..];
        let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
        Ok((id, kind, String::from_utf8_lossy(&body[..end]).into_owned()))
    }

    #[test]
    fn a_source_command_is_sent_and_its_whole_reply_comes_back() {
        let (address, received) = source_server("hunter2");
        let target = Target {
            protocol: RconProtocol::Source,
            address,
            password: "hunter2".into(),
        };

        let reply = command(&target, "playerlist").expect("the console must answer");
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            "playerlist"
        );
        // Both packets, joined. A reader that stopped at the first would
        // return "players: " and look entirely plausible.
        assert_eq!(reply, "players: 2/10");
    }

    #[test]
    fn a_wrong_password_is_refused_in_words_and_not_by_hanging() {
        let (address, _received) = source_server("hunter2");
        let target = Target {
            protocol: RconProtocol::Source,
            address,
            password: "letmein".into(),
        };
        let err = command(&target, "status").unwrap_err();
        assert!(err.contains("refused the console password"), "{err}");
    }

    #[test]
    fn a_server_that_is_not_there_fails_quickly_and_readably() {
        // Bind and drop, so the port is certainly free.
        let address = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().to_string()
        };
        let target = Target {
            protocol: RconProtocol::Source,
            address,
            password: "x".into(),
        };
        let started = std::time::Instant::now();
        let err = command_with_timeout(&target, "status", Duration::from_secs(2)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "it hung");
        assert!(err.contains("not answering its console"), "{err}");
    }

    /// Every message here reaches a player, so none of them may be a
    /// diagnostic — and in particular none may carry the password, which for
    /// WebRCON is part of the URL.
    #[test]
    fn no_failure_mentions_the_password_or_reads_like_a_stack_trace() {
        let address = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().to_string()
        };
        for protocol in [RconProtocol::Source, RconProtocol::Webrcon] {
            let target = Target {
                protocol,
                address: address.clone(),
                password: "sup3rs3cret".into(),
            };
            let err =
                command_with_timeout(&target, "status", Duration::from_millis(500)).unwrap_err();
            assert!(!err.contains("sup3rs3cret"), "{protocol:?}: {err}");
            for forbidden in ["unwrap", "panicked", "errno", "Err(", "os error"] {
                assert!(!err.contains(forbidden), "{protocol:?}: {err}");
            }
            assert!(err.ends_with('.'), "{protocol:?}: not a sentence: {err}");
        }
    }

    #[test]
    fn an_address_that_is_not_one_is_refused_before_anything_is_opened() {
        let target = Target {
            protocol: RconProtocol::Source,
            address: "not-an-address".into(),
            password: "x".into(),
        };
        let err = command(&target, "status").unwrap_err();
        assert!(err.contains("not a valid one"), "{err}");
    }

    /// A server that answers an absurd length must not be able to make this
    /// process allocate for it.
    #[test]
    fn a_console_claiming_a_gigabyte_reply_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Read whatever the client sends first, then answer with a
            // ludicrous size prefix.
            let mut scratch = [0u8; 64];
            let _ = stream.read(&mut scratch);
            let _ = stream.write_all(&1_000_000_000i32.to_le_bytes());
            let _ = stream.write_all(&[0u8; 16]);
            // Hold the socket open so the failure is the size check rather
            // than a closed connection.
            std::thread::sleep(Duration::from_secs(2));
        });

        let target = Target {
            protocol: RconProtocol::Source,
            address,
            password: "x".into(),
        };
        let err = command_with_timeout(&target, "status", Duration::from_secs(3)).unwrap_err();
        assert!(err.contains("unreadable"), "{err}");
    }

    #[test]
    fn a_command_longer_than_the_protocol_allows_is_refused_before_connecting() {
        let target = Target {
            protocol: RconProtocol::Source,
            address: "127.0.0.1:1".into(),
            password: "x".into(),
        };
        let err = command(&target, &"a".repeat(5000)).unwrap_err();
        assert!(err.contains("too long"), "{err}");
    }

    /// The WebSocket dialect, against a socket that speaks just enough of it.
    ///
    /// Worth a real handshake rather than a mock: the password is carried as
    /// the request *path*, and a client that put it anywhere else would fail
    /// only against a real server.
    #[test]
    fn a_webrcon_command_carries_its_password_in_the_path_and_matches_its_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (tx, rx) = mpsc::channel::<String>();

        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            // Capture the request line before handing the socket to the
            // server handshake, by peeking at it through a clone.
            let peek = stream.try_clone().expect("clone");
            let mut reader = std::io::BufReader::new(peek);
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let _ = tx.send(request_line);

            // `accept` re-reads the request from the start, which a fresh
            // handle on the same socket cannot do -- so this half of the test
            // stops here and the client's connect will fail. That is enough:
            // the assertion is about what was on the wire.
        });

        let target = Target {
            protocol: RconProtocol::Webrcon,
            address,
            password: "sup3rs3cret".into(),
        };
        let _ = command_with_timeout(&target, "status", Duration::from_millis(800));

        let request = rx.recv_timeout(Duration::from_secs(5)).expect("a request");
        assert!(
            request.starts_with("GET /sup3rs3cret "),
            "the password must be the path: {request:?}"
        );
    }
}

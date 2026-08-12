//! Server List Ping: the bytes that measure the round trip from this device,
//! through the gateway, to the server it is hosting.
//!
//! Reference: `nativeServerManager.ts` — `measureGatewayPing:1672` and
//! `slpPing:1683`.
//!
//! This is the number the player sees next to their server, and it is a
//! measurement of the whole path — the phone's radio, the tunnel, the gateway's
//! DNAT, and the server's own tick loop. Nothing else the app measures covers
//! that path end to end.
//!
//! # Why the codec is here and the socket is not
//!
//! The protocol is three packets out and two in, which sounds like something
//! each host can just write. The desktop's comment says otherwise, and it is
//! the reason this module exists:
//!
//! > Because Node delivers the stream in chunks and we only act once a whole
//! > length-prefixed packet is buffered, this is immune to the partial-read bug
//! > that made the Python version report ~0 ms (it read leftover status bytes
//! > as the pong).
//!
//! A status response carrying a favicon is tens of kilobytes and arrives in
//! however many TCP segments the network felt like. An implementation that
//! reads "some bytes" and calls that the status response has the tail of it
//! still queued, sees it the instant it writes the ping, and reports a round
//! trip of approximately zero — a plausible-looking number, on every server,
//! forever. That has now been got wrong once in Python and got right once in
//! TypeScript; writing the same state machine by hand in Kotlin and again in
//! Swift is how it gets got wrong again.
//!
//! So the framing lives here as [`Reader`], and the host keeps the socket and
//! the clock. This crate has no clock, which is fine: the only two instants
//! that matter are the ones either side of [`Step::SendPing`] and
//! [`Step::Pong`], and the host is holding both.
//!
//! ```text
//!   connect, write slp::open(host, port)
//!   loop {
//!       read some bytes  →  reader.feed(&bytes)
//!       loop {
//!           match reader.step() {
//!               NeedMore  => break,                       // read again
//!               SendPing  => { t0 = now(); write(slp::ping(0)) }
//!               Pong      => return now() - t0,
//!               Malformed => return None,
//!           }
//!       }
//!   }
//! ```
//!
//! # Java only
//!
//! The desktop pings Bedrock with a RakNet Unconnected Ping over UDP
//! (`raknetPing:1769`); mobile hosts Java servers only, so that path is not
//! ported. It is a different transport, a different packet and a different
//! reply, and it shares nothing with this module but the caller.

use serde::{Deserialize, Serialize};

/// The protocol version in the handshake.
///
/// 47 is 1.8. A status ping is served before any version negotiation happens,
/// so every server from 1.7 onwards answers whatever is claimed here — the
/// desktop, the device container and this module all say 47 so the number in
/// the UI means the same thing wherever it was measured.
pub const PROTOCOL_VERSION: u32 = 47;

/// Handshake next-state 1: status. (2 would be login.)
const NEXT_STATE_STATUS: u32 = 1;

/// The largest packet this reader will wait for.
///
/// The protocol allows up to 2 GiB and vanilla's status response is a few
/// kilobytes even with a 64×64 favicon. Not in the desktop, which is backed by
/// a machine with swap; a phone told to buffer a length-prefix somebody made up
/// dies to the OOM killer with no crash report, and the ping is the least
/// important thing the app is doing.
pub const MAX_PACKET: usize = 2 * 1024 * 1024;

/// Append a VarInt to `out`.
///
/// Seven bits per byte, little-endian, high bit set on every byte but the last.
pub fn put_varint(out: &mut Vec<u8>, value: u32) {
    let mut remaining = value;
    loop {
        let mut byte = (remaining & 0x7f) as u8;
        remaining >>= 7;
        if remaining != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if remaining == 0 {
            return;
        }
    }
}

/// A VarInt on its own.
pub fn varint(value: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    put_varint(&mut out, value);
    out
}

/// What [`read_varint`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VarInt {
    /// A complete VarInt, and the offset just past its last byte.
    Read { value: u32, next: usize },
    /// The buffer ended mid-VarInt. Ask again once more has arrived.
    NeedMore,
    /// More continuation bytes than a 32-bit VarInt can hold. Not a partial
    /// read — this stream will never become valid, so a caller that waits for
    /// more bytes waits until its own timeout.
    Malformed,
}

/// Read a VarInt at `offset`.
///
/// Distinguishing [`VarInt::NeedMore`] from [`VarInt::Malformed`] is the point.
/// The desktop returns `null` for both, so a peer that is not speaking this
/// protocol at all is indistinguishable from a slow one and the ping sits there
/// until the 5-second socket timeout instead of failing immediately.
pub fn read_varint(buffer: &[u8], offset: usize) -> VarInt {
    let mut value: u32 = 0;

    // Five bytes is the whole of a 32-bit VarInt: shifts of 0, 7, 14, 21, 28.
    for (index, shift) in [0u32, 7, 14, 21, 28].into_iter().enumerate() {
        let Some(byte) = buffer.get(offset + index) else {
            return VarInt::NeedMore;
        };
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return VarInt::Read {
                value,
                next: offset + index + 1,
            };
        }
    }
    VarInt::Malformed
}

/// Wrap a payload in its length prefix.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = varint(payload.len() as u32);
    out.extend_from_slice(payload);
    out
}

/// The handshake: packet 0x00, the protocol version, the address the client
/// dialled, and next-state status.
///
/// `host` and `port` are what the *client* used to reach the server — the
/// gateway's hostname and its external port, not the local ones. A server
/// behind a proxy routes on this, so a handshake naming `127.0.0.1` reaches a
/// different server or none.
pub fn handshake(host: &str, port: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(host.len() + 8);
    put_varint(&mut payload, 0x00);
    put_varint(&mut payload, PROTOCOL_VERSION);
    put_varint(&mut payload, host.len() as u32);
    payload.extend_from_slice(host.as_bytes());
    payload.extend_from_slice(&port.to_be_bytes());
    put_varint(&mut payload, NEXT_STATE_STATUS);
    frame(&payload)
}

/// The status request: packet 0x00 with no body. The server answers with the
/// JSON the multiplayer list shows.
pub fn status_request() -> Vec<u8> {
    frame(&[0x00])
}

/// The ping: packet 0x01 and an 8-byte payload the server echoes back verbatim.
///
/// The payload is conventionally a timestamp and is never read by anything
/// here — the host times the exchange itself, so the desktop sends zeroes and
/// so may you.
pub fn ping(payload: u64) -> Vec<u8> {
    let mut body = Vec::with_capacity(9);
    body.push(0x01);
    body.extend_from_slice(&payload.to_be_bytes());
    frame(&body)
}

/// [`handshake`] and [`status_request`] together, which is how they are
/// written: one `write` on connect, so the server sees both in the first
/// segment and the measured round trip is the ping's alone.
pub fn open(host: &str, port: u16) -> Vec<u8> {
    let mut out = handshake(host, port);
    out.extend_from_slice(&status_request());
    out
}

/// What the host should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    /// Nothing is complete yet. Read from the socket and feed it back.
    NeedMore,
    /// The status response is fully drained. Start the clock and write
    /// [`ping`] — in that order, since the write is part of what is measured.
    SendPing,
    /// The pong came back. Stop the clock; that difference is the round trip.
    Pong,
    /// This is not a status stream. Give up — waiting cannot fix it.
    Malformed,
}

/// The response side of the exchange, fed bytes as they arrive.
///
/// A [`Step`] is returned per complete packet, never per chunk, which is the
/// whole reason this is a struct and not a pair of functions. See the module
/// doc for what happens to implementations that conflate the two.
///
/// [`Step::Pong`] and [`Step::Malformed`] are terminal and repeat: once it has
/// answered either, it answers the same thing forever rather than starting a
/// second measurement out of whatever arrives next.
#[derive(Debug, Default)]
pub struct Reader {
    buffer: Vec<u8>,
    stage: Stage,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Draining the status response.
    #[default]
    Status,
    /// The ping has been written; the next complete packet is the pong.
    Pong,
    Ended(Step),
}

impl Reader {
    pub fn new() -> Self {
        Reader::default()
    }

    /// Take bytes off the socket. Any length, including none.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// Consume one complete packet, if one is buffered.
    ///
    /// Call it in a loop until it answers [`Step::NeedMore`]. One chunk can
    /// carry more than one packet, and a host that acts on only the first
    /// answer per read can deadlock waiting for bytes that are already in hand.
    pub fn step(&mut self) -> Step {
        if let Stage::Ended(step) = self.stage {
            return step;
        }

        let (length, header_end) = match read_varint(&self.buffer, 0) {
            VarInt::Read { value, next } => (value as usize, next),
            VarInt::NeedMore => return Step::NeedMore,
            VarInt::Malformed => return self.end(Step::Malformed),
        };
        if length > MAX_PACKET {
            return self.end(Step::Malformed);
        }
        // Bounded by the cap above, so the sum cannot wrap.
        let end = header_end + length;
        if self.buffer.len() < end {
            // The body has not fully arrived. Not an error, and not a packet —
            // the ~0 ms bug is this branch being skipped, and consuming what
            // happens to be buffered instead. Deriving the drain from `end`
            // rather than from the buffer's length is what keeps the two in
            // step.
            return Step::NeedMore;
        }
        self.buffer.drain(..end);

        match self.stage {
            Stage::Status => {
                self.stage = Stage::Pong;
                Step::SendPing
            }
            _ => self.end(Step::Pong),
        }
    }

    /// Bytes held but not yet part of a complete packet. Diagnostics only.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    fn end(&mut self, step: Step) -> Step {
        self.stage = Stage::Ended(step);
        self.buffer.clear();
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every encoding on the wiki's VarInt table that fits in 32 bits, plus the
    /// port and packet lengths this module actually emits. Wrong bytes here are
    /// a handshake no server answers.
    const VECTORS: &[(u32, &[u8])] = &[
        (0, &[0x00]),
        (1, &[0x01]),
        (2, &[0x02]),
        (47, &[0x2f]),
        (127, &[0x7f]),
        (128, &[0x80, 0x01]),
        (255, &[0xff, 0x01]),
        (300, &[0xac, 0x02]),
        (16383, &[0xff, 0x7f]),
        (16384, &[0x80, 0x80, 0x01]),
        (25565, &[0xdd, 0xc7, 0x01]),
        (2097151, &[0xff, 0xff, 0x7f]),
        (2147483647, &[0xff, 0xff, 0xff, 0xff, 0x07]),
        (4294967295, &[0xff, 0xff, 0xff, 0xff, 0x0f]),
    ];

    #[test]
    fn varints_encode_to_the_bytes_the_protocol_specifies() {
        for (value, bytes) in VECTORS {
            assert_eq!(varint(*value), *bytes, "encoding {value}");
        }
    }

    #[test]
    fn every_varint_reads_back_as_itself() {
        for (value, bytes) in VECTORS {
            assert_eq!(
                read_varint(bytes, 0),
                VarInt::Read {
                    value: *value,
                    next: bytes.len()
                },
                "decoding {value}"
            );
        }
    }

    /// A VarInt rarely sits at offset zero — it is the length prefix of the
    /// packet after the one just consumed.
    #[test]
    fn a_varint_is_read_from_an_offset() {
        let mut buffer = vec![0xde, 0xad];
        buffer.extend_from_slice(&varint(300));
        buffer.push(0xbe);
        assert_eq!(
            read_varint(&buffer, 2),
            VarInt::Read {
                value: 300,
                next: 4
            }
        );
    }

    /// The truncation case. Saying "need more" is what lets the reader wait; a
    /// decoder that returned 0 here would frame the next packet at the wrong
    /// offset and never recover.
    #[test]
    fn a_varint_cut_in_half_asks_for_more() {
        for (value, bytes) in VECTORS {
            for cut in 0..bytes.len() {
                assert_eq!(
                    read_varint(&bytes[..cut], 0),
                    VarInt::NeedMore,
                    "{value} truncated to {cut} of {} bytes",
                    bytes.len()
                );
            }
        }
        assert_eq!(read_varint(&[], 0), VarInt::NeedMore);
        assert_eq!(read_varint(&[0x01], 5), VarInt::NeedMore);
    }

    /// Six continuation bytes cannot be a 32-bit VarInt. It must be rejected
    /// rather than looped over — and rather than reported as truncation, which
    /// makes the host wait out its whole timeout for a stream that will never
    /// parse.
    #[test]
    fn a_varint_that_never_ends_is_refused_not_awaited() {
        assert_eq!(
            read_varint(&[0xff; 6], 0),
            VarInt::Malformed,
            "a sixth continuation byte was accepted, or read as a short buffer"
        );
        assert_eq!(read_varint(&[0x80; 32], 0), VarInt::Malformed);
        assert_eq!(
            read_varint(&[0xff; 5], 0),
            VarInt::Malformed,
            "the fifth byte still asking for a sixth is malformed, not short"
        );
    }

    // ─── the packets ────────────────────────────────────────────────────────

    /// Pinned byte for byte. A server silently drops a handshake it cannot
    /// read rather than complaining, so every field here has the same symptom
    /// when it is wrong: no ping, on every server, with nothing in any log.
    #[test]
    fn the_handshake_is_the_bytes_a_server_expects() {
        assert_eq!(
            handshake("mc.gethomerun.app", 33050),
            [
                &[
                    0x17, // packet length: 23
                    0x00, // packet id
                    0x2f, // protocol 47
                    0x11, // hostname length: 17
                ][..],
                b"mc.gethomerun.app",
                &[
                    0x81, 0x1a, // port 33050, big-endian
                    0x01, // next state: status
                ],
            ]
            .concat()
        );
    }

    /// The gateway's external port routinely exceeds 32767, which is where a
    /// signed 16-bit write would wrap into a negative and reach nothing.
    #[test]
    fn a_high_port_survives_the_handshake() {
        let bytes = handshake("h", 65535);
        assert_eq!(&bytes[bytes.len() - 3..], &[0xff, 0xff, 0x01]);
    }

    #[test]
    fn the_status_request_and_ping_are_their_documented_shapes() {
        assert_eq!(status_request(), [0x01, 0x00]);
        assert_eq!(ping(0), [0x09, 0x01, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            ping(0x0123_4567_89ab_cdef),
            [0x09, 0x01, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
            "the payload is big-endian, as the server echoes it"
        );
    }

    #[test]
    fn opening_writes_the_handshake_then_the_status_request() {
        let mut expected = handshake("h", 1);
        expected.extend_from_slice(&status_request());
        assert_eq!(open("h", 1), expected);
    }

    // ─── the reader ─────────────────────────────────────────────────────────

    /// A status response of `size` bytes of body, framed.
    fn status_response(size: usize) -> Vec<u8> {
        let mut body = vec![0x00]; // packet id
        body.resize(size + 1, b'x');
        frame(&body)
    }

    fn pong() -> Vec<u8> {
        ping(0)
    }

    /// **The regression this module exists for.** A byte at a time is the
    /// pathological case of "the stream arrives in chunks": if the reader ever
    /// mistakes a partial packet for a complete one it will say `SendPing` or
    /// `Pong` early, and the desktop's comment records what that produced — a
    /// round trip of ~0 ms, on every server, looking entirely plausible.
    #[test]
    fn a_response_delivered_one_byte_at_a_time_still_reaches_the_pong() {
        let mut stream = status_response(4096);
        let status_bytes = stream.len();
        stream.extend_from_slice(&pong());

        let mut reader = Reader::new();
        let mut ping_sent_after = None;
        let mut pong_after = None;

        for (index, byte) in stream.iter().enumerate() {
            reader.feed(&[*byte]);
            loop {
                match reader.step() {
                    Step::NeedMore => break,
                    Step::SendPing => {
                        assert!(
                            ping_sent_after.is_none(),
                            "the ping was written twice, so a packet boundary was invented"
                        );
                        ping_sent_after = Some(index + 1);
                    }
                    Step::Pong => {
                        pong_after = Some(index + 1);
                        break;
                    }
                    Step::Malformed => panic!("a well-formed stream was refused at byte {index}"),
                }
            }
            if pong_after.is_some() {
                break;
            }
        }

        assert_eq!(
            ping_sent_after,
            Some(status_bytes),
            "the ping went out after {ping_sent_after:?} of {status_bytes} status bytes — \
             an incomplete status response was read as a complete one, which is the \
             measurement that reports ~0 ms"
        );
        assert_eq!(
            pong_after,
            Some(stream.len()),
            "the pong was reported before its last byte arrived"
        );
    }

    /// The same stream in one delivery: nothing about the framing may depend on
    /// how the network happened to split it.
    #[test]
    fn the_same_stream_in_one_chunk_reaches_the_same_place() {
        let mut stream = status_response(4096);
        stream.extend_from_slice(&pong());

        let mut reader = Reader::new();
        reader.feed(&stream);
        assert_eq!(reader.step(), Step::SendPing);
        assert_eq!(reader.step(), Step::Pong);
    }

    /// The Python bug in miniature: the tail of the status response is still
    /// queued when the ping goes out, and gets read as the pong.
    #[test]
    fn leftover_status_bytes_are_not_a_pong() {
        let status = status_response(200);
        let mut reader = Reader::new();

        reader.feed(&status[..150]);
        assert_eq!(
            reader.step(),
            Step::NeedMore,
            "150 bytes of a 200-byte status response were read as the whole of it, \
             so the ping goes out early and the rest of the response answers it"
        );

        reader.feed(&status[150..]);
        assert_eq!(reader.step(), Step::SendPing);
        assert_eq!(
            reader.step(),
            Step::NeedMore,
            "the rest of the status response was counted as the pong"
        );
    }

    /// Chunk boundaries falling inside the length prefix itself, which is where
    /// a decoder that treats a short read as a zero length goes wrong.
    #[test]
    fn a_length_prefix_split_across_chunks_is_reassembled() {
        let status = status_response(300); // a two-byte length prefix
        let mut reader = Reader::new();

        reader.feed(&status[..1]);
        assert_eq!(reader.step(), Step::NeedMore, "half a length prefix");
        reader.feed(&status[1..]);
        assert_eq!(reader.step(), Step::SendPing);
    }

    #[test]
    fn an_empty_feed_changes_nothing() {
        let mut reader = Reader::new();
        reader.feed(&[]);
        assert_eq!(reader.step(), Step::NeedMore);
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn a_stream_that_is_not_this_protocol_is_refused_rather_than_awaited() {
        let mut reader = Reader::new();
        reader.feed(&[0xff; 6]); // a length prefix that never terminates
        assert_eq!(
            reader.step(), Step::Malformed,
            "a stream that will never parse was reported as needing more bytes, \n             so the host waits out its whole timeout"
        );
        assert_eq!(
            reader.step(),
            Step::Malformed,
            "a terminal answer must not become something else"
        );
    }

    /// A hostile or confused peer must not be able to make a phone buffer
    /// until the OS kills the app.
    #[test]
    fn an_impossible_packet_length_is_refused_immediately() {
        let mut reader = Reader::new();
        reader.feed(&varint(MAX_PACKET as u32 + 1));
        assert_eq!(
            reader.step(),
            Step::Malformed,
            "the reader agreed to buffer a packet larger than the cap"
        );
        assert_eq!(reader.buffered(), 0, "nothing is held for a dead stream");
    }

    #[test]
    fn the_pong_is_final() {
        let mut reader = Reader::new();
        reader.feed(&status_response(4));
        reader.feed(&pong());
        assert_eq!(reader.step(), Step::SendPing);
        assert_eq!(reader.step(), Step::Pong);

        reader.feed(&pong());
        assert_eq!(
            reader.step(),
            Step::Pong,
            "a second measurement was started from a stray packet"
        );
    }

    /// An empty packet is legal framing — length 0, no id — and must not be
    /// read as the end of the buffer.
    #[test]
    fn a_zero_length_packet_is_still_a_packet() {
        let mut reader = Reader::new();
        reader.feed(&[0x00, 0x00]);
        assert_eq!(reader.step(), Step::SendPing);
        assert_eq!(reader.step(), Step::Pong);
    }
}

//! Reading the PROXY v1 header the legacy gateway puts in front of TLS.
//!
//! Reference: `deviceWebsocket/tls/proxyprotocol.ts` in the `homerun` repo, and
//! the [PROXY protocol spec][spec] §2.1.
//!
//! [spec]: https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt
//!
//! # Why this is worth its own file
//!
//! The legacy plane is nginx with `proxy_protocol on`, which writes one ASCII
//! line **ahead of the TLS ClientHello**. The v2 gateway runs HAProxy with
//! `real_ip_mode=none` and writes nothing. So a device has to handle both, and
//! both ways of getting it wrong are silent:
//!
//! - not stripping a header that *is* there feeds `PROXY TCP4 …` to the TLS
//!   parser as if it were a ClientHello, and every handshake fails;
//! - stripping one that is *not* there eats the first bytes of a real
//!   ClientHello, and every handshake fails.
//!
//! Neither produces a message about PROXY headers. That is why the parser is
//! here, pure and tested, rather than a few lines inside a socket loop.
//!
//! Only v1 is implemented. nginx emits nothing else, and a v2 (binary) parser
//! would be untested code guarding against a case that cannot arise.

/// The longest a v1 header can be, including CRLF. Anything longer is not one.
pub const MAX_HEADER: usize = 108;

const PREFIX: &[u8] = b"PROXY ";

/// What the bytes at the front of a connection turn out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preface {
    /// A complete header. Skip `consumed` bytes; everything after is the
    /// client's own first byte.
    Header {
        consumed: usize,
        /// The header line without its CRLF, for logging. Not parsed further:
        /// the device does not make decisions from the client's address, and a
        /// field it does not use is a field that cannot be wrong.
        line: String,
    },
    /// Not a PROXY header at all — these bytes belong to the client. This is
    /// the answer for every connection on the v2 plane, and consuming anything
    /// here would break the handshake.
    Absent,
    /// Could still become a header. Read more and ask again.
    Incomplete,
}

/// Inspect the front of a connection.
///
/// Call with everything read so far, from the very first byte. [`Preface::Incomplete`]
/// means read more and call again with the longer buffer.
pub fn read(buffer: &[u8]) -> Preface {
    // A TLS ClientHello starts with 0x16, so the very first byte usually
    // settles this. Comparing what we have against the prefix handles a
    // buffer shorter than "PROXY " without guessing.
    let compared = buffer.len().min(PREFIX.len());
    if buffer[..compared] != PREFIX[..compared] {
        return Preface::Absent;
    }
    if buffer.len() < PREFIX.len() {
        return Preface::Incomplete;
    }

    match find_crlf(buffer) {
        Some(end) => Preface::Header {
            consumed: end + 2,
            line: String::from_utf8_lossy(&buffer[..end]).into_owned(),
        },
        // No terminator yet. Past the spec's maximum it never will be, and
        // treating it as `Absent` is the safe answer: a peer that opened with
        // "PROXY " and then sent a hundred bytes of something else is not
        // speaking this protocol, and the TLS handshake will reject it with a
        // better message than we could invent.
        None if buffer.len() >= MAX_HEADER => Preface::Absent,
        None => Preface::Incomplete,
    }
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|pair| pair == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &[u8] = b"PROXY TCP4 192.0.2.1 10.0.0.2 56324 443\r\n";

    #[test]
    fn a_complete_header_is_consumed_exactly() {
        let mut buffer = HEADER.to_vec();
        buffer.extend_from_slice(&[0x16, 0x03, 0x01]); // the ClientHello behind it
        match read(&buffer) {
            Preface::Header { consumed, line } => {
                assert_eq!(consumed, HEADER.len());
                assert_eq!(line, "PROXY TCP4 192.0.2.1 10.0.0.2 56324 443");
                assert_eq!(
                    &buffer[consumed..],
                    &[0x16, 0x03, 0x01],
                    "one byte out either way and the handshake fails with nothing to say why"
                );
            }
            other => panic!("expected a header, got {other:?}"),
        }
    }

    /// The v2 plane sends no header, so this is the common case and the one
    /// that must never consume anything.
    #[test]
    fn a_tls_client_hello_is_left_alone() {
        assert_eq!(read(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01]), Preface::Absent);
    }

    /// A one-byte read must already be able to say "not a header", or the
    /// caller waits for a second byte that a TLS client has no reason to send
    /// before it has been answered.
    #[test]
    fn a_single_non_matching_byte_settles_it() {
        assert_eq!(read(&[0x16]), Preface::Absent);
        assert_eq!(
            read(b"G"),
            Preface::Absent,
            "an HTTP request is not a header"
        );
    }

    #[test]
    fn a_partial_prefix_is_not_yet_an_answer() {
        assert_eq!(read(b"PRO"), Preface::Incomplete);
        assert_eq!(read(b"PROXY"), Preface::Incomplete);
    }

    #[test]
    fn a_header_split_across_reads_completes() {
        let (first, second) = HEADER.split_at(12);
        assert_eq!(read(first), Preface::Incomplete);

        let mut joined = first.to_vec();
        joined.extend_from_slice(second);
        assert!(matches!(read(&joined), Preface::Header { .. }));
    }

    /// nginx sends this when it cannot describe the connection. It is still a
    /// header and still has to come off.
    #[test]
    fn the_unknown_form_is_still_a_header() {
        match read(b"PROXY UNKNOWN\r\n\x16\x03\x01") {
            Preface::Header { consumed, line } => {
                assert_eq!(consumed, 15);
                assert_eq!(line, "PROXY UNKNOWN");
            }
            other => panic!("expected a header, got {other:?}"),
        }
    }

    /// A peer that opens with the prefix and never terminates the line is not
    /// speaking this protocol. Waiting for ever is the wrong answer; so is
    /// consuming a fixed guess.
    #[test]
    fn a_prefix_with_no_terminator_stops_being_a_header() {
        let runaway = [b"PROXY ".to_vec(), vec![b'x'; MAX_HEADER]].concat();
        assert_eq!(read(&runaway), Preface::Absent);
    }

    #[test]
    fn an_empty_buffer_asks_for_more() {
        assert_eq!(read(&[]), Preface::Incomplete);
    }
}

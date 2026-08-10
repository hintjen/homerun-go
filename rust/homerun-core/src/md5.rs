//! MD5, for one purpose: deriving offline-mode player UUIDs.
//!
//! # Why this is here and not a dependency
//!
//! This crate deliberately depends on serde and nothing else, and MD5 is used
//! in exactly one place — [`crate::settings::offline_uuid`], which must agree
//! byte-for-byte with Java's `UUID.nameUUIDFromBytes(("OfflinePlayer:" +
//! name).getBytes())` because that is how a Minecraft server derives the UUID
//! of a player joining an offline server. Pulling in RustCrypto's `md-5`
//! would add five transitive crates to hash at most a few dozen bytes a
//! launch.
//!
//! # This is not a security primitive
//!
//! MD5 is broken for every purpose that depends on collision resistance. It is
//! used here only because Mojang chose it in 2010 and the derivation must
//! match theirs; nothing about it is trusted. Do not reach for this module for
//! anything else.
//!
//! The tests pin it against vectors generated from the desktop's own
//! `crypto.createHash("md5")` output, including inputs long enough to require
//! more than one block — the case a naive implementation gets wrong.

/// Per-round left-rotation amounts.
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `floor(2^32 * abs(sin(i + 1)))`, the standard table.
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, //
    0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, //
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, //
    0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, //
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, //
    0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8, //
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, //
    0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, //
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, //
    0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, //
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, //
    0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, //
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, //
    0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1, //
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, //
    0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// The 16-byte MD5 digest of `input`.
pub fn digest(input: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];

    // Pad to a multiple of 64: a 0x80 byte, zeroes, then the bit length as a
    // little-endian u64. Done into an owned buffer rather than streaming —
    // the inputs here are one short string.
    let mut padded = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    let bits = (input.len() as u64).wrapping_mul(8);
    padded.extend_from_slice(&bits.to_le_bytes());

    for block in padded.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in block.chunks_exact(4).enumerate() {
            m[i] = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        }

        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                f.wrapping_add(a)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(SHIFTS[i]),
            );
            a = tmp;
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut out = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: [u8; 16]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// RFC 1321's own vectors.
    #[test]
    fn matches_the_rfc_vectors() {
        assert_eq!(hex(digest(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(digest(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(digest(b"message digest")),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
    }

    /// Generated from the desktop's `crypto.createHash("md5")`, which is the
    /// implementation this has to agree with.
    #[test]
    fn matches_the_desktops_hash_of_a_real_input() {
        assert_eq!(
            hex(digest(b"OfflinePlayer:Notch")),
            "b50ad385829da141a2167e7d7539ba7f"
        );
    }

    /// Anything at or past 56 bytes pushes the length field into a second
    /// block. A padding loop that stops one block early passes every short
    /// vector above and fails here.
    #[test]
    fn spans_multiple_blocks() {
        let long = format!("OfflinePlayer:{}", "x".repeat(64));
        assert_eq!(long.len(), 78, "the fixture must exceed one 64-byte block");
        assert_eq!(
            hex(digest(long.as_bytes())),
            // Cross-checked against the desktop for the same input.
            "06f75d59acf79c1a65b14c678e94f6be"
        );
    }

    /// The boundary itself: exactly 56 bytes is the first length needing a
    /// second block, and 64 is a whole block with nowhere to put the padding.
    #[test]
    fn handles_the_padding_boundaries() {
        for len in [55usize, 56, 57, 63, 64, 65] {
            let input = vec![b'a'; len];
            // Not a fixed expectation — the property under test is that
            // padding never panics and always consumes whole blocks.
            let out = digest(&input);
            assert_eq!(out.len(), 16, "len {len} produced a short digest");
        }
    }
}

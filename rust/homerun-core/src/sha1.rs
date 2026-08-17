//! SHA-1, for one purpose: naming the plugin jars we host ourselves.
//!
//! # Why this is here and not a dependency
//!
//! Same argument as [`crate::md5`], and the same shape. This crate depends on
//! serde and one signature verifier, and SHA-1 is used in exactly one place —
//! [`crate::minecraft::minigame::custom_plugins`], which must agree
//! byte-for-byte with the desktop's
//! `crypto.createHash("sha1").update(url).digest("hex").slice(0, 12)` because
//! that is the filename a plugin jar already has on disk in server directories
//! the desktop wrote.
//!
//! That agreement is not cosmetic. A server directory is what restic backs up
//! — the whole of it, `plugins/` included — so a player who hosts a Paper
//! server on their PC and then starts it on their phone restores that
//! directory onto the phone. Two platforms naming the same jar differently
//! would leave `plugins/` holding two copies of one plugin, and Bukkit refuses
//! to load a plugin whose name it has already seen. The server would stop
//! booting, on the second device, for a reason visible nowhere near the code
//! that caused it.
//!
//! # This is not a security primitive
//!
//! SHA-1 is broken for collision resistance and must not be reached for
//! anywhere that matters. It is used here only because the desktop chose it to
//! derive a filename, and the derivation must match; nothing about it is
//! trusted. [`crate::bundle`] is where a real signature lives, and it uses
//! ed25519 for exactly this reason.
//!
//! The tests pin it against the standard FIPS vectors *and* against digests
//! generated from the desktop's own `crypto.createHash("sha1")` for the URLs
//! this is actually called with — including inputs long enough to need more
//! than one block, which is the case a naive implementation gets wrong.

/// The five chaining values, as FIPS 180-4 defines them.
const INIT: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

/// One constant per twenty rounds.
const K: [u32; 4] = [0x5A827999, 0x6ED9EBA1, 0x8F1BBCDC, 0xCA62C1D6];

/// Lowercase hex of the SHA-1 of `input`.
pub fn hex(input: &[u8]) -> String {
    let mut h = INIT;

    // Padding: one set bit, zeros to 56 bytes mod 64, then the length in
    // *bits* as a big-endian u64. Built into an owned buffer rather than
    // streamed because everything hashed here is a URL — tens of bytes.
    let mut message = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        // The message schedule. This rotation is the whole difference between
        // SHA-1 and SHA-0, and omitting it is the classic silent bug — which
        // is why the multi-block vectors below are not optional.
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = h;

        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), K[0]),
                20..=39 => (b ^ c ^ d, K[1]),
                40..=59 => ((b & c) | (b & d) | (c & d), K[2]),
                _ => (b ^ c ^ d, K[3]),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        for (slot, value) in h.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4's own vectors: empty, one block, and the two that straddle
    /// the padding boundary.
    #[test]
    fn the_standard_vectors_match() {
        assert_eq!(hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
        );
        // `\` before the newline swallows it and the indentation after it, so
        // this is one 112-byte string.
        assert_eq!(
            hex(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn\
                  hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"),
            "a49b2446a02c645bf419f995b67091253a04a259",
        );
    }

    /// A length that needs many blocks, and the one input where an
    /// off-by-one in the bit-length encoding shows up.
    #[test]
    fn a_million_blocks_of_padding_are_counted_right() {
        assert_eq!(
            hex(&[b'a'; 1_000_000]),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f",
        );
    }

    /// The vectors that actually matter: generated by running
    /// `crypto.createHash("sha1")` — the desktop's own call — over the resolve
    /// URLs the API hands out. If this test fails, a phone and a PC will name
    /// the same plugin jar differently.
    #[test]
    fn the_desktops_digest_of_a_real_resolve_url_is_reproduced() {
        assert_eq!(
            hex(b"https://api.gethomerun.app/api/minigame/plugins/\
                  homerun-minigames/download/?channel=release"),
            "dafc06fcd9c7706852e217d70f37942d815ab5dc",
        );
        assert_eq!(
            hex(b"https://api.gethomerun.app/api/minigame/plugins/\
                  homerun-bedwars/download/?channel=development"),
            "c8959364b5724a7acdd11da19f911ee7aa12bbac",
        );
        assert_eq!(
            hex(b"not a url at all"),
            "1edc88d0873afac24615984afcc31c74ec6a7d16",
        );
    }
}

//! Judging an over-the-air UI bundle before a host is allowed to install it.
//!
//! # Why this is in Rust and not in each host
//!
//! `plans/ota-updates.md`: the payload delivered over the air is *the entire
//! user interface*, so a bundle we accept wrongly is an app we have replaced
//! wrongly. Two hosts each deciding that for themselves is the same setup that
//! already produced the `jar::paper` drift — except the failure here is not an
//! alpha build, it is a signature check that one platform performs slightly
//! more loosely than the other.
//!
//! There is also a concrete reason Android cannot do this itself: `minSdk` is
//! 26 and the platform only gained Ed25519 at API 33. Verifying in the host
//! would mean bundling a crypto provider for the seven API levels below that,
//! and then writing the same logic again in Swift.
//!
//! So the whole judgement lives here — parse, verify, compare — and the hosts
//! keep what only they can do: the HTTPS request, the digest of a file on
//! disk, the unzip, the rename.
//!
//! # The one dependency
//!
//! This crate otherwise depends on serde and nothing else, and [`crate::md5`]
//! exists precisely to avoid a dependency. That precedent does **not** extend
//! here. MD5 is a few dozen lines of table lookups that a test vector proves
//! correct; Ed25519 is field arithmetic where a subtly wrong implementation
//! still verifies every honest signature and silently accepts forged ones.
//! Hand-rolling it would be the worst possible application of the rule that
//! produced `md5.rs`, so this module takes `ed25519-dalek`.
//!
//! # What is deliberately *not* here
//!
//! SHA-256 of the downloaded archive. Every host already has a correct,
//! hardware-accelerated implementation (`MessageDigest`, `CryptoKit`,
//! `node:crypto`) and the file is streamed to disk in chunks, so hashing it
//! here would mean either crossing the FFI once per chunk or holding the whole
//! archive in memory to cross it once. The host produces the digest; this
//! module only says whether it is the digest that was signed — see
//! [`digest_matches`], which exists so that comparison cannot be written three
//! subtly different ways.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// How much of a manifest is signed, and in what form. Bumping this changes
/// the bytes covered by the signature, so old and new never validate against
/// each other by accident.
const PAYLOAD_VERSION: &str = "homerun-bundle-v1";

/// What the server offered.
///
/// Field names match the JSON in `plans/ota-updates.md` §Shape. `serial` and
/// `platform` are additions — see [`Manifest::signing_payload`] and
/// [`Verdict::Downgrade`] for what each one closes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Human-meaningful id, e.g. `2026-08-14.1`. Names the directory on disk
    /// and is what a bug report can be matched against. Ordering is *not*
    /// derived from it — see `serial`.
    pub bundle: String,
    /// Where the archive is. Checked against `sha256` after download.
    pub url: String,
    /// Lowercase hex, 64 characters, of the archive as served.
    pub sha256: String,
    /// The lowest `BRIDGE_HOST_REVISION` that can run this bundle.
    #[serde(rename = "minHost")]
    pub min_host: u32,
    /// Monotonic across every release on a channel. This is the ordering, and
    /// the reason ids are free to be dates.
    pub serial: u64,
    /// `android` or `ios`. Signed, so a manifest cannot be replayed at the
    /// other platform.
    pub platform: String,
    /// Ed25519 over [`Manifest::signing_payload`], lowercase hex, 128 chars.
    pub signature: String,
}

impl Manifest {
    /// Exactly the bytes the signature covers.
    ///
    /// **Line-oriented, not canonical JSON, on purpose.** Signing a JSON
    /// document means the signer and the verifier must agree on key order,
    /// unicode escaping, and how integers are rendered — and when they
    /// disagree the failure is a valid signature that will not verify, which
    /// is indistinguishable from an attack and will be "fixed" by someone
    /// relaxing the check. A field-per-line payload has one representation.
    ///
    /// Every field except the signature itself is covered. In particular:
    ///
    /// - `url`, so a valid manifest cannot be re-pointed at another archive
    /// - `sha256`, which is what actually binds the manifest to the bytes
    /// - `min_host`, or the safety rail could be lowered in transit
    /// - `platform` and `serial`, which close replay and downgrade
    pub fn signing_payload(&self) -> String {
        // Newline-separated with a trailing newline: no field can be extended
        // to absorb the next one, because none of them may contain a newline
        // (`validate` enforces that).
        format!(
            "{PAYLOAD_VERSION}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            self.bundle, self.url, self.sha256, self.min_host, self.serial, self.platform,
        )
    }

    /// Structural checks that must pass before the signature means anything.
    fn validate(&self) -> Result<()> {
        let fields = [
            ("bundle", &self.bundle),
            ("url", &self.url),
            ("sha256", &self.sha256),
            ("platform", &self.platform),
        ];
        for (name, value) in fields {
            if value.trim().is_empty() {
                return Err(Error::Malformed(format!("the manifest has no {name}")));
            }
            // Without this the payload above is ambiguous: a `bundle` ending in
            // "\nhttps://evil" would sign the same bytes as a different
            // manifest. Cheap to check, fatal to omit.
            if value.contains('\n') || value.contains('\r') {
                return Err(Error::Malformed(format!(
                    "the manifest's {name} contains a line break"
                )));
            }
        }
        if !is_hex(&self.sha256, 64) {
            return Err(Error::Malformed(
                "the manifest's sha256 is not 64 hex characters".into(),
            ));
        }
        if !is_hex(&self.signature, 128) {
            return Err(Error::Malformed(
                "the manifest's signature is not 128 hex characters".into(),
            ));
        }
        // An archive fetched over anything else is one a network can rewrite
        // between the digest being signed and the bytes arriving. The
        // signature would still catch it — this refuses earlier and louder.
        if !self.url.starts_with("https://") {
            return Err(Error::Malformed("the manifest's url is not https".into()));
        }
        Ok(())
    }
}

/// What the host already has, and what it can run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    /// The id of the bundle being served, or `None` when that is the copy
    /// inside the app.
    pub bundle: Option<String>,
    /// The serial of the bundle being served; `0` for the shipped copy, which
    /// every real release outranks.
    pub serial: u64,
    /// This host's `BRIDGE_HOST_REVISION`.
    #[serde(rename = "hostRevision")]
    pub host_revision: u32,
    /// `android` or `ios`.
    pub platform: String,
}

/// The answer. Every variant carries what a log line needs, because a host
/// that silently declines to update is indistinguishable from one that cannot
/// reach the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "camelCase")]
pub enum Verdict {
    /// Download it.
    Install,
    /// Already serving exactly this. The common case, on every launch.
    UpToDate,
    /// Needs a host newer than this one. Not an error: it is the safety rail
    /// working, and the store update that fixes it is on its way.
    TooNew { required: u32, host: u32 },
    /// Older than what is already installed. See the note on this variant.
    Downgrade { offered: u64, installed: u64 },
    /// Built for the other platform.
    WrongPlatform { offered: String, host: String },
}

impl Verdict {
    /// Whether the host should go and fetch the archive.
    pub fn should_install(&self) -> bool {
        matches!(self, Verdict::Install)
    }

    /// One line, meant for the host's log.
    pub fn reason(&self) -> String {
        match self {
            Verdict::Install => "the offered bundle is newer and this host can run it".into(),
            Verdict::UpToDate => "already serving the offered bundle".into(),
            Verdict::TooNew { required, host } => {
                format!("the offered bundle needs host revision {required} and this host is {host}")
            }
            Verdict::Downgrade { offered, installed } => {
                format!(
                    "the offered bundle is serial {offered}, older than the installed {installed}"
                )
            }
            Verdict::WrongPlatform { offered, host } => {
                format!("the offered bundle is for {offered} and this host is {host}")
            }
        }
    }
}

/// Parse a manifest **and** verify its signature, in that order, as one step.
///
/// Deliberately not two public functions. A `parse` that returned a usable
/// `Manifest` would make "forgot to verify" a one-line mistake with no visible
/// symptom — everything would work, against any manifest anyone served. The
/// only way to obtain a `Manifest` from untrusted bytes is to have verified it.
///
/// `public_key` is 64 hex characters, compiled into the host.
pub fn verify(json: &str, public_key: &str) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_str(json)
        .map_err(|e| Error::Malformed(format!("the manifest did not parse: {e}")))?;
    manifest.validate()?;

    let key_bytes: [u8; 32] = decode_hex(public_key, 32)
        .ok_or_else(|| Error::Malformed("the public key is not 64 hex characters".into()))?
        .try_into()
        .expect("decode_hex returned the length it was asked for");
    let signature_bytes: [u8; 64] = decode_hex(&manifest.signature, 64)
        .ok_or_else(|| Error::Malformed("the signature is not 128 hex characters".into()))?
        .try_into()
        .expect("decode_hex returned the length it was asked for");

    let key = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| Error::Malformed(format!("the public key is not a valid one: {e}")))?;
    let signature = ed25519_dalek::Signature::from_bytes(&signature_bytes);

    // `verify_strict`, not `verify`. The permissive variant accepts
    // small-order public keys and non-canonical encodings, which admit
    // signatures that verify under more than one key — exactly the ambiguity
    // this signature exists to remove.
    key.verify_strict(manifest.signing_payload().as_bytes(), &signature)
        .map_err(|_| Error::Malformed("the manifest's signature does not match".into()))?;

    Ok(manifest)
}

/// Whether a verified manifest should be installed over what is already there.
///
/// Separate from [`verify`] because it answers a different question and the
/// host needs both answers for its log: "this manifest is authentic" and
/// "this manifest is not for us".
pub fn judge(manifest: &Manifest, installed: &Installed) -> Verdict {
    if manifest.platform != installed.platform {
        return Verdict::WrongPlatform {
            offered: manifest.platform.clone(),
            host: installed.platform.clone(),
        };
    }
    if manifest.min_host > installed.host_revision {
        return Verdict::TooNew {
            required: manifest.min_host,
            host: installed.host_revision,
        };
    }
    if installed.bundle.as_deref() == Some(manifest.bundle.as_str()) {
        return Verdict::UpToDate;
    }
    // Strictly greater. Equal serials with different ids means two releases
    // were cut with the same number, and taking either one is a coin flip we
    // should not perform silently.
    //
    // Note this makes deliberate rollback a *forward* move: re-publishing an
    // older bundle means issuing it a new, higher serial. That is the intended
    // shape — otherwise anyone who can replay an old manifest can roll every
    // client back to a version whose bugs they know.
    if manifest.serial <= installed.serial {
        return Verdict::Downgrade {
            offered: manifest.serial,
            installed: installed.serial,
        };
    }
    Verdict::Install
}

/// Whether a digest the host computed is the one that was signed.
///
/// Case-insensitive, and compared in constant time with respect to content —
/// not because a timing attack on a public digest is realistic, but because
/// the alternative is three hosts each writing `==` on a string and one of
/// them one day writing `startsWith`.
pub fn digest_matches(expected: &str, actual: &str) -> bool {
    let (expected, actual) = (expected.trim(), actual.trim());
    if expected.len() != actual.len() || !is_hex(expected, expected.len()) {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.bytes().zip(actual.bytes()) {
        diff |= a.to_ascii_lowercase() ^ b.to_ascii_lowercase();
    }
    diff == 0
}

fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn decode_hex(text: &str, bytes: usize) -> Option<Vec<u8>> {
    if !is_hex(text, bytes * 2) {
        return None;
    }
    (0..bytes)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest produced by the *other* implementation of this format —
    /// `scripts/sign-manifest.js`, which is what the publish workflow runs.
    ///
    /// Everything else in this file signs with the same code it verifies with,
    /// so all of it would keep passing if `signing_payload` changed shape.
    /// This vector is the only test that can fail when the signer and the
    /// verifier drift apart, which is the failure that matters: it would ship
    /// bundles no device accepts, or — far worse, if someone then relaxed the
    /// check to make it go away — bundles every device accepts.
    ///
    /// Regenerate deliberately, never to make a red test green:
    /// ```text
    /// HOMERUN_BUNDLE_KEY=1c23…aff5 node scripts/sign-manifest.js sign \
    ///   --archive <file> --bundle 2026-08-14.1 --serial 3 \
    ///   --url https://cdn.gethomerun.app/ui/2026-08-14.1.zip
    /// ```
    ///
    /// The key below is a throwaway generated for this test and published in
    /// the repository. It signs nothing real.
    const JS_SIGNED: &str = r#"{
      "bundle": "2026-08-14.1",
      "url": "https://cdn.gethomerun.app/ui/2026-08-14.1.zip",
      "sha256": "d2045f55566b0d63ab5ac9216c8b068117a18043f0ba6453f7098dcbf8a4b038",
      "minHost": 1,
      "serial": 3,
      "platform": "android",
      "signature": "18b8a9dcd15af0a141d87eaf72b130e7698df55ec25744245f690bcaf0d4082fa0d373ed30f12c3536972f7d0d820e841671958fc1d042a206cd872670755506"
    }"#;

    /// The public half of the throwaway key `JS_SIGNED` was signed with.
    const JS_PUBLIC_KEY: &str = "f94519c8187b4ea306e539eb27010b6074e1a12bcc8b7fe654a27978abaefd21";

    #[test]
    fn verifies_a_manifest_the_javascript_signer_produced() {
        let manifest = verify(JS_SIGNED, JS_PUBLIC_KEY)
            .expect("scripts/sign-manifest.js and this module have drifted apart");
        assert_eq!(manifest.bundle, "2026-08-14.1");
        assert_eq!(manifest.serial, 3);
        assert!(digest_matches(
            &manifest.sha256,
            "D2045F55566B0D63AB5AC9216C8B068117A18043F0BA6453F7098DCBF8A4B038",
        ));
    }

    /// A throwaway key pair, generated once and pinned here so the tests are
    /// deterministic and do not need an RNG feature enabled.
    ///
    /// Produced with `ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])`.
    const SECRET: [u8; 32] = [7u8; 32];

    fn signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&SECRET)
    }

    fn public_key_hex() -> String {
        hex(signing_key().verifying_key().as_bytes())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Builds a manifest and signs it, so tests state what they are varying
    /// rather than carrying 300 characters of hex.
    fn signed(mutate: impl FnOnce(&mut Manifest)) -> String {
        use ed25519_dalek::Signer;
        let mut manifest = Manifest {
            bundle: "2026-08-14.1".into(),
            url: "https://cdn.gethomerun.app/ui/2026-08-14.1.zip".into(),
            sha256: "a".repeat(64),
            min_host: 1,
            serial: 5,
            platform: "android".into(),
            signature: String::new(),
        };
        mutate(&mut manifest);
        let signature = signing_key().sign(manifest.signing_payload().as_bytes());
        manifest.signature = hex(&signature.to_bytes());
        serde_json::to_string(&manifest).unwrap()
    }

    fn installed() -> Installed {
        Installed {
            bundle: Some("2026-08-01.1".into()),
            serial: 4,
            host_revision: 1,
            platform: "android".into(),
        }
    }

    #[test]
    fn accepts_a_manifest_it_signed_itself() {
        let manifest = verify(&signed(|_| {}), &public_key_hex()).unwrap();
        assert_eq!(manifest.bundle, "2026-08-14.1");
        assert_eq!(judge(&manifest, &installed()), Verdict::Install);
    }

    #[test]
    fn rejects_a_manifest_signed_by_another_key() {
        let other = hex(ed25519_dalek::SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .as_bytes());
        assert!(verify(&signed(|_| {}), &other).is_err());
    }

    /// The whole point of signing the payload rather than trusting TLS: every
    /// field an attacker would want to move has to break the signature.
    #[test]
    fn rejects_every_field_being_tampered_with() {
        let original = signed(|_| {});
        let key = public_key_hex();

        for (field, replacement) in [
            (
                "\"url\":\"https://cdn.gethomerun.app/ui/2026-08-14.1.zip\"",
                "\"url\":\"https://evil.example/ui.zip\"",
            ),
            ("\"minHost\":1", "\"minHost\":0"),
            ("\"serial\":5", "\"serial\":9"),
            ("\"platform\":\"android\"", "\"platform\":\"ios\""),
            ("\"bundle\":\"2026-08-14.1\"", "\"bundle\":\"2026-08-14.2\""),
        ] {
            let tampered = original.replace(field, replacement);
            assert_ne!(tampered, original, "test did not actually change {field}");
            assert!(
                verify(&tampered, &key).is_err(),
                "tampering with {field} was accepted"
            );
        }

        // The digest too — it is what binds the manifest to the bytes.
        let tampered = original.replace(&"a".repeat(64), &"b".repeat(64));
        assert!(
            verify(&tampered, &key).is_err(),
            "a swapped digest was accepted"
        );
    }

    /// A field containing a newline could otherwise absorb the next line of
    /// the payload and two different manifests would sign identical bytes.
    #[test]
    fn refuses_fields_containing_line_breaks() {
        let json = signed(|m| m.bundle = "2026-08-14.1\nhttps://evil.example/ui.zip".into());
        let error = verify(&json, &public_key_hex()).unwrap_err();
        assert!(format!("{error}").contains("line break"), "{error}");
    }

    #[test]
    fn refuses_a_url_that_is_not_https() {
        let json = signed(|m| m.url = "http://cdn.gethomerun.app/ui/x.zip".into());
        assert!(verify(&json, &public_key_hex()).is_err());
    }

    #[test]
    fn refuses_a_malformed_signature_without_panicking() {
        let json = signed(|_| {}).replace("\"signature\"", "\"sig\"");
        assert!(verify(&json, &public_key_hex()).is_err());
    }

    #[test]
    fn declines_a_bundle_this_host_is_too_old_for() {
        let manifest = verify(&signed(|m| m.min_host = 7), &public_key_hex()).unwrap();
        assert_eq!(
            judge(&manifest, &installed()),
            Verdict::TooNew {
                required: 7,
                host: 1
            }
        );
    }

    #[test]
    fn declines_the_bundle_it_is_already_serving() {
        let manifest = verify(&signed(|_| {}), &public_key_hex()).unwrap();
        let mut installed = installed();
        installed.bundle = Some("2026-08-14.1".into());
        assert_eq!(judge(&manifest, &installed), Verdict::UpToDate);
    }

    /// Replaying an old manifest is how someone puts a version whose bugs they
    /// know back on every device.
    #[test]
    fn declines_an_older_or_equal_serial() {
        let key = public_key_hex();
        for serial in [1, 4] {
            let manifest = verify(&signed(|m| m.serial = serial), &key).unwrap();
            assert!(
                matches!(judge(&manifest, &installed()), Verdict::Downgrade { .. }),
                "serial {serial} was accepted over the installed 4"
            );
        }
    }

    /// The shipped bundle is serial 0, so the first real release outranks it.
    #[test]
    fn installs_over_the_shipped_bundle() {
        let manifest = verify(&signed(|_| {}), &public_key_hex()).unwrap();
        let installed = Installed {
            bundle: None,
            serial: 0,
            host_revision: 1,
            platform: "android".into(),
        };
        assert_eq!(judge(&manifest, &installed), Verdict::Install);
    }

    #[test]
    fn declines_the_other_platforms_bundle() {
        let manifest = verify(&signed(|m| m.platform = "ios".into()), &public_key_hex()).unwrap();
        assert!(matches!(
            judge(&manifest, &installed()),
            Verdict::WrongPlatform { .. }
        ));
    }

    #[test]
    fn compares_digests_case_insensitively_and_exactly() {
        assert!(digest_matches(&"a".repeat(64), &"A".repeat(64)));
        assert!(!digest_matches(&"a".repeat(64), &"a".repeat(63)));
        // A prefix must not pass — the failure mode of a hand-written check.
        assert!(!digest_matches(
            &"a".repeat(64),
            &format!("{}b", "a".repeat(63))
        ));
        assert!(!digest_matches("nothex", "nothex"));
    }

    /// Signing bytes that are not the manifest's own is the mistake that
    /// makes every other test here pass while verifying nothing.
    #[test]
    fn the_payload_covers_the_fields_it_claims_to() {
        let manifest = verify(&signed(|_| {}), &public_key_hex()).unwrap();
        let payload = manifest.signing_payload();
        for expected in [
            PAYLOAD_VERSION,
            &manifest.bundle,
            &manifest.url,
            &manifest.sha256,
            &manifest.min_host.to_string(),
            &manifest.serial.to_string(),
            &manifest.platform,
        ] {
            assert!(payload.contains(expected), "payload is missing {expected}");
        }
        assert!(
            !payload.contains(&manifest.signature),
            "the payload covers the signature, which cannot be"
        );
    }
}

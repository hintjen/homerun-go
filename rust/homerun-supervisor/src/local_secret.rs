//! Sealing a secret to this computer's current user.
//!
//! What a game's extension keeps between runs -- a refresh token, most
//! likely, which is a standing key to the person's account -- is sealed with
//! Windows DPAPI before it touches the disk. A sealed file copied to another
//! machine, or read by another Windows account, opens to nothing.
//!
//! `label` is mixed in as DPAPI's optional entropy: each extension seals with
//! its own name, so one extension's file moved into another's folder does not
//! open either.
//!
//! **Elsewhere, the key is the host's.** Linux has no DPAPI, and a runner
//! cannot count on reaching a keystore itself. Homerun Desktop can: Electron's
//! `safeStorage` is the system keyring. So the desktop keeps a random 256-bit
//! key there and hands it to the runner in `hello`, and the runner seals with
//! AES-256-GCM under it ([`Scheme::HostKey`]): envelope encryption, with the
//! envelope's key in the keystore and the data on disk. For development,
//! `HOMERUN_SEAL_KEY_FILE` names an owner-only file holding the key in hex
//! ([`load_key_file`]).
//!
//! **Without a key, an owner-only file, and it says so.** A runner started by
//! something that has none, or a desktop whose keyring is only `basic_text`
//! (no GNOME Keyring or KWallet), keeps the value as it is, under
//! [`Scheme::OwnerOnly`], in a file only this user can read. That protects it
//! from the machine's other users and from nothing else, which is also what a
//! vendor's own tool does in the same place: Hytale's downloader keeps a token
//! for the same account in an owner-only JSON file. It is not dressed up as
//! encryption. Refusing instead barred everyone without a keyring from the
//! game while that token sat on disk anyway. A caller says so to the person
//! ([`keeps_in_the_clear`]), and a key that arrives later takes over on the
//! next save.
//!
//! # The first byte says how the rest was sealed
//!
//! Every sealed value starts with one [`Scheme`] byte. A backend that changes
//! later -- a new platform, or Windows moving to envelope encryption -- can
//! then tell what it is holding: open an older scheme and write the current
//! one on the next save, rather than read an old file as damage and make the
//! person sign in again. [`open`] refuses a scheme this build does not know,
//! in words that say a newer Homerun wrote it.

/// How a sealed value was sealed: its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Scheme {
    /// Windows DPAPI, current user, the label as entropy.
    Dpapi = 1,
    /// AES-256-GCM under the key the host gave ([`set_host_key`]): a 12-byte
    /// nonce, then the ciphertext and its tag, with the label as the
    /// associated data.
    HostKey = 2,
    /// **Not encrypted.** The label, a NUL, then the value as it is, for a
    /// file only its owner can read (the caller writes it so). Off Windows,
    /// when the host gave no key.
    OwnerOnly = 3,
}

impl Scheme {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Dpapi),
            2 => Some(Self::HostKey),
            3 => Some(Self::OwnerOnly),
            _ => None,
        }
    }
}

/// The scheme [`seal`] uses now: DPAPI on Windows; elsewhere the host's key
/// if one was given, and an owner-only file if not.
pub fn current() -> Option<Scheme> {
    imp::CURRENT
        .or_else(|| host_key().map(|_| Scheme::HostKey))
        .or(imp::FALLBACK)
}

/// Whether what is sealed now is kept unencrypted, in an owner-only file:
/// the case a person should be told about, with what would change it.
pub fn keeps_in_the_clear() -> bool {
    current() == Some(Scheme::OwnerOnly)
}

/// Whether this build seals with a host's key when given one. True for every
/// build: what decides whether it *can* is whether the host has a key, which
/// `--features` cannot know.
pub const HOST_KEY_SUPPORTED: bool = true;

static HOST_KEY: std::sync::Mutex<Option<[u8; 32]>> = std::sync::Mutex::new(None);

fn host_key() -> Option<[u8; 32]> {
    *HOST_KEY.lock().unwrap_or_else(|e| e.into_inner())
}

/// The key the host keeps for this user, as 64 hex digits.
pub fn set_host_key(hex: &str) -> Result<(), String> {
    let key = parse_key(hex)?;
    *HOST_KEY.lock().unwrap_or_else(|e| e.into_inner()) = Some(key);
    Ok(())
}

fn parse_key(hex: &str) -> Result<[u8; 32], String> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("The key for keeping sign-ins safe is not 64 hex digits.".into());
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| "bad hex")?;
    }
    Ok(key)
}

/// `HOMERUN_SEAL_KEY_FILE`, for a runner started from a terminal: the key in
/// hex, in a file only its owner can read. Refused when anyone else can.
pub fn load_key_file(path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta =
            std::fs::metadata(path).map_err(|_| format!("{} is missing.", path.display()))?;
        if meta.mode() & 0o077 != 0 {
            return Err(format!(
                "{} can be read by others; make it owner-only (chmod 600).",
                path.display()
            ));
        }
    }
    let hex =
        std::fs::read_to_string(path).map_err(|_| format!("{} cannot be read.", path.display()))?;
    set_host_key(&hex)
}

/// How `sealed` was sealed, if this build knows the scheme.
///
/// A caller that finds something other than [`current`] can open it and seal
/// it again, which is how a change of backend migrates without a new sign-in.
pub fn scheme_of(sealed: &[u8]) -> Option<Scheme> {
    sealed.first().copied().and_then(Scheme::from_byte)
}

/// Seal `plain` for this user on this computer.
pub fn seal(plain: &[u8], label: &str) -> Result<Vec<u8>, String> {
    let scheme = current().ok_or_else(|| imp::REFUSAL.to_string())?;
    let body = match scheme {
        Scheme::Dpapi => imp::seal(plain, label)?,
        Scheme::HostKey => host::seal(&host_key().expect("current() said so"), plain, label)?,
        Scheme::OwnerOnly => [label.as_bytes(), &[0], plain].concat(),
    };
    let mut sealed = Vec::with_capacity(body.len() + 1);
    sealed.push(scheme as u8);
    sealed.extend_from_slice(&body);
    Ok(sealed)
}

/// Open what [`seal`] produced, on the same computer, as the same user.
pub fn open(sealed: &[u8], label: &str) -> Result<Vec<u8>, String> {
    let Some((&first, body)) = sealed.split_first() else {
        return Err("There is nothing here to open.".into());
    };
    match Scheme::from_byte(first) {
        Some(Scheme::Dpapi) => imp::open_dpapi(body, label),
        Some(Scheme::HostKey) => match host_key() {
            Some(key) => host::open(&key, body, label),
            None => Err(
                "Homerun Desktop keeps the key to what was kept here, and it has not given it."
                    .into(),
            ),
        },
        // The label still has to match: one extension's file moved into
        // another's folder is not read as that one's.
        Some(Scheme::OwnerOnly) => match body.strip_prefix(label.as_bytes()) {
            Some([0, plain @ ..]) => Ok(plain.to_vec()),
            _ => Err("This was kept for another part of Homerun.".into()),
        },
        None => Err("This was kept by a newer version of Homerun, which can read it.".into()),
    }
}

#[cfg(windows)]
mod imp {
    use super::Scheme;

    pub const CURRENT: Option<Scheme> = Some(Scheme::Dpapi);
    pub const FALLBACK: Option<Scheme> = None;
    // Only reached when `CURRENT` is `None`, which it is not here.
    #[allow(dead_code)]
    pub const REFUSAL: &str = "This computer cannot keep a sign-in safe yet.";

    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            // DPAPI reads the input and never writes it.
            pbData: bytes.as_ptr() as *mut u8,
        }
    }

    /// Copy DPAPI's output and give its buffer back.
    ///
    /// # Safety
    /// `out` must be a blob DPAPI filled, not yet freed.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        bytes
    }

    pub fn seal(plain: &[u8], label: &str) -> Result<Vec<u8>, String> {
        if u32::try_from(plain.len()).is_err() {
            return Err("That is too much to keep safe.".into());
        }
        let input = blob(plain);
        let entropy = blob(label.as_bytes());
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: every pointer is valid for the call; `out` is freed by `take`.
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err("This computer could not keep that safe.".into());
        }
        // SAFETY: DPAPI succeeded, so `out` is its buffer.
        Ok(unsafe { take(out) })
    }

    pub fn open_dpapi(sealed: &[u8], label: &str) -> Result<Vec<u8>, String> {
        let input = blob(sealed);
        let entropy = blob(label.as_bytes());
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: as in `seal`.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err("What was kept here cannot be read on this computer.".into());
        }
        // SAFETY: as in `seal`.
        Ok(unsafe { take(out) })
    }
}

/// The host-key scheme. The nonce is random per seal: one key seals few
/// values (one sign-in per extension), far inside AES-GCM's random-nonce bound.
mod host {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
    use ring::rand::{SecureRandom, SystemRandom};

    fn key(bytes: &[u8; 32]) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, bytes).expect("a 32-byte key"))
    }

    pub fn seal(bytes: &[u8; 32], plain: &[u8], label: &str) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "This computer could not keep that safe.".to_string())?;
        let mut body = plain.to_vec();
        key(bytes)
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(label.as_bytes()),
                &mut body,
            )
            .map_err(|_| "This computer could not keep that safe.".to_string())?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn open(bytes: &[u8; 32], sealed: &[u8], label: &str) -> Result<Vec<u8>, String> {
        if sealed.len() < NONCE_LEN {
            return Err("What was kept here cannot be read on this computer.".into());
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce)
            .map_err(|_| "What was kept here cannot be read on this computer.".to_string())?;
        let mut body = body.to_vec();
        let plain = key(bytes)
            .open_in_place(nonce, Aad::from(label.as_bytes()), &mut body)
            .map_err(|_| "What was kept here cannot be read on this computer.".to_string())?;
        Ok(plain.to_vec())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Scheme;

    pub const CURRENT: Option<Scheme> = None;
    pub const FALLBACK: Option<Scheme> = Some(Scheme::OwnerOnly);
    // Only reached when nothing above is current, which `FALLBACK` prevents.
    #[allow(dead_code)]
    pub const REFUSAL: &str = "This computer cannot keep a sign-in yet.";

    pub fn seal(_plain: &[u8], _label: &str) -> Result<Vec<u8>, String> {
        Err(REFUSAL.into())
    }

    /// DPAPI is Windows-only: a value it sealed opens nowhere else.
    pub fn open_dpapi(_sealed: &[u8], _label: &str) -> Result<Vec<u8>, String> {
        Err("This was kept by Homerun on Windows, and can only be read there.".into())
    }
}

/// The header, which needs no DPAPI: these run everywhere.
#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn a_scheme_this_build_does_not_know_is_refused_as_newer() {
        let err = open(&[0xEE, 1, 2, 3], "fixture").unwrap_err();
        assert!(err.contains("newer version of Homerun"), "{err}");
        assert_eq!(scheme_of(&[0xEE, 1]), None);
    }

    #[test]
    fn nothing_is_not_a_sealed_value() {
        assert!(open(&[], "fixture").is_err());
        assert_eq!(scheme_of(&[]), None);
    }

    #[test]
    fn the_scheme_byte_is_the_documented_one() {
        assert_eq!(Scheme::Dpapi as u8, 1);
        assert_eq!(scheme_of(&[1, 0xAA]), Some(Scheme::Dpapi));
        assert_eq!(Scheme::HostKey as u8, 2);
        assert_eq!(scheme_of(&[2, 0xAA]), Some(Scheme::HostKey));
        assert_eq!(Scheme::OwnerOnly as u8, 3);
        assert_eq!(scheme_of(&[3, 0xAA]), Some(Scheme::OwnerOnly));
    }

    #[test]
    fn an_owner_only_value_opens_with_its_own_label_only() {
        let kept = [&[Scheme::OwnerOnly as u8][..], b"hytale\0token"].concat();
        assert_eq!(open(&kept, "hytale").unwrap(), b"token");
        assert!(open(&kept, "palworld").is_err(), "another label");
        assert!(open(&kept, "hytal").is_err(), "a prefix of the label");
    }

    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn a_host_key_seal_opens_with_its_key_and_label_and_nothing_else() {
        let secret = b"refresh-token-4f2a9c";
        let sealed = host::seal(&KEY, secret, "hytale").unwrap();
        assert!(!sealed.windows(secret.len()).any(|w| w == secret));
        assert_eq!(host::open(&KEY, &sealed, "hytale").unwrap(), secret);
        assert!(
            host::open(&KEY, &sealed, "palworld").is_err(),
            "another label"
        );
        assert!(
            host::open(&[8; 32], &sealed, "hytale").is_err(),
            "another key"
        );
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(
            host::open(&KEY, &tampered, "hytale").is_err(),
            "a changed byte"
        );
        // A fresh nonce each time: the same secret never seals the same.
        assert_ne!(sealed, host::seal(&KEY, secret, "hytale").unwrap());
    }

    #[test]
    fn a_key_is_64_hex_digits() {
        assert!(parse_key(&"ab".repeat(32)).is_ok());
        assert!(parse_key(&"ab".repeat(31)).is_err());
        assert!(parse_key(&"zz".repeat(32)).is_err());
        assert_eq!(
            parse_key(&format!("{}\n", "0f".repeat(32))).unwrap(),
            [0x0f; 32]
        );
    }

    /// The whole path, as an extension sees it: the host gives a key, then
    /// `seal` and `open` use it. Off Windows only, where DPAPI is not first.
    #[test]
    #[cfg(not(windows))]
    fn given_a_host_key_this_platform_seals_and_opens() {
        // Before any key: the owner-only file, said to be in the clear. (The
        // key is process-wide, so this test is the one that sets it.)
        let before = seal(b"token", "fixture").unwrap();
        if host_key().is_none() {
            assert!(keeps_in_the_clear());
            assert_eq!(scheme_of(&before), Some(Scheme::OwnerOnly));
        }
        set_host_key(&"5a".repeat(32)).unwrap();
        assert!(!keeps_in_the_clear());
        // What was kept before the key still opens, so nobody signs in again.
        assert_eq!(open(&before, "fixture").unwrap(), b"token");
        assert_eq!(current(), Some(Scheme::HostKey));
        let sealed = seal(b"token", "fixture").unwrap();
        assert_eq!(scheme_of(&sealed), Some(Scheme::HostKey));
        assert_eq!(open(&sealed, "fixture").unwrap(), b"token");
    }

    #[test]
    #[cfg(unix)]
    fn a_key_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("seal-key-{}", std::process::id()));
        std::fs::write(&path, "5a".repeat(32)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_key_file(&path).unwrap_err().contains("chmod 600"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_key_file(&path).is_ok());
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_value_starts_with_the_scheme_that_sealed_it() {
        let sealed = seal(b"x", "fixture").unwrap();
        assert_eq!(current(), Some(Scheme::Dpapi));
        assert_eq!(scheme_of(&sealed), current());
        assert_eq!(sealed[0], Scheme::Dpapi as u8);
    }

    /// A DPAPI blob with no header -- what this module wrote before it had
    /// one -- is not mistaken for a current value.
    #[test]
    fn a_headerless_blob_does_not_open() {
        let raw = imp::seal(b"x", "fixture").unwrap();
        assert!(open(&raw, "fixture").is_err());
    }

    #[test]
    fn a_sealed_secret_opens_with_its_label_and_is_not_on_disk_in_the_clear() {
        let secret = b"refresh-token-4f2a9c";
        let sealed = seal(secret, "fixture").unwrap();
        assert!(!sealed.windows(secret.len()).any(|w| w == secret));
        assert_eq!(open(&sealed, "fixture").unwrap(), secret);
    }

    #[test]
    fn another_label_cannot_open_it() {
        let sealed = seal(b"x", "fixture").unwrap();
        assert!(open(&sealed, "hytale").is_err());
    }

    #[test]
    fn a_damaged_seal_is_an_error_not_garbage() {
        let mut sealed = seal(b"x", "fixture").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(open(&sealed, "fixture").is_err());
        assert!(open(b"not sealed at all", "fixture").is_err());
    }

    #[test]
    fn empty_is_sealed_like_anything_else() {
        let sealed = seal(b"", "fixture").unwrap();
        assert_eq!(open(&sealed, "fixture").unwrap(), b"");
    }
}

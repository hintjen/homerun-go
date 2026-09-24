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
//! **Elsewhere there is no sealing yet, and so no storing.** Both calls
//! refuse on a platform without DPAPI rather than fall back to a plain file.
//! The runner is Windows-first; the day it hosts extensions on another
//! platform, that platform's keystore goes here -- most likely as envelope
//! encryption: a random key held in the keystore, the data encrypted with it
//! on disk, because keystores hold small items and DPAPI holds nothing.
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
}

impl Scheme {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Dpapi),
            _ => None,
        }
    }
}

/// The scheme [`seal`] uses on this platform, if it can seal at all.
pub fn current() -> Option<Scheme> {
    imp::CURRENT
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
    let body = imp::seal(plain, label)?;
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
        None => Err("This was kept by a newer version of Homerun, which can read it.".into()),
    }
}

#[cfg(windows)]
mod imp {
    use super::Scheme;

    pub const CURRENT: Option<Scheme> = Some(Scheme::Dpapi);
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

#[cfg(not(windows))]
mod imp {
    use super::Scheme;

    pub const CURRENT: Option<Scheme> = None;
    pub const REFUSAL: &str = "This computer cannot keep a sign-in safe yet.";

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

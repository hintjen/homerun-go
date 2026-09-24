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
//! platform, that platform's keychain goes here.

/// Seal `plain` for this user on this computer.
pub fn seal(plain: &[u8], label: &str) -> Result<Vec<u8>, String> {
    imp::seal(plain, label)
}

/// Open what [`seal`] produced, on the same computer, as the same user.
pub fn open(sealed: &[u8], label: &str) -> Result<Vec<u8>, String> {
    imp::open(sealed, label)
}

#[cfg(windows)]
mod imp {
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

    pub fn open(sealed: &[u8], label: &str) -> Result<Vec<u8>, String> {
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
    const REFUSAL: &str = "This computer cannot keep a sign-in safe yet.";

    pub fn seal(_plain: &[u8], _label: &str) -> Result<Vec<u8>, String> {
        Err(REFUSAL.into())
    }

    pub fn open(_sealed: &[u8], _label: &str) -> Result<Vec<u8>, String> {
        Err(REFUSAL.into())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

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

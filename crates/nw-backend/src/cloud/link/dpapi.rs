//! The node's private key at rest on Windows: encrypted with DPAPI for the
//! current user (`CryptProtectData`), so only that Windows account on this
//! computer can read it. Unix keeps the key in a private (0600) file instead.

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// Binds the encrypted key to its purpose: another program's DPAPI data for
/// the same user cannot be passed off as the node key, nor the reverse.
const ENTROPY: &[u8] = b"NeuroWatch cloud node key v1";

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        // DPAPI only reads its input blobs.
        pbData: data.as_ptr() as *mut u8,
    }
}

/// Copies DPAPI's output and frees it.
fn take(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if output.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: on success DPAPI returns `cbData` bytes at `pbData`, allocated
    // with LocalAlloc; they are copied before the single LocalFree.
    unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData.cast());
        bytes
    }
}

fn empty() -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    }
}

/// Encrypts `secret` for the current Windows user.
pub fn protect(secret: &[u8]) -> Result<Vec<u8>, String> {
    let (input, entropy) = (blob(secret), blob(ENTROPY));
    let mut output = empty();
    // SAFETY: the input blobs point at live slices; nothing else is passed.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(format!(
            "Windows could not protect the key: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(take(output))
}

/// Decrypts data from [`protect`]; fails for another user's or a tampered file.
pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, String> {
    let (input, entropy) = (blob(sealed), blob(ENTROPY));
    let mut output = empty();
    // SAFETY: as in `protect`; no description is requested.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(format!(
            "the key could not be decrypted for this Windows user: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(take(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_protected_secret_comes_back_and_is_not_stored_in_the_clear() {
        let secret = [7u8; 32];
        let sealed = protect(&secret).unwrap();
        assert!(!sealed.windows(secret.len()).any(|w| w == secret));
        assert_eq!(unprotect(&sealed).unwrap(), secret);
    }

    #[test]
    fn tampered_or_foreign_data_is_refused() {
        let mut sealed = protect(&[1u8; 32]).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(unprotect(&sealed).is_err());
        assert!(unprotect(b"not dpapi data").is_err());
    }
}

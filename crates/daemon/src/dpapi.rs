//! Windows DPAPI: encrypts secrets so that only this Windows account on this computer can decrypt them.
//! Another account, a copied disk image or a stolen backup of the config folder gets unreadable bytes.
use std::io;
use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

/// Mixed in so another program calling DPAPI with its own defaults cannot trivially open our blobs.
const ENTROPY: &[u8] = b"SmoothKvm identity key v1";

fn blob(d: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: d.len() as u32, pbData: d.as_ptr() as *mut u8 }
}

fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as *mut _));
        v
    }
}

pub fn protect(data: &[u8]) -> io::Result<Vec<u8>> {
    unsafe {
        let (input, entropy) = (blob(data), blob(ENTROPY));
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(&input, w!("SmoothKvm"), Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(take(out))
    }
}

pub fn unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
    unsafe {
        let (input, entropy) = (blob(data), blob(ENTROPY));
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(take(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_not_plaintext() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let enc = protect(secret).unwrap();
        assert!(!enc.windows(secret.len()).any(|w| w == secret), "secret is visible inside the encrypted blob");
        assert_eq!(unprotect(&enc).unwrap(), secret);
    }

    #[test]
    fn tampered_blob_is_rejected() {
        let mut enc = protect(b"secret").unwrap();
        let n = enc.len();
        enc[n / 2] ^= 0xFF;
        assert!(unprotect(&enc).is_err());
    }
}

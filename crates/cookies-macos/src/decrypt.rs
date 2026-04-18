//! Chromium cookie decryption primitive.
//!
//! macOS Brave/Chromium uses:
//! - Prefix: `v10` (3-byte ASCII) — followed by AES-128-CBC ciphertext.
//! - Key derivation: PBKDF2-HMAC-SHA1(password, salt=`saltysalt`, iter=1003, out_len=16).
//! - IV: 16 bytes of ASCII space (`b" " * 16`).
//! - Padding: PKCS#7.
//!
//! When no Keychain entry is available, Chromium uses the password `"peanuts"`. This matches
//! the `pycookiecheat` fallback behaviour.

use crate::{Error, Result};
use aes::Aes128;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::Hmac;
use pbkdf2::pbkdf2;
use sha1::Sha1;

type Aes128CbcDec = cbc::Decryptor<Aes128>;
type Aes128CbcEnc = cbc::Encryptor<Aes128>;

const SALT: &[u8] = b"saltysalt";
const ITERATIONS: u32 = 1003;
const KEY_LEN: usize = 16;
const IV: [u8; 16] = *b"                "; // 16 spaces

pub fn derive_key(password: &[u8]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2::<Hmac<Sha1>>(password, SALT, ITERATIONS, &mut key)
        .expect("pbkdf2 with fixed parameters cannot fail");
    key
}

pub fn decrypt(encrypted_value: &[u8], password: &[u8]) -> Result<Vec<u8>> {
    if encrypted_value.len() < 3 {
        return Err(Error::Decrypt("ciphertext too short".to_string()));
    }
    let prefix = std::str::from_utf8(&encrypted_value[..3]).unwrap_or("");
    if prefix != "v10" {
        return Err(Error::UnsupportedVersion(prefix.to_string()));
    }
    let body = &encrypted_value[3..];
    if body.len() % 16 != 0 {
        return Err(Error::Decrypt(format!(
            "ciphertext length {} is not a multiple of 16",
            body.len()
        )));
    }
    let key = derive_key(password);
    let mut buf = body.to_vec();
    let plaintext = Aes128CbcDec::new_from_slices(&key, &IV)
        .map_err(|e| Error::Decrypt(e.to_string()))?
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| Error::Decrypt(e.to_string()))?;
    Ok(plaintext.to_vec())
}

/// Encrypt `plaintext` with the same parameters Chromium uses. Intended for round-trip
/// vector tests only — production code never needs to re-encrypt cookies.
pub fn encrypt_for_test(plaintext: &[u8], password: &[u8]) -> Vec<u8> {
    let key = derive_key(password);
    let encryptor = Aes128CbcEnc::new_from_slices(&key, &IV).expect("key/iv length");
    let mut buf = plaintext.to_vec();
    // pad buf to next 16-byte boundary
    let pad_room = ((plaintext.len() / 16) + 1) * 16;
    buf.resize(pad_room, 0);
    let ct = encryptor
        .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
        .expect("pkcs7 pad");
    let mut out = Vec::with_capacity(3 + ct.len());
    out.extend_from_slice(b"v10");
    out.extend_from_slice(ct);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decrypt_vector_peanuts_hello_world() {
        // Chromium fallback password when no Keychain entry exists.
        let password = b"peanuts";
        let plaintext = b"hello world";
        let encrypted = encrypt_for_test(plaintext, password);
        let decrypted = decrypt(&encrypted, password).unwrap();
        assert_eq!(decrypted, plaintext.to_vec());
    }

    #[test]
    fn test_decrypt_vector_utf8_plaintext() {
        let password = b"brave safe storage test";
        let plaintext = "bb_session=abcdef123456; path=/; Russian Привет".as_bytes();
        let encrypted = encrypt_for_test(plaintext, password);
        let decrypted = decrypt(&encrypted, password).unwrap();
        assert_eq!(decrypted, plaintext.to_vec());
    }

    #[test]
    fn test_decrypt_rejects_non_v10_prefix() {
        let err = decrypt(b"v11\x00\x01\x02", b"peanuts").unwrap_err();
        matches!(err, Error::UnsupportedVersion(_));
    }

    #[test]
    fn test_decrypt_rejects_too_short() {
        let err = decrypt(b"v1", b"peanuts").unwrap_err();
        matches!(err, Error::Decrypt(_));
    }

    #[test]
    fn test_key_derivation_is_deterministic() {
        let k1 = derive_key(b"peanuts");
        let k2 = derive_key(b"peanuts");
        assert_eq!(k1, k2, "key derivation must be deterministic");
        let k3 = derive_key(b"other");
        assert_ne!(k1, k3, "different passwords must produce different keys");
    }

    #[test]
    fn test_decrypt_wrong_password_fails() {
        let encrypted = encrypt_for_test(b"secret cookie", b"right-password");
        let result = decrypt(&encrypted, b"wrong-password");
        assert!(
            result.is_err(),
            "decrypt with wrong password must fail (PKCS#7 padding validation rejects garbage)"
        );
    }
}

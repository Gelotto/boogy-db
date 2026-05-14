use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

use crate::error::{BoogyError, Result};
use crate::page::PAGE_SIZE;

/// Nonce size for AES-GCM (96 bits).
const NONCE_SIZE: usize = 12;
/// Authentication tag size for AES-GCM (128 bits).
const TAG_SIZE: usize = 16;
/// Maximum plaintext payload that fits in a single encrypted page.
pub const ENCRYPTED_PAYLOAD_SIZE: usize = PAGE_SIZE - NONCE_SIZE - TAG_SIZE;

/// AES-256-GCM cipher for page-level encryption.
///
/// Each encrypted page is laid out as `[nonce:12][ciphertext+tag]` and fits
/// exactly in `PAGE_SIZE` bytes.
///
/// Note: Key material in the `Aes256Gcm` struct is not zeroized on drop.
/// For applications requiring key zeroization, consider wrapping `Cipher`
/// in a custom type that manages key lifetime.
pub struct Cipher {
    inner: Aes256Gcm,
}

// Aes256Gcm is Send + Sync, so Cipher inherits that automatically.

impl Cipher {
    /// Create a cipher from a raw 256-bit key.
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            inner: Aes256Gcm::new_from_slice(key).expect("32-byte key is always valid"),
        }
    }

    /// Encrypt `plaintext` (must be exactly `ENCRYPTED_PAYLOAD_SIZE` bytes)
    /// into a full page: `[nonce:12][ciphertext+tag]`.
    pub fn encrypt_page(&self, plaintext: &[u8]) -> Result<[u8; PAGE_SIZE]> {
        if plaintext.len() != ENCRYPTED_PAYLOAD_SIZE {
            return Err(BoogyError::InvalidKey(format!(
                "plaintext must be {} bytes, got {}",
                ENCRYPTED_PAYLOAD_SIZE,
                plaintext.len()
            )));
        }

        let mut nonce_bytes = [0u8; NONCE_SIZE];
        rand::fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .inner
            .encrypt(nonce, plaintext)
            .map_err(|e| BoogyError::DecryptionFailed(format!("encryption failed: {e}")))?;

        let mut page = [0u8; PAGE_SIZE];
        page[..NONCE_SIZE].copy_from_slice(&nonce_bytes);
        page[NONCE_SIZE..NONCE_SIZE + ciphertext.len()].copy_from_slice(&ciphertext);
        Ok(page)
    }

    /// Decrypt a full encrypted page back to the original plaintext.
    pub fn decrypt_page(&self, encrypted: &[u8; PAGE_SIZE]) -> Result<[u8; ENCRYPTED_PAYLOAD_SIZE]> {
        let nonce = Nonce::from_slice(&encrypted[..NONCE_SIZE]);
        let ciphertext = &encrypted[NONCE_SIZE..];

        let plaintext = self
            .inner
            .decrypt(nonce, ciphertext)
            .map_err(|e| BoogyError::DecryptionFailed(format!("decryption failed: {e}")))?;

        let mut out = [0u8; ENCRYPTED_PAYLOAD_SIZE];
        out.copy_from_slice(&plaintext);
        Ok(out)
    }
}

impl Clone for Cipher {
    fn clone(&self) -> Self {
        // Aes256Gcm doesn't expose the key, but it implements Clone.
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cipher(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        rand::fill(&mut key);
        key
    }

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let key = test_key();
        let cipher = Cipher::new(&key);

        let mut plaintext = [0u8; ENCRYPTED_PAYLOAD_SIZE];
        rand::fill(&mut plaintext);

        let encrypted = cipher.encrypt_page(&plaintext).unwrap();
        let decrypted = cipher.decrypt_page(&encrypted).unwrap();

        assert_eq!(&plaintext[..], &decrypted[..]);
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = test_key();
        let key2 = test_key();
        let cipher1 = Cipher::new(&key1);
        let cipher2 = Cipher::new(&key2);

        let plaintext = [42u8; ENCRYPTED_PAYLOAD_SIZE];
        let encrypted = cipher1.encrypt_page(&plaintext).unwrap();

        let result = cipher2.decrypt_page(&encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = test_key();
        let cipher = Cipher::new(&key);

        let plaintext = [7u8; ENCRYPTED_PAYLOAD_SIZE];
        let mut encrypted = cipher.encrypt_page(&plaintext).unwrap();

        // Flip a byte in the ciphertext region (after the nonce).
        encrypted[NONCE_SIZE + 10] ^= 0xFF;

        let result = cipher.decrypt_page(&encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn unique_nonces_produce_different_ciphertext() {
        let key = test_key();
        let cipher = Cipher::new(&key);

        let plaintext = [0xAB; ENCRYPTED_PAYLOAD_SIZE];
        let enc1 = cipher.encrypt_page(&plaintext).unwrap();
        let enc2 = cipher.encrypt_page(&plaintext).unwrap();

        // Nonces (first 12 bytes) should differ.
        assert_ne!(&enc1[..NONCE_SIZE], &enc2[..NONCE_SIZE]);
        // Full pages should differ.
        assert_ne!(&enc1[..], &enc2[..]);
    }

    #[test]
    fn debug_does_not_leak_key() {
        let key = test_key();
        let cipher = Cipher::new(&key);
        let debug = format!("{:?}", cipher);
        assert_eq!(debug, "Cipher(***)");
    }
}

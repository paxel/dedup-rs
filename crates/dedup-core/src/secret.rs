//! Encrypting a small secret (a recovered archive password) at rest behind an
//! app passphrase. A confirmed working password is worth keeping so an archive
//! is never re-cracked — but storing it in the clear would turn a copied index
//! database into a password dump. So it is encrypted: the passphrase is stretched
//! with Argon2, and the secret sealed with XChaCha20-Poly1305 (authenticated, so
//! the wrong passphrase fails cleanly rather than returning garbage).
//!
//! The blob layout is `salt (16) || nonce (24) || ciphertext+tag`.

use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;

fn derive_key(passphrase: &str, salt: &[u8]) -> Option<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .ok()?;
    Some(key)
}

/// Seal `plaintext` under `passphrase`. Returns `salt || nonce || ciphertext`.
pub fn encrypt(passphrase: &str, plaintext: &str) -> Option<Vec<u8>> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).ok()?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes).ok()?;
    let key = derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, plaintext.as_bytes()).ok()?;
    let mut blob = Vec::with_capacity(SALT_LEN + NONCE_LEN + ct.len());
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    Some(blob)
}

/// Open a blob produced by [`encrypt`]. `None` on a wrong passphrase or a
/// corrupt/truncated blob — authentication makes the two indistinguishable,
/// which is the point.
pub fn decrypt(passphrase: &str, blob: &[u8]) -> Option<String> {
    if blob.len() < SALT_LEN + NONCE_LEN {
        return None;
    }
    let salt = &blob[..SALT_LEN];
    let nonce_bytes = &blob[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ct = &blob[SALT_LEN + NONCE_LEN..];
    let key = derive_key(passphrase, salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XNonce::from_slice(nonce_bytes);
    let pt = cipher.decrypt(nonce, ct).ok()?;
    String::from_utf8(pt).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_under_the_right_passphrase() {
        let blob = encrypt("app-passphrase", "hunter2").expect("encrypt");
        assert_eq!(
            decrypt("app-passphrase", &blob).as_deref(),
            Some("hunter2"),
            "the right passphrase recovers the password"
        );
    }

    #[test]
    fn the_blob_is_not_the_plaintext_and_a_wrong_passphrase_fails() {
        let blob = encrypt("correct", "s3cret-archive-pw").expect("encrypt");
        assert!(
            !blob
                .windows(b"s3cret-archive-pw".len())
                .any(|w| w == b"s3cret-archive-pw"),
            "the stored blob does not contain the plaintext password"
        );
        assert_eq!(
            decrypt("wrong", &blob),
            None,
            "a wrong passphrase yields nothing, not garbage"
        );
        assert_eq!(
            decrypt("correct", &[1, 2, 3]),
            None,
            "a truncated blob fails"
        );
    }

    #[test]
    fn each_encryption_uses_a_fresh_salt_and_nonce() {
        let a = encrypt("pw", "same").unwrap();
        let b = encrypt("pw", "same").unwrap();
        assert_ne!(
            a, b,
            "encrypting the same secret twice differs (fresh salt/nonce)"
        );
    }
}

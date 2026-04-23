//! ChaCha20-Poly1305 authenticated encryption.
//!
//! Wire format produced by `seal`:
//!
//! ```text
//! <alg:1B> <nonce:12B> <ciphertext:N> <tag:16B>
//! ```
//!
//! The 1-byte algorithm tag is present so a future rotation is additive —
//! v1 of the feature only emits `ALG_CHACHA20_POLY1305 = 0x01`, but readers
//! dispatch on this byte.
//!
//! One AEAD seal per chunk — for chunked large files, the chunk boundary is
//! the AEAD-frame boundary (not the 64 KiB read buffer). See doc 13
//! §Interaction-with-large-files.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};

use crate::error::{CryptoError, Result};

/// Algorithm tag for ChaCha20-Poly1305 in the sealed-blob wire format.
pub const ALG_CHACHA20_POLY1305: u8 = 0x01;

/// Tag size in bytes (Poly1305 auth tag).
pub const TAG_SIZE: usize = 16;

/// Nonce size in bytes (ChaCha20-Poly1305 uses a 96-bit nonce).
pub const NONCE_SIZE: usize = 12;

/// Generate a fresh random nonce from the OS CSPRNG. Callers that need a
/// deterministic per-chunk nonce should use
/// `crate::chunk_nonce::nonce_for_chunk` instead.
pub fn random_nonce() -> [u8; NONCE_SIZE] {
    use rand_core::{OsRng, RngCore};
    let mut out = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut out);
    out
}

/// Generate a fresh random 32-byte key (e.g., a per-file content key).
pub fn random_key() -> [u8; 32] {
    use rand_core::{OsRng, RngCore};
    let mut out = [0u8; 32];
    OsRng.fill_bytes(&mut out);
    out
}

/// Seal `plaintext` under `key` with `nonce` and additional authenticated
/// data `aad`. Returns the framed blob defined in the module docs.
///
/// Caller is responsible for supplying a nonce that is unique per `key`.
/// For chunked files, use `chunk_nonce::nonce_for_chunk` which derives a
/// deterministic unique nonce from the content key + chunk index.
pub fn seal(
    key: &[u8; 32],
    nonce: &[u8; NONCE_SIZE],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let ct = cipher
        .encrypt(
            Nonce::from_slice(nonce),
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|e| CryptoError::Aead(format!("encrypt: {e}")))?;

    let mut out = Vec::with_capacity(1 + NONCE_SIZE + ct.len());
    out.push(ALG_CHACHA20_POLY1305);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Inverse of `seal`. Errors on tamper (AEAD auth-tag mismatch), unknown
/// algorithm tag, or truncated input.
pub fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < 1 + NONCE_SIZE + TAG_SIZE {
        return Err(CryptoError::Invalid(format!(
            "sealed blob too short: {} bytes",
            sealed.len()
        )));
    }
    let alg = sealed[0];
    if alg != ALG_CHACHA20_POLY1305 {
        return Err(CryptoError::Invalid(format!(
            "unknown algorithm tag: 0x{alg:02x}"
        )));
    }
    let nonce: &[u8; NONCE_SIZE] = (&sealed[1..1 + NONCE_SIZE])
        .try_into()
        .expect("bounds-checked");
    let ct = &sealed[1 + NONCE_SIZE..];

    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            chacha20poly1305::aead::Payload { msg: ct, aad },
        )
        .map_err(|e| CryptoError::Aead(format!("decrypt: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let key = [1u8; 32];
        let nonce = [2u8; NONCE_SIZE];
        let sealed = seal(&key, &nonce, b"context", b"hello world").unwrap();
        let plain = open(&key, b"context", &sealed).unwrap();
        assert_eq!(plain, b"hello world");
    }

    #[test]
    fn tamper_on_ciphertext_fails_open() {
        let key = [1u8; 32];
        let nonce = [2u8; NONCE_SIZE];
        let mut sealed = seal(&key, &nonce, b"", b"secret").unwrap();
        let last = sealed.len() - TAG_SIZE - 1;
        sealed[last] ^= 1;
        let err = open(&key, b"", &sealed).unwrap_err();
        assert!(matches!(err, CryptoError::Aead(_)));
    }

    #[test]
    fn tamper_on_tag_fails_open() {
        let key = [1u8; 32];
        let nonce = [2u8; NONCE_SIZE];
        let mut sealed = seal(&key, &nonce, b"", b"secret").unwrap();
        let tag_start = sealed.len() - TAG_SIZE;
        sealed[tag_start] ^= 1;
        let err = open(&key, b"", &sealed).unwrap_err();
        assert!(matches!(err, CryptoError::Aead(_)));
    }

    #[test]
    fn wrong_aad_fails_open() {
        let key = [1u8; 32];
        let nonce = [2u8; NONCE_SIZE];
        let sealed = seal(&key, &nonce, b"context-a", b"secret").unwrap();
        let err = open(&key, b"context-b", &sealed).unwrap_err();
        assert!(matches!(err, CryptoError::Aead(_)));
    }

    #[test]
    fn unknown_alg_tag_is_rejected() {
        let mut sealed = seal(&[7u8; 32], &[0u8; NONCE_SIZE], b"", b"x").unwrap();
        sealed[0] = 0xff;
        let err = open(&[7u8; 32], b"", &sealed).unwrap_err();
        assert!(matches!(err, CryptoError::Invalid(_)));
    }

    #[test]
    fn too_short_is_rejected() {
        let err = open(&[0u8; 32], b"", &[0x01]).unwrap_err();
        assert!(matches!(err, CryptoError::Invalid(_)));
    }
}

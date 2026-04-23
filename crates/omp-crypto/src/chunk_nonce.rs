//! Per-chunk nonce derivation for encrypted chunked files.
//!
//! See `docs/design/13-end-to-end-encryption.md §Interaction with large
//! files`:
//!
//! > Each chunk `blob` is encrypted with the file's content key, using
//! > `nonce = hkdf(content_key, "chunk" || chunk_index)` — unique per chunk,
//! > deterministic given the content key, so chunked uploads don't need
//! > per-chunk nonce bookkeeping.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::aead::NONCE_SIZE;
use crate::error::Result;

/// Derive a 12-byte ChaCha20-Poly1305 nonce for chunk index `i` under the
/// file's content key. Deterministic; callers need not persist nonces.
pub fn nonce_for_chunk(content_key: &[u8; 32], chunk_index: u32) -> Result<[u8; NONCE_SIZE]> {
    let hk = Hkdf::<Sha256>::new(None, content_key);
    let mut info = Vec::with_capacity(5 + 4);
    info.extend_from_slice(b"chunk");
    info.extend_from_slice(&chunk_index.to_be_bytes());
    let mut out = [0u8; NONCE_SIZE];
    hk.expand(&info, &mut out)
        .map_err(|e| crate::error::CryptoError::Hkdf(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn different_indices_produce_different_nonces() {
        let key = [0x42u8; 32];
        let n0 = nonce_for_chunk(&key, 0).unwrap();
        let n1 = nonce_for_chunk(&key, 1).unwrap();
        assert_ne!(n0, n1);
    }

    #[test]
    fn different_keys_produce_different_nonces() {
        let a = nonce_for_chunk(&[0x01u8; 32], 0).unwrap();
        let b = nonce_for_chunk(&[0x02u8; 32], 0).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn deterministic() {
        let key = [0x11u8; 32];
        assert_eq!(
            nonce_for_chunk(&key, 42).unwrap(),
            nonce_for_chunk(&key, 42).unwrap()
        );
    }

    #[test]
    fn first_thousand_indices_are_pairwise_distinct() {
        let key = [0u8; 32];
        let mut seen = HashSet::new();
        for i in 0..1000u32 {
            let n = nonce_for_chunk(&key, i).unwrap();
            assert!(seen.insert(n), "collision at {i}");
        }
    }
}

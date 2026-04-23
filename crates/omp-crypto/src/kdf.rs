//! Key derivation — Argon2id for the root key, HKDF-SHA-256 for subkeys.
//! See `docs/design/13-end-to-end-encryption.md §Key hierarchy`.

use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

use crate::error::{CryptoError, Result};

/// Strong-default Argon2id parameters. These are memory-hard enough to
/// frustrate offline cracking on modern hardware but still derive in <1s on
/// a typical laptop: m=64 MiB, t=3, p=1.
pub const ARGON2_MEMORY_KIB: u32 = 64 * 1024;
pub const ARGON2_TIME_COST: u32 = 3;
pub const ARGON2_PARALLELISM: u32 = 1;

/// Derive the per-tenant root key from a passphrase.
///
/// `tenant_salt` must be non-empty and unique per tenant (the doc suggests
/// using the opaque tenant id issued at creation, so the same passphrase
/// reused across tenants produces different roots).
pub fn argon2id_root(passphrase: &[u8], tenant_salt: &[u8]) -> Result<[u8; 32]> {
    if tenant_salt.is_empty() {
        return Err(CryptoError::Invalid("tenant_salt is empty".into()));
    }
    if tenant_salt.len() < 8 {
        // argon2's minimum salt size — reject loudly rather than silently
        // padding so mis-use is obvious.
        return Err(CryptoError::Invalid(format!(
            "tenant_salt too short ({} bytes); Argon2 requires ≥ 8",
            tenant_salt.len()
        )));
    }
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_TIME_COST,
        ARGON2_PARALLELISM,
        Some(32),
    )
    .map_err(|e| CryptoError::Argon2(e.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut out = [0u8; 32];
    argon2
        .hash_password_into(passphrase, tenant_salt, &mut out)
        .map_err(|e| CryptoError::Argon2(e.to_string()))?;
    Ok(out)
}

/// Derive a 32-byte subkey from the root key using HKDF-SHA-256 with a
/// domain-separation `info` label. Deterministic — callers re-derive on
/// demand rather than storing subkeys.
///
/// Typical `info` values (per doc 13 §Key hierarchy):
///   - `"omp/data"`
///   - `"omp/manifest"`
///   - `"omp/path"`
///   - `"omp/commit"`
///   - `"omp/share"`
pub fn hkdf_subkey(root: &[u8; 32], info: &[u8]) -> Result<[u8; 32]> {
    // No salt — the root key is already uniform. HKDF's extract step is a
    // formality here; we feed the root as the IKM with an empty salt.
    let hk = Hkdf::<Sha256>::new(None, root);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .map_err(|e| CryptoError::Hkdf(e.to_string()))?;
    Ok(out)
}

/// Wipe a byte slice on drop. Useful at call sites that briefly hold a
/// subkey or a per-file content key.
pub fn wipe(bytes: &mut [u8; 32]) {
    bytes.zeroize();
}

/// HKDF-SHA-256 expand to an arbitrary output length. Callers pick the
/// `info` label (domain separation). No salt — inputs are expected to be
/// uniform key material already.
pub fn hkdf_bytes(input_key: &[u8], info: &[u8], out: &mut [u8]) -> Result<()> {
    let hk = Hkdf::<Sha256>::new(None, input_key);
    hk.expand(info, out)
        .map_err(|e| CryptoError::Hkdf(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2_deterministic_with_same_salt() {
        let a = argon2id_root(b"correct horse battery staple", b"alice-tenant").unwrap();
        let b = argon2id_root(b"correct horse battery staple", b"alice-tenant").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn argon2_differs_across_salts() {
        let a = argon2id_root(b"same passphrase", b"alice-tenant").unwrap();
        let b = argon2id_root(b"same passphrase", b"bob-tenant--").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn argon2_differs_across_passphrases() {
        let a = argon2id_root(b"first-passphrase", b"alice-tenant").unwrap();
        let b = argon2id_root(b"second-passphras", b"alice-tenant").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn argon2_rejects_short_salt() {
        let err = argon2id_root(b"x", b"short").unwrap_err();
        assert!(matches!(err, CryptoError::Invalid(_)));
    }

    #[test]
    fn hkdf_distinct_info_distinct_keys() {
        let root = [7u8; 32];
        let data = hkdf_subkey(&root, b"omp/data").unwrap();
        let manifest = hkdf_subkey(&root, b"omp/manifest").unwrap();
        assert_ne!(data, manifest);
    }

    #[test]
    fn hkdf_deterministic() {
        let root = [3u8; 32];
        let a = hkdf_subkey(&root, b"omp/share").unwrap();
        let b = hkdf_subkey(&root, b"omp/share").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn wipe_zeros_the_key() {
        let mut key = [9u8; 32];
        wipe(&mut key);
        assert_eq!(key, [0u8; 32]);
    }
}

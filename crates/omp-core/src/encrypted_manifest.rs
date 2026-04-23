//! Encrypted manifest envelope — a canonical-TOML outer wrapper holding:
//!
//!   - `wrapped_content_key` — hex AEAD(data_key, [per-file content key])
//!   - `sealed_body` — hex AEAD(manifest_key, [canonical-TOML of inner Manifest])
//!   - `alg` — algorithm tag ("chacha20poly1305") for both seals
//!
//! The outer envelope is the bytes that land in `ObjectType::Manifest`. A
//! server reading the envelope can inspect framing but not content — it
//! can't open either sealed payload without the tenant's keys.
//!
//! See `docs/design/13-end-to-end-encryption.md §What is encrypted`.

use serde::{Deserialize, Serialize};

use omp_crypto::aead::{self};

use crate::error::{OmpError, Result};
use crate::manifest::Manifest;
use crate::share::{hex_decode, hex_encode};
use crate::toml_canonical;

/// Fixed AAD labels binding a seal to its purpose — distinct labels for
/// the manifest body vs the content-key wrap prevent a ciphertext from one
/// slot being replayed into the other.
const AAD_MANIFEST: &[u8] = b"omp-encrypted-manifest";
const AAD_CONTENT_KEY_WRAP: &[u8] = b"omp-content-key-wrap";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncryptedManifestEnvelope {
    pub alg: String,
    pub wrapped_content_key: String,
    pub sealed_body: String,
}

const ALG: &str = "chacha20poly1305";

impl EncryptedManifestEnvelope {
    /// Seal a `Manifest` + the per-file content key used to encrypt the
    /// blob body. Returns the TOML bytes the caller writes as the
    /// `ObjectType::Manifest` payload.
    pub fn seal(
        manifest: &Manifest,
        content_key: &[u8; 32],
        manifest_key: &[u8; 32],
        data_key: &[u8; 32],
    ) -> Result<Vec<u8>> {
        let body_bytes = manifest.serialize()?;

        let wrap_nonce = aead::random_nonce();
        let wrapped = aead::seal(
            data_key,
            &wrap_nonce,
            AAD_CONTENT_KEY_WRAP,
            content_key,
        )
        .map_err(|e| OmpError::internal(format!("wrap content key: {e}")))?;

        let body_nonce = aead::random_nonce();
        let sealed = aead::seal(
            manifest_key,
            &body_nonce,
            AAD_MANIFEST,
            &body_bytes,
        )
        .map_err(|e| OmpError::internal(format!("seal manifest body: {e}")))?;

        let env = EncryptedManifestEnvelope {
            alg: ALG.into(),
            wrapped_content_key: hex_encode(&wrapped),
            sealed_body: hex_encode(&sealed),
        };
        let raw = toml::to_string(&env)
            .map_err(|e| OmpError::internal(format!("envelope to_string: {e}")))?;
        let canonical = toml_canonical::canonicalize(&raw)?;
        Ok(canonical.into_bytes())
    }

    /// Parse an envelope without decrypting anything. Always succeeds on a
    /// well-formed envelope; does not consult keys.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::Corrupt("envelope is not UTF-8".into()))?;
        let env: EncryptedManifestEnvelope = toml::from_str(s)
            .map_err(|e| OmpError::Corrupt(format!("envelope TOML: {e}")))?;
        if env.alg != ALG {
            return Err(OmpError::Corrupt(format!(
                "envelope alg {:?} not supported (want {:?})",
                env.alg, ALG
            )));
        }
        Ok(env)
    }

    /// Decrypt the envelope and recover `(Manifest, content_key)`.
    ///
    /// Fails if either sealed payload fails AEAD verification — the usual
    /// signal of a wrong key (or tampering).
    pub fn open(
        &self,
        manifest_key: &[u8; 32],
        data_key: &[u8; 32],
    ) -> Result<(Manifest, [u8; 32])> {
        let wrapped = hex_decode(&self.wrapped_content_key)?;
        let content_key_vec = aead::open(data_key, AAD_CONTENT_KEY_WRAP, &wrapped)
            .map_err(|_| OmpError::Unauthorized(
                "unable to unwrap content key (wrong key or tampered envelope)".into()
            ))?;
        let content_key: [u8; 32] = content_key_vec
            .as_slice()
            .try_into()
            .map_err(|_| OmpError::Corrupt(format!(
                "wrapped content key has wrong length: {}",
                content_key_vec.len()
            )))?;

        let sealed = hex_decode(&self.sealed_body)?;
        let body_bytes = aead::open(manifest_key, AAD_MANIFEST, &sealed)
            .map_err(|_| OmpError::Unauthorized(
                "unable to open manifest body (wrong key or tampered envelope)".into()
            ))?;
        let manifest = Manifest::parse(&body_bytes)?;
        Ok((manifest, content_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use std::collections::BTreeMap;

    fn minimal_manifest() -> Manifest {
        Manifest {
            source_hash: Hash::of(b"sealed blob"),
            file_type: "_minimal".into(),
            schema_hash: Hash::of(b"schema"),
            ingested_at: "2026-04-22T00:00:00Z".into(),
            ingester_version: "0.1.0".into(),
            probe_hashes: BTreeMap::new(),
            fields: BTreeMap::new(),
        }
    }

    #[test]
    fn envelope_seal_and_open_roundtrip() {
        let m = minimal_manifest();
        let content_key = [0x42u8; 32];
        let manifest_key = [0xaau8; 32];
        let data_key = [0x77u8; 32];

        let bytes = EncryptedManifestEnvelope::seal(
            &m,
            &content_key,
            &manifest_key,
            &data_key,
        )
        .unwrap();
        let env = EncryptedManifestEnvelope::parse(&bytes).unwrap();
        let (back, key) = env.open(&manifest_key, &data_key).unwrap();
        assert_eq!(back, m);
        assert_eq!(key, content_key);
    }

    #[test]
    fn wrong_manifest_key_fails_with_unauthorized() {
        let m = minimal_manifest();
        let bytes = EncryptedManifestEnvelope::seal(
            &m,
            &[0u8; 32],
            &[1u8; 32],
            &[2u8; 32],
        )
        .unwrap();
        let env = EncryptedManifestEnvelope::parse(&bytes).unwrap();
        let err = env.open(&[99u8; 32], &[2u8; 32]).unwrap_err();
        assert!(matches!(err, OmpError::Unauthorized(_)));
    }

    #[test]
    fn wrong_data_key_fails_with_unauthorized() {
        let m = minimal_manifest();
        let bytes = EncryptedManifestEnvelope::seal(
            &m,
            &[0u8; 32],
            &[1u8; 32],
            &[2u8; 32],
        )
        .unwrap();
        let env = EncryptedManifestEnvelope::parse(&bytes).unwrap();
        let err = env.open(&[1u8; 32], &[99u8; 32]).unwrap_err();
        assert!(matches!(err, OmpError::Unauthorized(_)));
    }

    #[test]
    fn envelope_bytes_do_not_contain_plaintext() {
        // A cursory check that the envelope doesn't leak identifiable
        // plaintext fragments. We encrypt a manifest whose file_type is a
        // deliberately-unique literal and confirm the literal isn't in the
        // outer bytes.
        let mut m = minimal_manifest();
        m.file_type = "unique-plaintext-marker-x7q2".into();
        let bytes = EncryptedManifestEnvelope::seal(
            &m,
            &[0u8; 32],
            &[1u8; 32],
            &[2u8; 32],
        )
        .unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            !s.contains("unique-plaintext-marker-x7q2"),
            "plaintext leaked into envelope"
        );
    }

    #[test]
    fn different_seals_produce_different_ciphertext() {
        // Fresh nonces each seal → outputs differ even with identical keys.
        let m = minimal_manifest();
        let a = EncryptedManifestEnvelope::seal(
            &m,
            &[0u8; 32],
            &[1u8; 32],
            &[2u8; 32],
        )
        .unwrap();
        let b = EncryptedManifestEnvelope::seal(
            &m,
            &[0u8; 32],
            &[1u8; 32],
            &[2u8; 32],
        )
        .unwrap();
        assert_ne!(a, b);
    }
}

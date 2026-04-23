//! `share` object body — grants a file's content key to one or more
//! recipients. See `docs/design/13-end-to-end-encryption.md §Sharing`.
//!
//! Canonical TOML body, stored plaintext. The `wrapped_key` per recipient
//! is already ciphertext (X25519 + ChaCha20-Poly1305 produced by
//! `omp_crypto::identity::wrap_to_recipient`), so the outer TOML need not
//! be encrypted.

use serde::{Deserialize, Serialize};

use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::tenant::TenantId;
use crate::toml_canonical;

// Backwards-compatible re-exports: callers inside this crate still use the
// `share::hex_encode` / `share::hex_decode` paths.
pub use crate::hex::{decode as hex_decode, encode as hex_encode};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareBody {
    /// The `source_hash` (pointing at a `blob` or `chunks` object) this
    /// share grants access to. Recipients look up the referenced object
    /// through the regular content-addressable store.
    pub for_hash: Hash,
    pub granted_by: TenantId,
    pub granted_at: String,
    pub recipients: Vec<Recipient>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipient {
    pub tenant: TenantId,
    /// Hex-encoded output of `omp_crypto::identity::wrap_to_recipient`.
    /// The payload is the content key encrypted to the recipient's
    /// published X25519 public key.
    pub wrapped_key: String,
    /// Wrap algorithm. v1 always `"x25519+chacha20poly1305"`.
    pub alg: String,
}

pub const SHARE_ALG: &str = "x25519+chacha20poly1305";

impl ShareBody {
    /// Serialize to the canonical-TOML wire format.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        // `toml::to_string` is not guaranteed canonical; run through the
        // canonicalizer used for manifests so hash stability is preserved.
        let raw = toml::to_string(self)
            .map_err(|e| OmpError::internal(format!("share to_string: {e}")))?;
        let canonical = toml_canonical::canonicalize(&raw)?;
        Ok(canonical.into_bytes())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::Corrupt("share body is not UTF-8".into()))?;
        toml::from_str(s).map_err(|e| OmpError::Corrupt(format!("share body: {e}")))
    }

    /// Locate the recipient entry for a given tenant, if present.
    pub fn recipient_for(&self, tenant: &TenantId) -> Option<&Recipient> {
        self.recipients.iter().find(|r| &r.tenant == tenant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ShareBody {
        ShareBody {
            for_hash: Hash::of(b"referenced blob"),
            granted_by: TenantId::new("alice").unwrap(),
            granted_at: "2026-04-22T10:00:00Z".into(),
            recipients: vec![
                Recipient {
                    tenant: TenantId::new("bob").unwrap(),
                    wrapped_key: "deadbeef".into(),
                    alg: SHARE_ALG.into(),
                },
                Recipient {
                    tenant: TenantId::new("carol").unwrap(),
                    wrapped_key: "cafebabe".into(),
                    alg: SHARE_ALG.into(),
                },
            ],
        }
    }

    #[test]
    fn serialize_is_canonical_and_roundtrips() {
        let body = fixture();
        let bytes = body.serialize().unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Canonical: a second pass is a no-op.
        assert_eq!(crate::toml_canonical::canonicalize(s).unwrap(), s);
        let back = ShareBody::parse(&bytes).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn recipient_for_finds_match() {
        let body = fixture();
        let bob = TenantId::new("bob").unwrap();
        assert_eq!(
            body.recipient_for(&bob).unwrap().wrapped_key,
            "deadbeef"
        );
        let dave = TenantId::new("dave").unwrap();
        assert!(body.recipient_for(&dave).is_none());
    }

}

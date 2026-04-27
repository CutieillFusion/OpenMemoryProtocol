//! Per-tenant key material derived from a passphrase. See
//! `docs/design/13-end-to-end-encryption.md §Key hierarchy`.
//!
//! Nothing in this module touches disk or the object store beyond the
//! thin `seal_identity_private` / `unseal_identity_private` helpers that
//! wrap/unwrap the X25519 private key under `identity_wrap_key`. The
//! CLI/client decides where to persist the sealed bytes (typically
//! `.omp/local.toml`).

use omp_crypto::identity::{generate_identity, IdentityPrivate};
use omp_crypto::kdf::{argon2id_root, hkdf_subkey};

use crate::error::{OmpError, Result};
use crate::tenant::TenantId;

/// Domain-separation labels for the HKDF subkeys derived from the tenant
/// root key. The labels are fixed — changing them breaks every repo
/// encrypted under the old label.
pub const DATA_KEY_INFO: &[u8] = b"omp/data";
pub const MANIFEST_KEY_INFO: &[u8] = b"omp/manifest";
pub const PATH_KEY_INFO: &[u8] = b"omp/path";
pub const COMMIT_KEY_INFO: &[u8] = b"omp/commit";
pub const SHARE_UNWRAP_KEY_INFO: &[u8] = b"omp/share";
/// Label used to wrap the identity private key for at-rest storage.
pub const IDENTITY_WRAP_KEY_INFO: &[u8] = b"omp/identity";

/// Derive a deterministic 12-byte AEAD nonce from `key`, a domain-separation
/// `label`, and content `data`. Used for tree-name and commit-message seals
/// so re-serialization is byte-stable (the framed-object hash depends on
/// deterministic output).
pub fn derive_nonce(key: &[u8; 32], label: &[u8], data: &[u8]) -> Result<[u8; 12]> {
    let mut info = Vec::with_capacity(label.len() + data.len());
    info.extend_from_slice(label);
    info.extend_from_slice(data);
    let mut out = [0u8; 12];
    omp_crypto::kdf::hkdf_bytes(key, &info, &mut out)
        .map_err(|e| OmpError::internal(format!("hkdf derive_nonce: {e}")))?;
    Ok(out)
}

/// All the symmetric subkeys plus the X25519 identity pair for one tenant.
pub struct TenantKeys {
    pub root: [u8; 32],
    pub data_key: [u8; 32],
    pub manifest_key: [u8; 32],
    pub path_key: [u8; 32],
    pub commit_key: [u8; 32],
    pub share_unwrap_key: [u8; 32],
    pub identity_wrap_key: [u8; 32],
    /// Present when the identity has been generated or loaded. `None`
    /// on first unlock before `generate_identity` is called.
    pub identity: Option<TenantIdentity>,
}

pub struct TenantIdentity {
    pub priv_key: IdentityPrivate,
    pub pub_key: [u8; 32],
}

impl TenantKeys {
    /// Derive all subkeys from a passphrase. The tenant id is used as the
    /// Argon2 salt so the same passphrase reused across tenants produces
    /// different roots (doc 13 §Key hierarchy).
    ///
    /// Argon2's minimum salt length is 8 bytes; tenant ids can be as short
    /// as 1 byte. We prefix with a fixed domain-separation label so every
    /// salt is at least 8 bytes without reducing entropy — this is safe
    /// because the salt's role is uniqueness, not secrecy.
    pub fn unlock(passphrase: &[u8], tenant: &TenantId) -> Result<Self> {
        let mut salt = Vec::with_capacity(16 + tenant.as_str().len());
        salt.extend_from_slice(b"omp-tenant:");
        salt.extend_from_slice(tenant.as_str().as_bytes());
        let root = argon2id_root(passphrase, &salt)
            .map_err(|e| OmpError::internal(format!("argon2id: {e}")))?;
        let _ = &salt; // explicit: salt is plaintext, no need to zeroize.
        let data_key = hkdf_subkey(&root, DATA_KEY_INFO)
            .map_err(|e| OmpError::internal(format!("hkdf data: {e}")))?;
        let manifest_key = hkdf_subkey(&root, MANIFEST_KEY_INFO)
            .map_err(|e| OmpError::internal(format!("hkdf manifest: {e}")))?;
        let path_key = hkdf_subkey(&root, PATH_KEY_INFO)
            .map_err(|e| OmpError::internal(format!("hkdf path: {e}")))?;
        let commit_key = hkdf_subkey(&root, COMMIT_KEY_INFO)
            .map_err(|e| OmpError::internal(format!("hkdf commit: {e}")))?;
        let share_unwrap_key = hkdf_subkey(&root, SHARE_UNWRAP_KEY_INFO)
            .map_err(|e| OmpError::internal(format!("hkdf share: {e}")))?;
        let identity_wrap_key = hkdf_subkey(&root, IDENTITY_WRAP_KEY_INFO)
            .map_err(|e| OmpError::internal(format!("hkdf identity: {e}")))?;
        Ok(TenantKeys {
            root,
            data_key,
            manifest_key,
            path_key,
            commit_key,
            share_unwrap_key,
            identity_wrap_key,
            identity: None,
        })
    }

    /// Generate a fresh X25519 identity keypair and attach it.
    pub fn generate_identity(&mut self) -> [u8; 32] {
        let (priv_key, pub_key) = generate_identity();
        self.identity = Some(TenantIdentity { priv_key, pub_key });
        pub_key
    }

    /// Attach a known identity keypair (for example, after unwrapping it
    /// from at-rest storage under `identity_wrap_key`).
    pub fn set_identity(&mut self, priv_key: IdentityPrivate, pub_key: [u8; 32]) {
        self.identity = Some(TenantIdentity { priv_key, pub_key });
    }

    pub fn identity(&self) -> Option<&TenantIdentity> {
        self.identity.as_ref()
    }

    /// Seal the currently-attached identity private key under
    /// `identity_wrap_key`. Returns a byte blob safe to persist to
    /// `.omp/local.toml` (still plaintext TOML-wise, but the private key
    /// inside is ciphertext). See doc 13 §Key management.
    ///
    /// Errors if no identity is attached.
    pub fn seal_identity_private(&self) -> Result<Vec<u8>> {
        use omp_crypto::aead;
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| OmpError::internal("no identity attached to keys"))?;
        let nonce = aead::random_nonce();
        aead::seal(
            &self.identity_wrap_key,
            &nonce,
            b"omp-identity-wrap",
            &identity.priv_key.0,
        )
        .map_err(|e| OmpError::internal(format!("seal identity: {e}")))
    }

    /// Open a blob produced by `seal_identity_private` and attach the
    /// identity to this `TenantKeys`. Returns `Unauthorized` on AEAD
    /// failure (wrong passphrase → different `identity_wrap_key`).
    pub fn unseal_and_attach_identity(&mut self, sealed: &[u8]) -> Result<()> {
        use omp_crypto::aead;
        use omp_crypto::identity::IdentityPrivate;
        let priv_bytes = aead::open(&self.identity_wrap_key, b"omp-identity-wrap", sealed)
            .map_err(|_| {
                OmpError::Unauthorized(
                    "unable to unseal identity private key (wrong passphrase?)".into(),
                )
            })?;
        let arr: [u8; 32] = priv_bytes
            .as_slice()
            .try_into()
            .map_err(|_| OmpError::Corrupt("sealed identity: wrong length".into()))?;
        let priv_key = IdentityPrivate(arr);
        let pub_key = priv_key.public();
        self.set_identity(priv_key, pub_key);
        Ok(())
    }
}

impl Drop for TenantKeys {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.root.zeroize();
        self.data_key.zeroize();
        self.manifest_key.zeroize();
        self.path_key.zeroize();
        self.commit_key.zeroize();
        self.share_unwrap_key.zeroize();
        self.identity_wrap_key.zeroize();
        if let Some(id) = self.identity.as_mut() {
            id.priv_key.0.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlock_is_deterministic_for_same_inputs() {
        let t = TenantId::new("alice").unwrap();
        let k1 = TenantKeys::unlock(b"correct horse battery staple", &t).unwrap();
        let k2 = TenantKeys::unlock(b"correct horse battery staple", &t).unwrap();
        assert_eq!(k1.data_key, k2.data_key);
        assert_eq!(k1.manifest_key, k2.manifest_key);
    }

    #[test]
    fn subkeys_are_pairwise_distinct() {
        let t = TenantId::new("alice").unwrap();
        let k = TenantKeys::unlock(b"hello-passphrase", &t).unwrap();
        let keys = [
            k.data_key,
            k.manifest_key,
            k.path_key,
            k.commit_key,
            k.share_unwrap_key,
            k.identity_wrap_key,
        ];
        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                assert_ne!(keys[i], keys[j], "subkeys {i} and {j} collide");
            }
        }
    }

    #[test]
    fn different_tenants_derive_different_keys() {
        let a = TenantKeys::unlock(b"same passphrase", &TenantId::new("alice").unwrap()).unwrap();
        let b = TenantKeys::unlock(b"same passphrase", &TenantId::new("bob").unwrap()).unwrap();
        assert_ne!(a.data_key, b.data_key);
    }

    #[test]
    fn generate_identity_produces_distinct_keypair() {
        let mut k = TenantKeys::unlock(b"hello", &TenantId::new("alice").unwrap()).unwrap();
        let pub_a = k.generate_identity();
        // Re-generate: different keypair.
        let pub_b = k.generate_identity();
        assert_ne!(pub_a, pub_b);
    }

    #[test]
    fn identity_seal_and_unseal_roundtrip() {
        let tenant = TenantId::new("alice").unwrap();
        let mut k = TenantKeys::unlock(b"the passphrase", &tenant).unwrap();
        let pub_orig = k.generate_identity();
        let sealed = k.seal_identity_private().unwrap();

        // Fresh session: unlock with the same passphrase, unseal.
        let mut k2 = TenantKeys::unlock(b"the passphrase", &tenant).unwrap();
        k2.unseal_and_attach_identity(&sealed).unwrap();
        assert_eq!(k2.identity().unwrap().pub_key, pub_orig);
    }

    #[test]
    fn identity_unseal_fails_with_wrong_passphrase() {
        let tenant = TenantId::new("alice").unwrap();
        let mut right = TenantKeys::unlock(b"correct", &tenant).unwrap();
        right.generate_identity();
        let sealed = right.seal_identity_private().unwrap();

        let mut wrong = TenantKeys::unlock(b"wrong--", &tenant).unwrap();
        let err = wrong.unseal_and_attach_identity(&sealed).unwrap_err();
        assert!(matches!(err, OmpError::Unauthorized(_)));
    }
}

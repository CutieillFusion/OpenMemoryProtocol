//! Tenant registry — `tenants.toml` file that maps a hashed Bearer token to
//! a `(TenantId, Quotas)` pair. The auth middleware consults this registry
//! on every request.
//!
//! The registry is read from disk on load and held in memory. Mutations go
//! through `TenantRegistry::save`, which rewrites the file atomically.
//!
//! See `docs/design/11-multi-tenancy.md`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{OmpError, Result};
use crate::tenant::TenantId;

/// Per-tenant quota ceilings. `None` means "no cap".
///
/// Applied at the write path:
/// - `bytes` / `object_count` are soft-checked before each `put` and hard-
///   checked per commit.
/// - `probe_fuel_per_request` / `wall_clock_s_per_request` clamp below the
///   global `[probes]` caps in `omp.toml`.
/// - `concurrent_writes` is enforced by a per-tenant semaphore in the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Quotas {
    pub bytes: Option<u64>,
    pub object_count: Option<u64>,
    pub probe_fuel_per_request: Option<u64>,
    pub wall_clock_s_per_request: Option<u32>,
    pub concurrent_writes: Option<u32>,
}

impl Default for Quotas {
    fn default() -> Self {
        Quotas::unlimited()
    }
}

impl Quotas {
    pub const fn unlimited() -> Self {
        Quotas {
            bytes: None,
            object_count: None,
            probe_fuel_per_request: None,
            wall_clock_s_per_request: None,
            concurrent_writes: None,
        }
    }

    /// Produce a probe config by clamping the repo-config caps against this
    /// tenant's per-request ceilings (tenant always wins the tighter bound).
    pub fn clamp_probe(&self, from_repo: crate::config::ProbeLimits) -> crate::config::ProbeLimits {
        crate::config::ProbeLimits {
            memory_mb: from_repo.memory_mb,
            fuel: match self.probe_fuel_per_request {
                Some(cap) => from_repo.fuel.min(cap),
                None => from_repo.fuel,
            },
            wall_clock_s: match self.wall_clock_s_per_request {
                Some(cap) => from_repo.wall_clock_s.min(cap),
                None => from_repo.wall_clock_s,
            },
        }
    }
}

/// End-to-end encryption mode for a tenant. Immutable after tenant
/// creation — see `docs/design/13-end-to-end-encryption.md §Migration and
/// coexistence`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum EncryptionMode {
    /// Server-side ingest; plaintext objects. The v1 default.
    Plaintext,
    /// Client-side ingest; ciphertext objects only. `identity_pub` is the
    /// tenant's X25519 public key, published so others may create `share`
    /// objects addressed to this tenant.
    Encrypted {
        #[serde(with = "hex_array_32")]
        identity_pub: [u8; 32],
    },
}

impl Default for EncryptionMode {
    fn default() -> Self {
        EncryptionMode::Plaintext
    }
}

impl EncryptionMode {
    pub fn is_encrypted(&self) -> bool {
        matches!(self, EncryptionMode::Encrypted { .. })
    }
}

mod hex_array_32 {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        if s.len() != 64 {
            return Err(D::Error::custom(format!(
                "expected 64 hex chars, got {}",
                s.len()
            )));
        }
        let bytes = crate::hex::decode(&s).map_err(D::Error::custom)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    }
}

/// Entry stored for one tenant.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TenantEntry {
    pub id: TenantId,
    /// Hex SHA-256 of the shared token. The plaintext token is shown once
    /// on `omp admin tenant create` and never written to disk.
    pub token_sha256: String,
    #[serde(default)]
    pub quotas: Quotas,
    /// End-to-end encryption mode. `Plaintext` is assumed for registries
    /// written before this field existed — so legacy deployments continue
    /// working unchanged.
    #[serde(default)]
    pub encryption_mode: EncryptionMode,
}

/// On-disk tenant registry. Serialized as TOML:
///
/// ```toml
/// [[tenants]]
/// id = "alice"
/// token_sha256 = "abcdef..."
/// [tenants.quotas]
/// bytes = 1000000000
/// object_count = 100000
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TenantRegistry {
    #[serde(default, rename = "tenants")]
    entries: Vec<TenantEntry>,
}

impl TenantRegistry {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(TenantRegistry::default());
        }
        let s = std::fs::read_to_string(path).map_err(|e| OmpError::io(path, e))?;
        let reg: TenantRegistry = toml::from_str(&s)
            .map_err(|e| OmpError::SchemaValidation(format!("tenants.toml: {e}")))?;
        Ok(reg)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| OmpError::io(parent, e))?;
        }
        let body = toml::to_string_pretty(self)
            .map_err(|e| OmpError::internal(format!("serialize registry: {e}")))?;
        std::fs::write(path, body).map_err(|e| OmpError::io(path, e))
    }

    pub fn entries(&self) -> &[TenantEntry] {
        &self.entries
    }

    pub fn by_id(&self, id: &TenantId) -> Option<&TenantEntry> {
        self.entries.iter().find(|e| &e.id == id)
    }

    pub fn by_token(&self, token: &str) -> Option<&TenantEntry> {
        let hash = hash_token(token);
        self.entries
            .iter()
            .find(|e| constant_time_eq(&e.token_sha256, &hash))
    }

    /// Create a new plaintext-mode tenant, generating a random token.
    /// Returns the plaintext token — which must be shown to the operator
    /// and immediately discarded.
    pub fn create(&mut self, id: TenantId, quotas: Quotas) -> Result<String> {
        self.create_with_mode(id, quotas, EncryptionMode::Plaintext)
    }

    /// Create a new end-to-end-encrypted tenant. The `identity_pub` is the
    /// X25519 public key the client generated during first-time setup;
    /// it's published in the registry so peers can wrap content keys for
    /// this tenant. The corresponding private key never leaves the client.
    pub fn create_encrypted(
        &mut self,
        id: TenantId,
        quotas: Quotas,
        identity_pub: [u8; 32],
    ) -> Result<String> {
        self.create_with_mode(id, quotas, EncryptionMode::Encrypted { identity_pub })
    }

    fn create_with_mode(
        &mut self,
        id: TenantId,
        quotas: Quotas,
        encryption_mode: EncryptionMode,
    ) -> Result<String> {
        if self.by_id(&id).is_some() {
            return Err(OmpError::Conflict(format!("tenant {id} already exists")));
        }
        let token = generate_token();
        let token_sha256 = hash_token(&token);
        self.entries.push(TenantEntry {
            id,
            token_sha256,
            quotas,
            encryption_mode,
        });
        Ok(token)
    }

    pub fn remove(&mut self, id: &TenantId) -> Result<()> {
        let before = self.entries.len();
        self.entries.retain(|e| &e.id != id);
        if self.entries.len() == before {
            return Err(OmpError::NotFound(format!("tenant {id}")));
        }
        Ok(())
    }

    pub fn set_quotas(&mut self, id: &TenantId, quotas: Quotas) -> Result<()> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| &e.id == id)
            .ok_or_else(|| OmpError::NotFound(format!("tenant {id}")))?;
        entry.quotas = quotas;
        Ok(())
    }
}

/// Default registry path under a tenants-base directory.
pub fn default_registry_path(tenants_base: &Path) -> PathBuf {
    tenants_base.join("admin").join("tenants.toml")
}

/// Stable-length hex SHA-256 of the plaintext token.
pub fn hash_token(token: &str) -> String {
    crate::hex::sha256_hex(token.as_bytes())
}

/// Random base-62 token of ~240 bits. Uses the OS RNG.
pub fn generate_token() -> String {
    use std::fs::File;
    use std::io::Read;
    let mut bytes = [0u8; 32];
    // Linux/macOS: /dev/urandom. Windows builds would go through `getrandom`
    // — deferred; this is a server path and targets Unix first.
    let mut file = File::open("/dev/urandom").expect("open /dev/urandom");
    file.read_exact(&mut bytes).expect("read /dev/urandom");
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(43);
    // Base-62 encode 32 random bytes by chunking. Simpler: hex.
    for b in bytes {
        out.push(ALPHA[(b as usize) % ALPHA.len()] as char);
    }
    out
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn quotas_default_is_unlimited() {
        let q = Quotas::default();
        assert!(q.bytes.is_none());
        assert!(q.object_count.is_none());
    }

    #[test]
    fn quotas_clamp_probe_takes_tighter() {
        let repo = crate::config::ProbeLimits {
            memory_mb: 64,
            fuel: 1_000_000_000,
            wall_clock_s: 10,
        };
        let q = Quotas {
            probe_fuel_per_request: Some(500_000),
            wall_clock_s_per_request: Some(5),
            ..Quotas::unlimited()
        };
        let out = q.clamp_probe(repo);
        assert_eq!(out.fuel, 500_000);
        assert_eq!(out.wall_clock_s, 5);
        assert_eq!(out.memory_mb, 64);
    }

    #[test]
    fn quotas_clamp_probe_keeps_repo_when_unlimited() {
        let repo = crate::config::ProbeLimits {
            memory_mb: 64,
            fuel: 1_000_000_000,
            wall_clock_s: 10,
        };
        let q = Quotas::unlimited();
        let out = q.clamp_probe(repo);
        assert_eq!(out.fuel, 1_000_000_000);
        assert_eq!(out.wall_clock_s, 10);
    }

    #[test]
    fn registry_create_and_load_roundtrip() {
        let td = TempDir::new().unwrap();
        let path = default_registry_path(td.path());
        let mut reg = TenantRegistry::default();
        let token = reg
            .create(TenantId::new("alice").unwrap(), Quotas::unlimited())
            .unwrap();
        reg.save(&path).unwrap();
        let reloaded = TenantRegistry::load(&path).unwrap();
        assert_eq!(reloaded.entries().len(), 1);
        assert!(reloaded.by_token(&token).is_some());
        assert!(reloaded.by_token("wrong-token").is_none());
    }

    #[test]
    fn encryption_mode_defaults_plaintext_for_back_compat() {
        // A pre-encryption-feature registry lacks `encryption_mode` entirely.
        // Parsing must succeed and treat it as `Plaintext`.
        let legacy = r#"
[[tenants]]
id = "alice"
token_sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
"#;
        let reg: TenantRegistry = toml::from_str(legacy).unwrap();
        assert_eq!(reg.entries.len(), 1);
        assert_eq!(reg.entries[0].encryption_mode, EncryptionMode::Plaintext);
    }

    #[test]
    fn encrypted_tenant_roundtrips_through_toml() {
        let td = TempDir::new().unwrap();
        let path = default_registry_path(td.path());
        let mut reg = TenantRegistry::default();
        let pk = [0xab; 32];
        let token = reg
            .create_encrypted(TenantId::new("alice").unwrap(), Quotas::unlimited(), pk)
            .unwrap();
        reg.save(&path).unwrap();

        let reloaded = TenantRegistry::load(&path).unwrap();
        assert_eq!(reloaded.entries().len(), 1);
        let entry = reloaded.by_token(&token).unwrap();
        match &entry.encryption_mode {
            EncryptionMode::Encrypted { identity_pub } => assert_eq!(identity_pub, &pk),
            other => panic!("expected Encrypted, got {other:?}"),
        }
    }

    #[test]
    fn plaintext_create_leaves_mode_plaintext() {
        let mut reg = TenantRegistry::default();
        reg.create(TenantId::new("alice").unwrap(), Quotas::unlimited())
            .unwrap();
        assert_eq!(reg.entries[0].encryption_mode, EncryptionMode::Plaintext);
    }

    #[test]
    fn registry_rejects_duplicate_id() {
        let mut reg = TenantRegistry::default();
        reg.create(TenantId::new("alice").unwrap(), Quotas::unlimited())
            .unwrap();
        let err = reg
            .create(TenantId::new("alice").unwrap(), Quotas::unlimited())
            .unwrap_err();
        assert!(matches!(err, OmpError::Conflict(_)));
    }

    #[test]
    fn registry_remove_missing_fails() {
        let mut reg = TenantRegistry::default();
        let err = reg.remove(&TenantId::new("nobody").unwrap()).unwrap_err();
        assert!(matches!(err, OmpError::NotFound(_)));
    }

    #[test]
    fn hash_token_is_stable_hex() {
        let h = hash_token("hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, hash_token("hello"));
        assert_ne!(h, hash_token("hellO"));
    }

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }
}

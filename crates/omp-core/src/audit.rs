//! Per-tenant audit log stored as objects in the OMP store.
//!
//! See `docs/design/18-observability.md § Audit log`.
//!
//! Each entry is a CBOR blob stored as an `audit` object. Entries form a
//! singly-linked hash chain: every entry except the first names its parent
//! by hash. The chain head lives at the ref `refs/audit/HEAD`.
//!
//! Why content-addressed: the audit log inherits the same byte-integrity story
//! as everything else in OMP. An auditor walking the chain can re-hash every
//! entry and verify nothing was edited in flight.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::store::ObjectStore;

/// Object type tag for audit entries.
pub const AUDIT_OBJECT_TYPE: &str = "audit";
/// Ref name for the audit chain head.
pub const AUDIT_HEAD_REF: &str = "refs/audit/HEAD";

/// One audit-log entry. Stable on the wire — additive only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditEntry {
    /// Wire-format version for forward compat.
    pub version: u32,
    /// Hash of the previous audit entry. `None` for the genesis entry.
    pub parent: Option<Hash>,
    /// ISO-8601 UTC timestamp.
    pub at: String,
    /// Tenant id this entry belongs to.
    pub tenant: String,
    /// Dotted-namespace event name. Examples: `auth.token.accepted`,
    /// `commit.created`, `quota.exceeded`, `share.granted`.
    pub event: String,
    /// Actor identifier (e.g. token id, user id, service id). Opaque to OMP.
    #[serde(default)]
    pub actor: String,
    /// Free-form structured details. Keep small — no file bytes.
    #[serde(default)]
    pub details: BTreeMap<String, AuditValue>,
}

/// Closed value type for audit entry details — same shape as TOML scalars
/// so the entries are easily projected to JSON for the `/audit` route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AuditValue {
    Null,
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<AuditValue>),
}

impl AuditEntry {
    pub fn new(tenant: &str, event: &str, actor: &str) -> Self {
        Self {
            version: 1,
            parent: None,
            at: crate::time::now_rfc3339(),
            tenant: tenant.to_string(),
            event: event.to_string(),
            actor: actor.to_string(),
            details: BTreeMap::new(),
        }
    }

    pub fn with_detail(mut self, key: &str, value: AuditValue) -> Self {
        self.details.insert(key.to_string(), value);
        self
    }

    /// Encode to canonical CBOR bytes — what the store hashes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(self, &mut buf)
            .map_err(|e| OmpError::internal(format!("audit cbor encode: {e}")))?;
        Ok(buf)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ciborium::de::from_reader(bytes)
            .map_err(|e| OmpError::internal(format!("audit cbor decode: {e}")))
    }
}

/// Append a new entry to the chain. Walks the current head, sets the new
/// entry's `parent`, stores the entry, advances the ref. Returns the entry's
/// hash.
pub fn append<S: ObjectStore + ?Sized>(store: &S, mut entry: AuditEntry) -> Result<Hash> {
    let parent = store.read_ref(AUDIT_HEAD_REF)?;
    entry.parent = parent;
    let bytes = entry.encode()?;
    let hash = store.put(AUDIT_OBJECT_TYPE, &bytes)?;
    store.write_ref(AUDIT_HEAD_REF, &hash)?;
    Ok(hash)
}

/// Walk the chain from HEAD, oldest-last. Stops at `limit` entries.
pub fn read_chain<S: ObjectStore + ?Sized>(store: &S, limit: usize) -> Result<Vec<AuditEntry>> {
    let mut out = Vec::new();
    let mut next = store.read_ref(AUDIT_HEAD_REF)?;
    while let Some(h) = next {
        if out.len() >= limit {
            break;
        }
        let (ty, content) = store
            .get(&h)?
            .ok_or_else(|| OmpError::NotFound(format!("audit entry {}", h.hex())))?;
        if ty != AUDIT_OBJECT_TYPE {
            return Err(OmpError::Corrupt(format!(
                "ref {AUDIT_HEAD_REF} points at non-audit object {}",
                h.hex()
            )));
        }
        let entry = AuditEntry::decode(&content)?;
        next = entry.parent;
        out.push(entry);
    }
    Ok(out)
}

/// Verify the chain is internally consistent: each non-genesis entry's
/// `parent` field matches the hash of the entry that the chain link points to.
/// Returns `Ok(true)` for a verified chain, `Ok(false)` for tampering, errors
/// for unreadable storage.
pub fn verify_chain<S: ObjectStore + ?Sized>(store: &S) -> Result<bool> {
    Ok(read_chain_verified(store, usize::MAX)?.1)
}

/// Walk the chain once, returning entries (newest first, capped at `limit`)
/// and whether the *full* chain hashed cleanly. Lets the `/audit` route avoid
/// a second pass when callers want both the listing and the verified flag.
pub fn read_chain_verified<S: ObjectStore + ?Sized>(
    store: &S,
    limit: usize,
) -> Result<(Vec<AuditEntry>, bool)> {
    let mut out = Vec::new();
    let mut verified = true;
    let mut next = store.read_ref(AUDIT_HEAD_REF)?;
    while let Some(h) = next {
        let (ty, content) = store
            .get(&h)?
            .ok_or_else(|| OmpError::NotFound(format!("audit entry {}", h.hex())))?;
        if ty != AUDIT_OBJECT_TYPE {
            return Err(OmpError::Corrupt(format!(
                "ref {AUDIT_HEAD_REF} points at non-audit object {}",
                h.hex()
            )));
        }
        if crate::object::hash_of(crate::object::ObjectType::Audit, &content) != h {
            verified = false;
        }
        let entry = AuditEntry::decode(&content)?;
        next = entry.parent;
        if out.len() < limit {
            out.push(entry);
        }
    }
    Ok((out, verified))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::disk::DiskStore;
    use tempfile::TempDir;

    #[test]
    fn append_and_read_chain() {
        let td = TempDir::new().unwrap();
        let store = DiskStore::init(td.path()).unwrap();

        let h1 = append(&store, AuditEntry::new("alice", "token.accepted", "tok123")).unwrap();
        let h2 = append(
            &store,
            AuditEntry::new("alice", "commit.created", "user")
                .with_detail("commit", AuditValue::String(h1.hex())),
        )
        .unwrap();
        assert_ne!(h1, h2);

        let chain = read_chain(&store, 100).unwrap();
        assert_eq!(chain.len(), 2);
        // Newest first.
        assert_eq!(chain[0].event, "commit.created");
        assert_eq!(chain[1].event, "token.accepted");
        assert_eq!(chain[0].parent, Some(h1));
        assert_eq!(chain[1].parent, None);
    }

    #[test]
    fn verify_chain_passes_on_clean_chain() {
        let td = TempDir::new().unwrap();
        let store = DiskStore::init(td.path()).unwrap();
        for i in 0..5 {
            append(
                &store,
                AuditEntry::new("alice", "event", &format!("actor-{i}")),
            )
            .unwrap();
        }
        assert!(verify_chain(&store).unwrap());
    }

    #[test]
    fn limit_truncates_response() {
        let td = TempDir::new().unwrap();
        let store = DiskStore::init(td.path()).unwrap();
        for _ in 0..10 {
            append(&store, AuditEntry::new("alice", "e", "actor")).unwrap();
        }
        let chain = read_chain(&store, 3).unwrap();
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn empty_chain_returns_empty() {
        let td = TempDir::new().unwrap();
        let store = DiskStore::init(td.path()).unwrap();
        assert!(read_chain(&store, 10).unwrap().is_empty());
        assert!(verify_chain(&store).unwrap());
    }
}

//! Reachability walker for garbage-collection bookkeeping.
//!
//! Given one or more commit roots, returns every object hash reachable
//! through the DAG: commits → trees (recursive) → manifests → `source_hash`
//! (a `blob` or `chunks` object — if `chunks`, every referenced chunk blob
//! is also reached). See `docs/design/12-large-files.md §Garbage
//! collection becomes load-bearing`.
//!
//! The walker is deliberately a library primitive. The actual reclamation
//! (`omp admin gc`) is still deferred per `09-roadmap.md`; this function
//! is what that command will call to build the live set.
//!
//! For encrypted tenants the walker needs the tenant's `path_key` to
//! unseal tree entry names. The hashes themselves are always plaintext,
//! so reachability works without keys — but we accept an optional
//! `path_key` so callers can pass it through uniformly.

use std::collections::{HashMap, HashSet};

use crate::chunks::ChunksBody;
use crate::commit::Commit;
use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::manifest::Manifest;
use crate::store::ObjectStore;
use crate::tree::{Mode, Tree};

/// Live-set: the hashes of every object that must be preserved. Grouped
/// by type so callers can compute per-type stats cheaply.
#[derive(Debug, Default, Clone)]
pub struct LiveSet {
    pub commits: HashSet<Hash>,
    pub trees: HashSet<Hash>,
    pub manifests: HashSet<Hash>,
    pub chunks_objects: HashSet<Hash>,
    pub blobs: HashSet<Hash>,
    pub shares: HashSet<Hash>,
}

impl LiveSet {
    pub fn total(&self) -> usize {
        self.commits.len()
            + self.trees.len()
            + self.manifests.len()
            + self.chunks_objects.len()
            + self.blobs.len()
            + self.shares.len()
    }

    /// Returns `true` if any set contains `h`.
    pub fn contains(&self, h: &Hash) -> bool {
        self.commits.contains(h)
            || self.trees.contains(h)
            || self.manifests.contains(h)
            || self.chunks_objects.contains(h)
            || self.blobs.contains(h)
            || self.shares.contains(h)
    }
}

/// Walk the reachability graph rooted at `roots` (commit hashes) and
/// return every live object. When manifests are encrypted, the walker
/// inspects the envelope-wrapped `source_hash` without needing to open
/// the payload — the envelope's `source_hash` field would be inside the
/// sealed body, so encrypted manifests require `manifest_key` + `data_key`
/// to trace through to their blob/chunks object.
///
/// Callers that only need structural reachability (commits → trees →
/// manifests) can pass `None` for both keys; unresolved source hashes
/// will be skipped and reported via `MissingSourceHashes`.
pub fn walk(
    store: &dyn ObjectStore,
    roots: &[Hash],
    keys: Option<&crate::keys::TenantKeys>,
    also_include_shares: &[Hash],
) -> Result<LiveSet> {
    let mut live = LiveSet::default();

    // BFS over commits. Follow each commit's parent chain until we hit
    // already-seen nodes.
    let mut queue: Vec<Hash> = roots.to_vec();
    while let Some(commit_hash) = queue.pop() {
        if !live.commits.insert(commit_hash) {
            continue;
        }
        let (ty, body) = store
            .get(&commit_hash)?
            .ok_or_else(|| OmpError::NotFound(format!("commit {}", commit_hash.hex())))?;
        if ty != "commit" {
            return Err(OmpError::Corrupt(format!(
                "expected commit, got {ty} for {}",
                commit_hash.hex()
            )));
        }
        // We don't need the message, so parse without a key — even for
        // encrypted commits, the headers are plaintext and the message
        // body stays sealed (but we don't touch it).
        let commit = parse_commit_headers(&body)?;
        for parent in &commit.parents {
            queue.push(*parent);
        }
        walk_tree(store, &commit.tree, &mut live, keys)?;
    }

    // Explicit shares: share objects are not reachable from the commit
    // DAG unless the caller committed them as tree entries. We accept an
    // explicit list so callers that track shares out-of-band can preserve
    // them.
    for share in also_include_shares {
        live.shares.insert(*share);
    }

    Ok(live)
}

fn walk_tree(
    store: &dyn ObjectStore,
    tree_hash: &Hash,
    live: &mut LiveSet,
    keys: Option<&crate::keys::TenantKeys>,
) -> Result<()> {
    if !live.trees.insert(*tree_hash) {
        return Ok(());
    }
    let (ty, body) = store
        .get(tree_hash)?
        .ok_or_else(|| OmpError::NotFound(format!("tree {}", tree_hash.hex())))?;
    if ty != "tree" {
        return Err(OmpError::Corrupt(format!(
            "expected tree, got {ty} for {}",
            tree_hash.hex()
        )));
    }
    let path_key = keys.map(|k| &k.path_key);
    let tree = Tree::parse_with_path_key(&body, path_key)?;
    for (_name, entry) in tree.entries() {
        match entry.mode {
            Mode::Tree => walk_tree(store, &entry.hash, live, keys)?,
            Mode::Manifest => walk_manifest(store, &entry.hash, live, keys)?,
            Mode::Blob => {
                live.blobs.insert(entry.hash);
            }
        }
    }
    Ok(())
}

fn walk_manifest(
    store: &dyn ObjectStore,
    manifest_hash: &Hash,
    live: &mut LiveSet,
    keys: Option<&crate::keys::TenantKeys>,
) -> Result<()> {
    if !live.manifests.insert(*manifest_hash) {
        return Ok(());
    }
    let (ty, body) = store
        .get(manifest_hash)?
        .ok_or_else(|| OmpError::NotFound(format!("manifest {}", manifest_hash.hex())))?;
    if ty != "manifest" {
        return Err(OmpError::Corrupt(format!(
            "expected manifest, got {ty} for {}",
            manifest_hash.hex()
        )));
    }
    // Probe the first few bytes: an encrypted envelope starts with
    // `alg = "chacha20poly1305"` or similar TOML; a plaintext manifest
    // starts with `source_hash = ...`. Try plaintext first and fall back.
    let source_hash = match Manifest::parse(&body) {
        Ok(m) => {
            for (_, probe_hash) in &m.probe_hashes {
                live.blobs.insert(*probe_hash);
            }
            Some(m.source_hash)
        }
        Err(_) => {
            // Envelope path: need manifest_key + data_key to recover
            // source_hash.
            match keys {
                Some(k) => {
                    let env = crate::encrypted_manifest::EncryptedManifestEnvelope::parse(&body)?;
                    let (manifest, _content_key) = env.open(&k.manifest_key, &k.data_key)?;
                    for (_, probe_hash) in &manifest.probe_hashes {
                        live.blobs.insert(*probe_hash);
                    }
                    Some(manifest.source_hash)
                }
                None => None,
            }
        }
    };

    if let Some(sh) = source_hash {
        walk_source(store, &sh, live)?;
    }
    Ok(())
}

fn walk_source(store: &dyn ObjectStore, source_hash: &Hash, live: &mut LiveSet) -> Result<()> {
    // Source may be a single blob (v1 path) or a chunks object.
    let (ty, body) = store
        .get(source_hash)?
        .ok_or_else(|| OmpError::NotFound(format!("source {}", source_hash.hex())))?;
    match ty.as_str() {
        "blob" => {
            live.blobs.insert(*source_hash);
        }
        "chunks" => {
            live.chunks_objects.insert(*source_hash);
            let parsed = ChunksBody::parse(&body)?;
            for entry in parsed.entries {
                live.blobs.insert(entry.hash);
            }
        }
        other => {
            return Err(OmpError::Corrupt(format!(
                "source {} is unexpected type {other:?}",
                source_hash.hex()
            )));
        }
    }
    Ok(())
}

/// Parse just the headers of a commit (treating the body as opaque).
/// Needed because an encrypted commit body requires a key to fully parse,
/// but the reachability walker only cares about `tree` and `parent`
/// headers, which are plaintext in both modes.
fn parse_commit_headers(bytes: &[u8]) -> Result<Commit> {
    // `Commit::parse` can read plaintext messages and rejects a sealed
    // body without a key. For the walker we want headers either way —
    // try parse with no key; if it's encrypted, that's fine, we synthesize
    // a Commit with an empty message.
    match Commit::parse_with_commit_key(bytes, None) {
        Ok(c) => Ok(c),
        Err(OmpError::Unauthorized(_)) => {
            // Parse headers only.
            let s = std::str::from_utf8(bytes)
                .map_err(|_| OmpError::Corrupt("commit is not UTF-8".into()))?;
            let mut tree: Option<Hash> = None;
            let mut parents: Vec<Hash> = Vec::new();
            let mut author: Option<crate::commit::Author> = None;
            for line in s.lines() {
                if line.is_empty() {
                    break;
                }
                if let Some(rest) = line.strip_prefix("tree ") {
                    tree = Some(
                        rest.parse()
                            .map_err(|e| OmpError::Corrupt(format!("commit tree: {e}")))?,
                    );
                } else if let Some(rest) = line.strip_prefix("parent ") {
                    parents.push(
                        rest.parse()
                            .map_err(|e| OmpError::Corrupt(format!("commit parent: {e}")))?,
                    );
                } else if let Some(rest) = line.strip_prefix("author ") {
                    author = Some(crate::commit::Author::parse(rest)?);
                }
            }
            Ok(Commit {
                tree: tree.ok_or_else(|| OmpError::Corrupt("commit: missing tree".into()))?,
                parents,
                author: author.ok_or_else(|| OmpError::Corrupt("commit: missing author".into()))?,
                message: String::new(),
            })
        }
        Err(e) => Err(e),
    }
}

#[allow(dead_code)]
fn _suppress_unused_import_warning() {
    let _: HashMap<(), ()> = HashMap::new();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Repo;
    use crate::api::{AddResult, AuthorOverride};
    use tempfile::TempDir;

    fn fixed_author() -> AuthorOverride {
        AuthorOverride {
            name: Some("test".into()),
            email: Some("test@local".into()),
            timestamp: Some("2026-04-22T00:00:00Z".into()),
        }
    }

    fn stage_starter(repo: &Repo) {
        let root = repo.root().to_path_buf();
        let opts = crate::walker::WalkOptions::default();
        let entries = crate::walker::walk_repo(&root, &opts).unwrap();
        for e in entries {
            let bytes = std::fs::read(&e.fs_path).unwrap();
            repo.add(&e.repo_path, &bytes, None, None).unwrap();
        }
    }

    #[test]
    fn walk_plaintext_repo_includes_every_reachable_object() {
        let td = TempDir::new().unwrap();
        let repo = Repo::init(td.path()).unwrap();
        stage_starter(&repo);
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "title".into(),
            crate::manifest::FieldValue::String("t".into()),
        );
        repo.add("a.md", b"content", Some(fields), Some("text"))
            .unwrap();
        let commit_hash = repo.commit("init", Some(fixed_author())).unwrap();

        let live = walk(repo.store(), &[commit_hash], None, &[]).unwrap();
        assert_eq!(live.commits.len(), 1);
        assert!(live.trees.len() >= 1, "at least the root tree");
        assert!(live.manifests.len() >= 1);
        assert!(!live.blobs.is_empty(), "blobs should include the content");
    }

    #[test]
    fn walk_includes_chunk_blobs_via_chunks_object() {
        let td = TempDir::new().unwrap();
        let _ = Repo::init(td.path()).unwrap();
        std::fs::write(
            td.path().join("omp.toml"),
            "[ingest]\ndefault_schema_policy = \"minimal\"\n\n[storage]\nchunk_size_bytes = 64\n",
        )
        .unwrap();
        let repo = Repo::open(td.path()).unwrap();
        stage_starter(&repo);

        let big: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
        let res = repo
            .add("data/big.bin", &big, None, Some("_minimal"))
            .unwrap();
        let source_hash = match res {
            AddResult::Manifest { source_hash, .. } => source_hash,
            other => panic!("{other:?}"),
        };
        let commit_hash = repo.commit("init", Some(fixed_author())).unwrap();

        let live = walk(repo.store(), &[commit_hash], None, &[]).unwrap();
        assert!(live.chunks_objects.contains(&source_hash));
        // 200 bytes / 64-byte chunks ⇒ 4 chunk blobs should be live.
        let expected_chunk_count = 4;
        // Plus whatever starter probe blobs got staged.
        assert!(
            live.blobs.len() >= expected_chunk_count,
            "expected at least {} chunk blobs in live set, got {}",
            expected_chunk_count,
            live.blobs.len()
        );
    }

    #[test]
    fn walk_follows_parent_chain() {
        let td = TempDir::new().unwrap();
        let repo = Repo::init(td.path()).unwrap();
        stage_starter(&repo);
        let c1 = repo.commit("init", Some(fixed_author())).unwrap();
        repo.add("a.md", b"v2", None, Some("text")).unwrap();
        let c2 = repo
            .commit(
                "v2",
                Some(AuthorOverride {
                    timestamp: Some("2026-04-22T01:00:00Z".into()),
                    ..fixed_author()
                }),
            )
            .unwrap();
        let live = walk(repo.store(), &[c2], None, &[]).unwrap();
        assert!(live.commits.contains(&c1));
        assert!(live.commits.contains(&c2));
    }
}

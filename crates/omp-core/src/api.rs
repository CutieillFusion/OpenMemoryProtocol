//! The `Repo` handle — every documented operation, shared by CLI and HTTP.
//!
//! See `docs/design/06-api-surface.md` for the contract.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::commit::{Author, Commit};
use crate::config::{LocalConfig, RepoConfig};
use crate::engine::{self, IngestInput, ProbeBlob, ProbeOutputCache, TreeView};
use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::manifest::{FieldValue, Manifest};
use crate::object::{hash_of, ObjectType};
use crate::paths;
use crate::probes::starter::STARTER_PROBES;
use crate::refs;
use crate::registry::Quotas;
use crate::schema::{RenderHint, RenderKind, Schema};
use crate::store::disk::DiskStore;
use crate::store::ObjectStore;
use crate::tenant::TenantId;
use crate::tree::{Entry, Mode, Tree};
use crate::walker;

/// Maps a field name → a user-provided value.
pub type Fields = BTreeMap<String, FieldValue>;

/// Infer a render hint for a blob whose own bytes are not interpretable as
/// a manifest — i.e. files in `schemas/`, `probes/`, and `omp.toml`. The
/// schema's own `[render]` block only applies to file_types declared in the
/// schema; the schema *file itself* needs a separate hint or the UI shows
/// "Binary file — no inline preview" for what is plainly readable text.
fn render_for_blob_path(path: &str) -> RenderHint {
    let lower = path.to_lowercase();
    let kind = if lower.ends_with(".md") || lower.ends_with(".markdown") {
        RenderKind::Markdown
    } else if lower.ends_with(".toml")
        || lower.ends_with(".txt")
        || lower.ends_with(".json")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".rs")
        || lower.ends_with(".py")
        || lower.ends_with(".ts")
        || lower.ends_with(".js")
        || lower.ends_with(".sh")
        || lower.ends_with(".css")
        || lower.ends_with(".html")
    {
        RenderKind::Text
    } else {
        RenderKind::Binary
    };
    RenderHint {
        kind,
        max_inline_bytes: None,
    }
}

/// If `path` is `schemas/<file_type>/schema.toml` for a single-segment
/// `<file_type>`, return the file_type. Otherwise `None`. The per-folder
/// layout is documented in `docs/design/25-schema-marketplace.md`.
fn schema_file_type_from_path(path: &str) -> Option<&str> {
    let stem = path.strip_prefix("schemas/")?.strip_suffix("/schema.toml")?;
    if stem.is_empty() || stem.contains('/') {
        return None;
    }
    Some(stem)
}

/// Same as `schema_file_type_from_path` but for a tree-walked entry name
/// already relative to `schemas/` (e.g. `text/schema.toml`).
fn schema_file_type_from_tree_name(name: &str) -> Option<&str> {
    let stem = name.strip_suffix("/schema.toml")?;
    if stem.is_empty() || stem.contains('/') {
        return None;
    }
    Some(stem)
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind")]
pub enum AddResult {
    #[serde(rename = "manifest")]
    Manifest {
        path: String,
        manifest_hash: Hash,
        source_hash: Hash,
    },
    #[serde(rename = "blob")]
    Blob {
        path: String,
        blob_hash: Hash,
        size: usize,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind")]
pub enum ShowResult {
    #[serde(rename = "manifest")]
    Manifest {
        path: String,
        manifest: Manifest,
        render: RenderHint,
    },
    #[serde(rename = "blob")]
    Blob {
        path: String,
        blob_hash: Hash,
        size: usize,
        render: RenderHint,
    },
    #[serde(rename = "tree")]
    Tree {
        path: String,
        entries: Vec<TreeEntryOut>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct TreeEntryOut {
    pub name: String,
    pub mode: String, // "blob" | "manifest" | "tree"
    pub hash: Hash,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileListing {
    pub path: String,
    pub manifest_hash: Hash,
    pub source_hash: Hash,
    pub file_type: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BranchInfo {
    pub name: String,
    pub head: Option<Hash>,
    pub is_current: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepoStatus {
    pub branch: Option<String>,
    pub head: Option<Hash>,
    pub staged: Vec<StagedChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StagedChange {
    pub path: String,
    pub kind: StagedKind,
    pub hash: Option<Hash>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StagedKind {
    Upsert,
    Delete,
}

#[derive(Clone, Debug, Serialize)]
pub struct CommitView {
    pub hash: Hash,
    pub tree: Hash,
    pub parents: Vec<Hash>,
    pub author: String,
    pub email: String,
    pub timestamp: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiffEntry {
    pub path: String,
    pub status: DiffStatus,
    pub before: Option<Hash>,
    pub after: Option<Hash>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffStatus {
    Added,
    Removed,
    Modified,
    Unchanged,
}

/// Per-file failure inside an automatic reprobe pass. The commit still
/// succeeds; the path retains its previous manifest. See
/// `docs/design/21-schema-reprobe.md` §"Failure handling".
#[derive(Clone, Debug, Serialize)]
pub struct ReprobeSkip {
    pub path: String,
    pub reason: String,
}

/// Per-file_type summary returned alongside a successful commit when one or
/// more `schemas/<X>/schema.toml` blobs in the same commit triggered a reprobe.
#[derive(Clone, Debug, Serialize)]
pub struct ReprobeSummary {
    pub file_type: String,
    /// Number of manifests successfully rebuilt against the new schema.
    pub count: usize,
    /// Files where reprobe failed (probe error, missing source, …). The
    /// commit still succeeded; these paths retain their previous manifest.
    pub skipped: Vec<ReprobeSkip>,
}

/// Persistent staging index on disk at `.omp/index.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    /// path -> staged change
    entries: HashMap<String, StagedChange>,
}

/// Internal: how the ingest engine should source `source_hash` and
/// whether streaming built-ins are available. See
/// `docs/design/12-large-files.md`.
enum ChunkedIngest {
    /// v1 path — `source_hash` is `hash_of(Blob, bytes)`; no built-ins.
    None,
    /// Chunked path — the caller already wrote the chunks object and
    /// knows its hash, plus `file.size` / `file.sha256` over the
    /// plaintext. The engine uses these directly for any probe whose
    /// `max_input_bytes` is exceeded.
    Chunked {
        source_hash: Hash,
        file_size: u64,
        sha256_hex: String,
        /// A prefix of the plaintext (first chunk) used for MIME
        /// detection. The engine itself sees no plaintext for chunked
        /// ingest — only this slice is consulted, and only for sniffing.
        sniff_prefix: Vec<u8>,
    },
}

/// Per-call overrides: lets tests pin the clock and author.
#[derive(Clone, Debug, Default)]
pub struct AuthorOverride {
    pub name: Option<String>,
    pub email: Option<String>,
    pub timestamp: Option<String>,
}

pub struct Repo {
    root: PathBuf,
    store: DiskStore,
    /// Which tenant owns this handle. Tenancy is pinned at construction time:
    /// there is no operation that retargets a `Repo` at a different tenant,
    /// so handlers downstream of the auth middleware cannot cross tenants.
    /// See `docs/design/11-multi-tenancy.md`.
    tenant: TenantId,
    /// Per-tenant quota ceiling. `Quotas::unlimited()` means "no caps" —
    /// single-tenant local dev and `--no-auth` mode use this.
    quotas: Quotas,
    /// Serializes all writes within this tenant handle.
    write_lock: Mutex<()>,
}

impl Repo {
    /// Initialize a repo for single-tenant (local / `--no-auth`) use.
    /// Equivalent to `init_tenant(root, TenantId::local(), Quotas::unlimited())`.
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        Self::init_tenant(root, TenantId::local(), Quotas::unlimited())
    }

    /// Open an existing single-tenant repo. Equivalent to
    /// `open_tenant(root, TenantId::local(), Quotas::unlimited())`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_tenant(root, TenantId::local(), Quotas::unlimited())
    }

    /// Initialize a repo scoped to a specific tenant with the given quota
    /// ceiling. Creates `.omp/`, drops starter schemas + probes + `omp.toml` +
    /// `.omp/local.toml`.
    pub fn init_tenant(root: impl AsRef<Path>, tenant: TenantId, quotas: Quotas) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|e| OmpError::io(&root, e))?;
        let store = DiskStore::init(&root)?;
        LocalConfig::write_skeleton(store.root())?;

        let repo = Repo {
            root: root.clone(),
            store,
            tenant,
            quotas,
            write_lock: Mutex::new(()),
        };

        // Drop starter files only if the working tree hasn't already grown past
        // the skeleton (so `init` is safe to rerun).
        for probe in STARTER_PROBES {
            let p = root.join(probe.tree_path_wasm());
            if !p.exists() {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent).map_err(|e| OmpError::io(parent, e))?;
                }
                fs::write(&p, probe.wasm).map_err(|e| OmpError::io(&p, e))?;
            }
            let pt = root.join(probe.tree_path_manifest());
            if !pt.exists() {
                fs::write(&pt, probe.manifest_toml).map_err(|e| OmpError::io(&pt, e))?;
            }
        }
        for (path, bytes) in crate::probes::starter::starter_schemas() {
            let p = root.join(path);
            if !p.exists() {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent).map_err(|e| OmpError::io(parent, e))?;
                }
                fs::write(&p, bytes).map_err(|e| OmpError::io(&p, e))?;
            }
        }
        let omp_toml = root.join("omp.toml");
        if !omp_toml.exists() {
            fs::write(
                &omp_toml,
                b"# OpenMemoryProtocol versioned config.\n# See docs/design/07-config.md.\n\n[ingest]\ndefault_schema_policy = \"reject\"\nallow_blob_fallback = false\n\n[workdir]\nignore = [\".git/\", \"node_modules/\", \"*.log\", \"*.tmp\", \"__pycache__/\"]\nfollow_symlinks = false\n\n[probes]\nmemory_mb = 64\nfuel = 1000000000\nwall_clock_s = 10\n",
            )
            .map_err(|e| OmpError::io(&omp_toml, e))?;
        }

        Ok(repo)
    }

    /// Open an existing tenant-scoped repo.
    pub fn open_tenant(root: impl AsRef<Path>, tenant: TenantId, quotas: Quotas) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let store = DiskStore::open(&root)?;
        Ok(Repo {
            root,
            store,
            tenant,
            quotas,
            write_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> &DiskStore {
        &self.store
    }

    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    pub fn quotas(&self) -> &Quotas {
        &self.quotas
    }

    /// Snapshot on-disk usage for this tenant: `(object_count, total_bytes)`.
    /// Walks `.omp/objects/` and sums compressed file sizes.
    pub fn usage(&self) -> Result<(u64, u64)> {
        let objects = self.store.root().join("objects");
        if !objects.is_dir() {
            return Ok((0, 0));
        }
        let mut count: u64 = 0;
        let mut bytes: u64 = 0;
        for bucket in fs::read_dir(&objects).map_err(|e| OmpError::io(&objects, e))? {
            let bucket = bucket.map_err(|e| OmpError::io(&objects, e))?;
            if !bucket.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            for entry in fs::read_dir(bucket.path()).map_err(|e| OmpError::io(bucket.path(), e))? {
                let entry = entry.map_err(|e| OmpError::io(bucket.path(), e))?;
                let meta = entry
                    .metadata()
                    .map_err(|e| OmpError::io(entry.path(), e))?;
                if meta.is_file() {
                    count += 1;
                    bytes += meta.len();
                }
            }
        }
        Ok((count, bytes))
    }

    /// Check that a write of `incoming_bytes` (sum of object sizes to be
    /// added) won't push this tenant past its quota. Over-quota raises
    /// `OmpError::QuotaExceeded`; at-quota passes.
    fn check_quota_for_write(&self, incoming_bytes: u64, incoming_objects: u64) -> Result<()> {
        if self.quotas.bytes.is_none() && self.quotas.object_count.is_none() {
            return Ok(());
        }
        let (cur_count, cur_bytes) = self.usage()?;
        if let Some(cap) = self.quotas.bytes {
            if cur_bytes.saturating_add(incoming_bytes) > cap {
                return Err(OmpError::QuotaExceeded {
                    limit: format!("bytes: {} + {} > {}", cur_bytes, incoming_bytes, cap),
                });
            }
        }
        if let Some(cap) = self.quotas.object_count {
            if cur_count.saturating_add(incoming_objects) > cap {
                return Err(OmpError::QuotaExceeded {
                    limit: format!(
                        "object_count: {} + {} > {}",
                        cur_count, incoming_objects, cap
                    ),
                });
            }
        }
        Ok(())
    }

    /// Stage a file. Schemas + `omp.toml` + `probes/*` go in as blobs;
    /// everything else is ingested as a manifest.
    pub fn add(
        &self,
        path: &str,
        bytes: &[u8],
        user_fields: Option<Fields>,
        file_type_override: Option<&str>,
    ) -> Result<AddResult> {
        let _lock = self.write_lock.lock().unwrap();
        validate_path(path)?;
        // Soft pre-check: a write adds at least one blob and, for user files,
        // a manifest. We don't know the manifest size yet, so this is a lower
        // bound — tight quotas will still catch the overrun at commit time.
        self.check_quota_for_write(bytes.len() as u64, 1)?;

        let mode = walker::classify_path(Path::new(path));
        if mode == Mode::Blob {
            // Schemas get pre-flight validated; `omp.toml` gets parsed-validated.
            if let Some(stem) = schema_file_type_from_path(path) {
                let parsed = Schema::parse(bytes, stem)?;
                let probe_names = self.current_probe_names()?;
                parsed.validate_probe_refs(&probe_names)?;
            } else if path == "omp.toml" {
                RepoConfig::parse(bytes)?;
            }
            let blob_hash = self.store.put(ObjectType::Blob.as_str(), bytes)?;
            self.stage_upsert(path, Mode::Blob, blob_hash)?;
            return Ok(AddResult::Blob {
                path: path.to_string(),
                blob_hash,
                size: bytes.len(),
            });
        }

        // Manifest path. Decide chunked vs. single-blob.
        let repo_config = self.current_repo_config()?;
        let chunk_size = repo_config.storage.chunk_size_bytes;
        if (bytes.len() as u64) >= chunk_size {
            return self.add_chunked(path, bytes, user_fields, file_type_override, chunk_size);
        }

        let manifest = self.build_manifest(
            path,
            bytes,
            user_fields.unwrap_or_default(),
            file_type_override,
            None,
            clock_now(),
        )?;
        let blob_hash = self.store.put(ObjectType::Blob.as_str(), bytes)?;
        let manifest_bytes = manifest.serialize()?;
        let manifest_hash = self
            .store
            .put(ObjectType::Manifest.as_str(), &manifest_bytes)?;
        self.stage_upsert(path, Mode::Manifest, manifest_hash)?;
        Ok(AddResult::Manifest {
            path: path.to_string(),
            manifest_hash,
            source_hash: blob_hash,
        })
    }

    /// Chunked ingest for files >= `chunk_size_bytes`. Splits into fixed-size
    /// blobs, writes a `chunks` object pointing at them, and builds the
    /// manifest with `source_hash` = the chunks-object hash. See
    /// `docs/design/12-large-files.md §Chunked-file ingest`.
    fn add_chunked(
        &self,
        path: &str,
        bytes: &[u8],
        user_fields: Option<Fields>,
        file_type_override: Option<&str>,
        chunk_size: u64,
    ) -> Result<AddResult> {
        use crate::chunks::{ChunkEntry, ChunksBody};
        use sha2::{Digest, Sha256};

        let chunk_size_usize = usize::try_from(chunk_size).map_err(|_| {
            OmpError::internal(format!("chunk_size_bytes {chunk_size} exceeds usize"))
        })?;

        let mut entries: Vec<ChunkEntry> = Vec::new();
        let mut hasher = Sha256::new();
        for slice in bytes.chunks(chunk_size_usize) {
            hasher.update(slice);
            let chunk_hash = self.store.put(ObjectType::Blob.as_str(), slice)?;
            entries.push(ChunkEntry {
                hash: chunk_hash,
                length: slice.len() as u64,
            });
        }
        let sha256_hex = crate::hex::encode(&hasher.finalize());

        let chunks_body = ChunksBody::new(entries);
        let chunks_bytes = chunks_body.serialize();
        let chunks_hash = self.store.put(ObjectType::Chunks.as_str(), &chunks_bytes)?;

        let sniff_prefix = sniff_prefix_of(bytes);
        let manifest = self.build_manifest_with(
            path,
            &[],
            user_fields.unwrap_or_default(),
            file_type_override,
            None,
            clock_now(),
            ChunkedIngest::Chunked {
                source_hash: chunks_hash,
                file_size: bytes.len() as u64,
                sha256_hex,
                sniff_prefix,
            },
            None,
        )?;
        let manifest_bytes = manifest.serialize()?;
        let manifest_hash = self
            .store
            .put(ObjectType::Manifest.as_str(), &manifest_bytes)?;
        self.stage_upsert(path, Mode::Manifest, manifest_hash)?;
        Ok(AddResult::Manifest {
            path: path.to_string(),
            manifest_hash,
            source_hash: chunks_hash,
        })
    }

    // ---- Client-side encrypted ingest (docs/design/13-end-to-end-encryption.md) ----

    /// End-to-end-encrypted ingest path.
    ///
    /// The client (not the server) runs the engine — probes execute on the
    /// plaintext in-process before encryption. Each file gets a fresh
    /// CSPRNG content key; the ciphertext blob is what the server sees.
    /// The manifest is sealed under the tenant's `manifest_key` and the
    /// content key is wrapped under `data_key` inside the manifest
    /// envelope (see `encrypted_manifest::EncryptedManifestEnvelope`).
    ///
    /// For files at or above `chunk_size_bytes`, the plaintext is split
    /// into fixed-size chunks, each sealed under a per-chunk nonce
    /// derived from `nonce_for_chunk(content_key, index)` (doc 13
    /// §Interaction-with-large-files). The `chunks` object body stays
    /// plaintext — its entries are already hashes of ciphertext.
    pub fn add_encrypted(
        &self,
        path: &str,
        plaintext: &[u8],
        user_fields: Option<Fields>,
        file_type_override: Option<&str>,
        keys: &crate::keys::TenantKeys,
    ) -> Result<AddResult> {
        use omp_crypto::aead;

        let _lock = self.write_lock.lock().unwrap();
        validate_path(path)?;
        self.check_quota_for_write(plaintext.len() as u64, 2)?;

        let mode = walker::classify_path(Path::new(path));
        if mode == Mode::Blob {
            return Err(OmpError::InvalidPath(format!(
                "path {:?} is a config/schema blob path; encrypted ingest only accepts user files",
                path
            )));
        }

        let content_key = aead::random_key();

        let repo_config = self.current_repo_config_with_key(Some(&keys.path_key))?;
        let chunk_size = repo_config.storage.chunk_size_bytes;

        let (source_hash, file_size, sha256_hex, sniff_prefix) =
            if (plaintext.len() as u64) >= chunk_size {
                self.seal_chunked(plaintext, &content_key, chunk_size)?
            } else {
                self.seal_single_blob(plaintext, &content_key)?
            };

        let manifest = self.build_manifest_with(
            path,
            &[],
            user_fields.unwrap_or_default(),
            file_type_override,
            None,
            clock_now(),
            ChunkedIngest::Chunked {
                source_hash,
                file_size,
                sha256_hex,
                sniff_prefix,
            },
            Some(&keys.path_key),
        )?;

        let envelope_bytes = crate::encrypted_manifest::EncryptedManifestEnvelope::seal(
            &manifest,
            &content_key,
            &keys.manifest_key,
            &keys.data_key,
        )?;
        let manifest_hash = self
            .store
            .put(ObjectType::Manifest.as_str(), &envelope_bytes)?;
        self.stage_upsert(path, Mode::Manifest, manifest_hash)?;

        // Zeroize the stack copy of the content key; its wrapped form lives
        // inside the encrypted manifest, and `manifest_key` can recover it.
        use zeroize::Zeroize as _;
        let mut ck = content_key;
        ck.zeroize();

        Ok(AddResult::Manifest {
            path: path.to_string(),
            manifest_hash,
            source_hash,
        })
    }

    /// Seal a small-file plaintext as a single ciphertext blob.
    /// Returns (sealed_blob_hash, plaintext_size, sha256_hex, sniff_prefix).
    fn seal_single_blob(
        &self,
        plaintext: &[u8],
        content_key: &[u8; 32],
    ) -> Result<(Hash, u64, String, Vec<u8>)> {
        use omp_crypto::{aead, chunk_nonce};

        // Chunk index 0 so a later chunked re-ingest of the same file
        // under the same content key produces matching ciphertext for
        // its first chunk.
        let nonce = chunk_nonce::nonce_for_chunk(content_key, 0)
            .map_err(|e| OmpError::internal(format!("nonce_for_chunk: {e}")))?;
        let ciphertext = aead::seal(content_key, &nonce, b"omp-blob", plaintext)
            .map_err(|e| OmpError::internal(format!("seal blob: {e}")))?;
        let sealed_hash = self.store.put(ObjectType::Blob.as_str(), &ciphertext)?;

        let sha256 = crate::hex::sha256_hex(plaintext);
        Ok((
            sealed_hash,
            plaintext.len() as u64,
            sha256,
            sniff_prefix_of(plaintext),
        ))
    }

    /// Seal a large-file plaintext as N chunks. Each chunk is sealed with
    /// `nonce = nonce_for_chunk(content_key, index)`; the `chunks` object
    /// body stays plaintext (doc 13 §Interaction-with-large-files).
    fn seal_chunked(
        &self,
        plaintext: &[u8],
        content_key: &[u8; 32],
        chunk_size: u64,
    ) -> Result<(Hash, u64, String, Vec<u8>)> {
        use crate::chunks::{ChunkEntry, ChunksBody};
        use omp_crypto::{aead, chunk_nonce};
        use sha2::{Digest, Sha256};

        let chunk_size_usize = usize::try_from(chunk_size).map_err(|_| {
            OmpError::internal(format!("chunk_size_bytes {chunk_size} exceeds usize"))
        })?;

        // AAD binds each chunk to its position and the total count so
        // reordering or truncating the chunks-body makes AEAD open fail.
        let total_chunks: u32 = if plaintext.is_empty() {
            0
        } else {
            let n = plaintext.len().div_ceil(chunk_size_usize);
            u32::try_from(n).map_err(|_| {
                OmpError::internal(format!(
                    "chunk count {n} exceeds u32 — file too large for v1"
                ))
            })?
        };
        let total_be = total_chunks.to_be_bytes();

        let mut entries: Vec<ChunkEntry> = Vec::new();
        let mut plaintext_hasher = Sha256::new();
        for (index, slice) in plaintext.chunks(chunk_size_usize).enumerate() {
            let idx32: u32 = u32::try_from(index).map_err(|_| {
                OmpError::internal(format!(
                    "chunk index {index} exceeds u32 — file too large for v1"
                ))
            })?;
            plaintext_hasher.update(slice);
            let nonce = chunk_nonce::nonce_for_chunk(content_key, idx32)
                .map_err(|e| OmpError::internal(format!("nonce_for_chunk: {e}")))?;
            let mut aad = [0u8; 9 + 4 + 4];
            aad[..9].copy_from_slice(b"omp-chunk");
            aad[9..13].copy_from_slice(&idx32.to_be_bytes());
            aad[13..17].copy_from_slice(&total_be);
            let ciphertext = aead::seal(content_key, &nonce, &aad, slice)
                .map_err(|e| OmpError::internal(format!("seal chunk {index}: {e}")))?;
            let chunk_hash = self.store.put(ObjectType::Blob.as_str(), &ciphertext)?;
            entries.push(ChunkEntry {
                hash: chunk_hash,
                length: ciphertext.len() as u64,
            });
        }
        let sha256 = crate::hex::encode(&plaintext_hasher.finalize());

        let chunks_body = ChunksBody::new(entries);
        let chunks_bytes = chunks_body.serialize();
        let chunks_hash = self.store.put(ObjectType::Chunks.as_str(), &chunks_bytes)?;

        Ok((
            chunks_hash,
            plaintext.len() as u64,
            sha256,
            sniff_prefix_of(plaintext),
        ))
    }

    /// Read back an encrypted manifest + plaintext content. The caller
    /// must hold the tenant's `TenantKeys`.
    ///
    /// Resolves `path` through the tree at `at` (HEAD by default), fetches
    /// the manifest envelope from the object store, opens it under the
    /// tenant's `manifest_key` / `data_key`, then decrypts the referenced
    /// blob or chunks object using the recovered content key.
    pub fn show_encrypted(
        &self,
        path: &str,
        at: Option<&str>,
        keys: &crate::keys::TenantKeys,
    ) -> Result<(Manifest, Vec<u8>)> {
        use crate::encrypted_manifest::EncryptedManifestEnvelope;

        let tree_root = self.root_tree(at)?;
        let target = if path.is_empty() {
            tree_root.map(|r| (Mode::Tree, r))
        } else {
            match tree_root {
                Some(r) => paths::get_at_with_key(&self.store, path, &r, Some(&keys.path_key))?,
                None => None,
            }
        };
        let (mode, hash) = target.ok_or_else(|| OmpError::NotFound(format!("path {path:?}")))?;
        if mode != Mode::Manifest {
            return Err(OmpError::InvalidPath(format!(
                "path {path:?} is not a manifest"
            )));
        }
        let (_, envelope_bytes) = self
            .store
            .get(&hash)?
            .ok_or_else(|| OmpError::NotFound(format!("manifest {}", hash.hex())))?;

        let envelope = EncryptedManifestEnvelope::parse(&envelope_bytes)?;
        let (manifest, content_key) = envelope.open(&keys.manifest_key, &keys.data_key)?;

        let plaintext = self.decrypt_source(&manifest.source_hash, &content_key)?;
        Ok((manifest, plaintext))
    }

    fn decrypt_source(&self, source_hash: &Hash, content_key: &[u8; 32]) -> Result<Vec<u8>> {
        use crate::chunks::ChunksBody;
        use omp_crypto::aead;

        let (ty, body) = self
            .store
            .get(source_hash)?
            .ok_or_else(|| OmpError::NotFound(format!("source {}", source_hash.hex())))?;
        match ty.as_str() {
            "blob" => aead::open(content_key, b"omp-blob", &body).map_err(|_| {
                OmpError::Unauthorized(
                    "unable to open blob (wrong content key or tampered ciphertext)".into(),
                )
            }),
            "chunks" => {
                let parsed = ChunksBody::parse(&body)?;
                let total: u32 = u32::try_from(parsed.entries.len()).map_err(|_| {
                    OmpError::Corrupt("chunks body has more than u32::MAX entries".into())
                })?;
                let total_be = total.to_be_bytes();
                let mut out = Vec::with_capacity(parsed.total_length() as usize);
                for (idx, entry) in parsed.entries.iter().enumerate() {
                    let idx32 = u32::try_from(idx).map_err(|_| {
                        OmpError::Corrupt("chunks body has more than u32::MAX entries".into())
                    })?;
                    let (chunk_ty, ct) = self
                        .store
                        .get(&entry.hash)?
                        .ok_or_else(|| OmpError::NotFound(format!("chunk {}", entry.hash.hex())))?;
                    if chunk_ty != "blob" {
                        return Err(OmpError::Corrupt(format!(
                            "chunk {} is not a blob",
                            entry.hash.hex()
                        )));
                    }
                    let mut aad = [0u8; 9 + 4 + 4];
                    aad[..9].copy_from_slice(b"omp-chunk");
                    aad[9..13].copy_from_slice(&idx32.to_be_bytes());
                    aad[13..17].copy_from_slice(&total_be);
                    let pt = aead::open(content_key, &aad, &ct).map_err(|_| {
                        OmpError::Unauthorized(format!("unable to open chunk {}", entry.hash.hex()))
                    })?;
                    out.extend_from_slice(&pt);
                }
                Ok(out)
            }
            other => Err(OmpError::Corrupt(format!(
                "source {} has unexpected type {other:?}",
                source_hash.hex()
            ))),
        }
    }

    // ---- Sharing: docs/design/13-end-to-end-encryption.md §Sharing ----

    /// Wrap a file's content key to one or more recipient X25519 public
    /// keys and emit a `share` object. Alice's keys are needed to unwrap
    /// the content key from the encrypted manifest referenced by `path`.
    pub fn create_share(
        &self,
        path: &str,
        at: Option<&str>,
        keys: &crate::keys::TenantKeys,
        recipients: &[(TenantId, [u8; 32])],
    ) -> Result<Hash> {
        use crate::encrypted_manifest::EncryptedManifestEnvelope;
        use crate::share::{hex_encode, Recipient, ShareBody, SHARE_ALG};
        use omp_crypto::identity;

        let _lock = self.write_lock.lock().unwrap();
        validate_path(path)?;

        // Resolve to the manifest hash, then pull its envelope to get the
        // content key.
        let tree_root = self
            .root_tree(at)?
            .ok_or_else(|| OmpError::NotFound("no HEAD".into()))?;
        let (mode, manifest_hash) =
            paths::get_at_with_key(&self.store, path, &tree_root, Some(&keys.path_key))?
                .ok_or_else(|| OmpError::NotFound(format!("path {path:?}")))?;
        if mode != Mode::Manifest {
            return Err(OmpError::InvalidPath(format!(
                "path {path:?} is not a manifest"
            )));
        }
        let (_, env_bytes) = self
            .store
            .get(&manifest_hash)?
            .ok_or_else(|| OmpError::NotFound(format!("manifest {}", manifest_hash.hex())))?;
        let envelope = EncryptedManifestEnvelope::parse(&env_bytes)?;
        let (manifest, content_key) = envelope.open(&keys.manifest_key, &keys.data_key)?;

        let mut recipient_entries = Vec::with_capacity(recipients.len());
        for (tenant, pk) in recipients {
            let wrapped = identity::wrap_to_recipient(&content_key, pk)
                .map_err(|e| OmpError::internal(format!("wrap: {e}")))?;
            recipient_entries.push(Recipient {
                tenant: tenant.clone(),
                wrapped_key: hex_encode(&wrapped),
                alg: SHARE_ALG.into(),
            });
        }

        let body = ShareBody {
            for_hash: manifest.source_hash,
            granted_by: self.tenant.clone(),
            granted_at: clock_now(),
            recipients: recipient_entries,
        };
        let body_bytes = body.serialize()?;
        let share_hash = self.store.put(ObjectType::Share.as_str(), &body_bytes)?;
        Ok(share_hash)
    }

    /// Revoke one or more recipients from an existing share by rewriting
    /// the underlying ciphertext. See doc 13 §Sharing (Revocation).
    ///
    /// Steps:
    ///   1. Open the encrypted manifest at `path` to get the current
    ///      content_key and Manifest.
    ///   2. Decrypt the source blob or chunks-object stream to plaintext.
    ///   3. Generate a fresh content key; re-seal the plaintext under it.
    ///   4. Rewrite the manifest envelope with the new content key and
    ///      updated `source_hash`.
    ///   5. Emit a new `share` object with the filtered recipient list.
    ///
    /// Old objects remain on disk until `omp admin gc` reclaims them.
    /// Returns the new `share` object hash.
    pub fn revoke_share(
        &self,
        path: &str,
        keys: &crate::keys::TenantKeys,
        keep_recipients: &[(TenantId, [u8; 32])],
    ) -> Result<Hash> {
        let _lock = self.write_lock.lock().unwrap();
        validate_path(path)?;

        // Read current plaintext + old manifest (so we can preserve user
        // fields, file_type, etc. on re-seal).
        let (old_manifest, plaintext) = {
            drop(_lock);
            self.show_encrypted(path, None, keys)?
        };
        let _lock = self.write_lock.lock().unwrap();

        use omp_crypto::aead;
        let new_content_key = aead::random_key();
        let repo_config = self.current_repo_config_with_key(Some(&keys.path_key))?;
        let chunk_size = repo_config.storage.chunk_size_bytes;
        let (new_source_hash, file_size, sha256_hex, sniff_prefix) =
            if (plaintext.len() as u64) >= chunk_size {
                self.seal_chunked(&plaintext, &new_content_key, chunk_size)?
            } else {
                self.seal_single_blob(&plaintext, &new_content_key)?
            };

        let user_fields: Fields = old_manifest.fields.clone();
        let manifest = self.build_manifest_with(
            path,
            &[],
            user_fields,
            Some(&old_manifest.file_type),
            None,
            clock_now(),
            ChunkedIngest::Chunked {
                source_hash: new_source_hash,
                file_size,
                sha256_hex,
                sniff_prefix,
            },
            Some(&keys.path_key),
        )?;

        let envelope_bytes = crate::encrypted_manifest::EncryptedManifestEnvelope::seal(
            &manifest,
            &new_content_key,
            &keys.manifest_key,
            &keys.data_key,
        )?;
        let new_manifest_hash = self
            .store
            .put(ObjectType::Manifest.as_str(), &envelope_bytes)?;
        self.stage_upsert(path, Mode::Manifest, new_manifest_hash)?;

        use crate::share::{hex_encode, Recipient, ShareBody, SHARE_ALG};
        use omp_crypto::identity;
        let mut recipients = Vec::with_capacity(keep_recipients.len());
        for (tenant, pk) in keep_recipients {
            let wrapped = identity::wrap_to_recipient(&new_content_key, pk)
                .map_err(|e| OmpError::internal(format!("wrap: {e}")))?;
            recipients.push(Recipient {
                tenant: tenant.clone(),
                wrapped_key: hex_encode(&wrapped),
                alg: SHARE_ALG.into(),
            });
        }
        let share = ShareBody {
            for_hash: new_source_hash,
            granted_by: self.tenant.clone(),
            granted_at: clock_now(),
            recipients,
        };
        let share_bytes = share.serialize()?;
        let share_hash = self.store.put(ObjectType::Share.as_str(), &share_bytes)?;
        Ok(share_hash)
    }

    /// Bob's side of a share: given a `share` object's hash, find Bob's
    /// recipient entry, unwrap the content key, and return it plus the
    /// `for_hash` (the blob/chunks object the content key decrypts).
    pub fn apply_share(
        &self,
        share_hash: &Hash,
        as_tenant: &TenantId,
        identity_priv: &omp_crypto::identity::IdentityPrivate,
    ) -> Result<(Hash, [u8; 32])> {
        use crate::share::{hex_decode, ShareBody};
        use omp_crypto::identity;

        let (ty, body) = self
            .store
            .get(share_hash)?
            .ok_or_else(|| OmpError::NotFound(format!("share {}", share_hash.hex())))?;
        if ty != "share" {
            return Err(OmpError::Corrupt(format!(
                "object {} is not a share",
                share_hash.hex()
            )));
        }
        let parsed = ShareBody::parse(&body)?;
        let recipient = parsed.recipient_for(as_tenant).ok_or_else(|| {
            OmpError::Unauthorized(format!(
                "tenant {as_tenant} is not a recipient of share {}",
                share_hash.hex()
            ))
        })?;
        let wrapped = hex_decode(&recipient.wrapped_key)?;
        let content_key = identity::unwrap_from_stanza(&wrapped, identity_priv).map_err(|_| {
            OmpError::Unauthorized("unable to unwrap content key from share".into())
        })?;
        Ok((parsed.for_hash, content_key))
    }

    /// Rebuild a user file's manifest under updated user fields. The blob
    /// stays the same; probe fields re-run.
    pub fn patch_fields(&self, path: &str, updates: Fields) -> Result<Manifest> {
        let _lock = self.write_lock.lock().unwrap();
        validate_path(path)?;

        let (mode, hash) = self.resolve_staged_or_head(path)?;
        if mode != Mode::Manifest {
            return Err(OmpError::InvalidPath(format!(
                "path {:?} is not a user file",
                path
            )));
        }
        let (_, content) = self
            .store
            .get(&hash)?
            .ok_or_else(|| OmpError::NotFound(format!("manifest {}", hash.hex())))?;
        let prev = Manifest::parse(&content)?;
        let (_, blob_content) = self
            .store
            .get(&prev.source_hash)?
            .ok_or_else(|| OmpError::NotFound(format!("blob {}", prev.source_hash.hex())))?;

        // Merge existing user fields with updates: prev's user_provided fields
        // are whatever is in its [fields] table; updates overwrite.
        let mut user_fields: Fields = prev.fields.clone();
        for (k, v) in updates {
            user_fields.insert(k, v);
        }

        let manifest = self.build_manifest(
            path,
            &blob_content,
            user_fields,
            Some(&prev.file_type),
            None,
            clock_now(),
        )?;
        let manifest_bytes = manifest.serialize()?;
        let manifest_hash = self
            .store
            .put(ObjectType::Manifest.as_str(), &manifest_bytes)?;
        self.stage_upsert(path, Mode::Manifest, manifest_hash)?;
        Ok(manifest)
    }

    /// Stage a deletion.
    pub fn remove(&self, path: &str) -> Result<()> {
        let _lock = self.write_lock.lock().unwrap();
        validate_path(path)?;
        let mut idx = self.load_index()?;
        idx.entries.insert(
            path.to_string(),
            StagedChange {
                path: path.to_string(),
                kind: StagedKind::Delete,
                hash: None,
            },
        );
        self.save_index(&idx)
    }

    /// Read a path from the staging index instead of the committed tree.
    /// Used by the UI's "view staged file" path so a freshly-installed
    /// marketplace probe (or an in-progress upload) is browsable before
    /// the user runs `commit`. Returns `NotFound` when nothing is staged
    /// for that path; the caller should fall back to `show()` for
    /// committed content. Hits only the local index file + object store
    /// — no tree walk.
    pub fn show_staged(&self, path: &str) -> Result<ShowResult> {
        let idx = self.load_index()?;
        let entry = idx
            .entries
            .get(path)
            .ok_or_else(|| OmpError::NotFound(format!("path {path:?} not staged")))?;
        if entry.kind == StagedKind::Delete {
            return Err(OmpError::NotFound(format!(
                "path {path:?} is staged for deletion"
            )));
        }
        let hash = entry
            .hash
            .clone()
            .ok_or_else(|| OmpError::NotFound(format!("path {path:?} has no staged hash")))?;
        let (obj_type, content) = self
            .store
            .get(&hash)?
            .ok_or_else(|| OmpError::Corrupt(format!("staged blob {} missing", hash.hex())))?;
        match obj_type.as_str() {
            "manifest" => {
                let manifest = Manifest::parse(&content)?;
                let render = self.render_for_file_type(&manifest.file_type, None);
                Ok(ShowResult::Manifest {
                    path: path.to_string(),
                    manifest,
                    render,
                })
            }
            "blob" => Ok(ShowResult::Blob {
                path: path.to_string(),
                blob_hash: hash,
                size: content.len(),
                render: render_for_blob_path(path),
            }),
            // The index never contains tree or commit objects; if it
            // somehow does, refuse rather than guess.
            other => Err(OmpError::Corrupt(format!(
                "staged path {path:?} points at a non-blob/manifest ({other})"
            ))),
        }
    }

    /// Like `bytes_of` but reads from the staged index.
    pub fn bytes_of_staged(&self, path: &str) -> Result<Vec<u8>> {
        let idx = self.load_index()?;
        let entry = idx
            .entries
            .get(path)
            .ok_or_else(|| OmpError::NotFound(format!("path {path:?} not staged")))?;
        if entry.kind == StagedKind::Delete {
            return Err(OmpError::NotFound(format!(
                "path {path:?} is staged for deletion"
            )));
        }
        let hash = entry
            .hash
            .clone()
            .ok_or_else(|| OmpError::NotFound(format!("path {path:?} has no staged hash")))?;
        let (obj_type, content) = self
            .store
            .get(&hash)?
            .ok_or_else(|| OmpError::Corrupt(format!("staged blob {} missing", hash.hex())))?;
        match obj_type.as_str() {
            "blob" => Ok(content),
            "manifest" => {
                // Bytes-of follows the manifest's source_hash to the raw
                // blob, mirroring the committed-tree path.
                let manifest = Manifest::parse(&content)?;
                let (_, raw) = self
                    .store
                    .get(&manifest.source_hash)?
                    .ok_or_else(|| OmpError::Corrupt("source_hash missing".to_string()))?;
                Ok(raw)
            }
            _ => Err(OmpError::NotFound(format!(
                "staged path {path:?} is not a file"
            ))),
        }
    }

    pub fn show(&self, path: &str, at: Option<&str>) -> Result<ShowResult> {
        let tree_root = self.root_tree(at)?;
        let target: Option<(Mode, Hash)> = if path.is_empty() {
            tree_root.map(|r| (Mode::Tree, r))
        } else {
            match tree_root {
                Some(r) => paths::get_at(&self.store, path, &r)?,
                None => None,
            }
        };
        let (mode, hash) = target.ok_or_else(|| OmpError::NotFound(format!("path {path:?}")))?;
        match mode {
            Mode::Manifest => {
                let (_, content) = self
                    .store
                    .get(&hash)?
                    .ok_or_else(|| OmpError::NotFound(format!("manifest {}", hash.hex())))?;
                let manifest = Manifest::parse(&content)?;
                let render = self.render_for_file_type(&manifest.file_type, tree_root.as_ref());
                Ok(ShowResult::Manifest {
                    path: path.to_string(),
                    manifest,
                    render,
                })
            }
            Mode::Blob => {
                let (_, content) = self
                    .store
                    .get(&hash)?
                    .ok_or_else(|| OmpError::NotFound(format!("blob {}", hash.hex())))?;
                Ok(ShowResult::Blob {
                    path: path.to_string(),
                    blob_hash: hash,
                    size: content.len(),
                    render: render_for_blob_path(path),
                })
            }
            Mode::Tree => Ok(ShowResult::Tree {
                path: path.to_string(),
                entries: self.list_tree(hash)?,
            }),
        }
    }

    /// Best-effort lookup of the schema's render hint **at the same tree as
    /// the manifest being shown** (so time-traveled requests pick up the
    /// schema that was committed at that point). On any failure path
    /// (schema missing, fails to parse, no tree at all), fall back to
    /// `Binary` so a bad schema never errors a `GET /files`.
    fn render_for_file_type(&self, file_type: &str, tree_root: Option<&Hash>) -> RenderHint {
        let fallback = RenderHint {
            kind: RenderKind::Binary,
            max_inline_bytes: None,
        };
        let Some(root) = tree_root else {
            return fallback;
        };
        let path = format!("schemas/{file_type}/schema.toml");
        let lookup = match paths::get_at(&self.store, &path, root) {
            Ok(v) => v,
            Err(_) => return fallback,
        };
        let Some((Mode::Blob, h)) = lookup else {
            return fallback;
        };
        let content = match self.store.get(&h) {
            Ok(Some((_, c))) => c,
            _ => return fallback,
        };
        match Schema::parse(&content, file_type) {
            Ok(s) => s.effective_render(),
            Err(_) => fallback,
        }
    }

    pub fn bytes_of(&self, path: &str, at: Option<&str>) -> Result<Vec<u8>> {
        let tree_root = self.root_tree(at)?;
        let target = match tree_root {
            Some(r) => paths::get_at(&self.store, path, &r)?,
            None => None,
        };
        let (mode, hash) = target.ok_or_else(|| OmpError::NotFound(format!("path {path:?}")))?;
        match mode {
            Mode::Manifest => {
                let (_, content) = self
                    .store
                    .get(&hash)?
                    .ok_or_else(|| OmpError::NotFound(format!("manifest {}", hash.hex())))?;
                let manifest = Manifest::parse(&content)?;
                let (_, blob) = self.store.get(&manifest.source_hash)?.ok_or_else(|| {
                    OmpError::NotFound(format!("blob {}", manifest.source_hash.hex()))
                })?;
                Ok(blob)
            }
            Mode::Blob => {
                let (_, content) = self
                    .store
                    .get(&hash)?
                    .ok_or_else(|| OmpError::NotFound(format!("blob {}", hash.hex())))?;
                Ok(content)
            }
            Mode::Tree => Err(OmpError::InvalidPath(format!(
                "path {path:?} is a directory"
            ))),
        }
    }

    /// Flat list of every path currently in the staging index. Used by the
    /// frontend's file sidebar before the first commit so a freshly-installed
    /// or uploaded probe is visible without requiring a commit first. Each
    /// entry's `mode` is resolved by inspecting the object header — it'll
    /// be `blob` or `manifest` depending on what was staged.
    pub fn ls_staged(&self) -> Result<Vec<TreeEntryOut>> {
        let idx = self.load_index()?;
        let mut out = Vec::with_capacity(idx.entries.len());
        for (path, change) in idx.entries.iter() {
            if change.kind == StagedKind::Delete {
                continue;
            }
            let Some(hash) = change.hash.as_ref() else {
                continue;
            };
            let (obj_type, _) = match self.store.get(hash)? {
                Some(o) => o,
                None => continue,
            };
            let mode = match obj_type.as_str() {
                "blob" | "manifest" | "tree" => obj_type,
                _ => "blob".to_string(),
            };
            out.push(TreeEntryOut {
                name: path.clone(),
                mode,
                hash: hash.clone(),
            });
        }
        // Lexicographic so the sidebar's grouping logic produces a stable
        // tree.
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn ls(&self, path: &str, at: Option<&str>, recursive: bool) -> Result<Vec<TreeEntryOut>> {
        let tree_root = self
            .root_tree(at)?
            .ok_or_else(|| OmpError::NotFound("no commits".into()))?;
        let subtree = if path.is_empty() {
            tree_root
        } else {
            match paths::get_at(&self.store, path, &tree_root)? {
                Some((Mode::Tree, h)) => h,
                Some(_) => {
                    return Err(OmpError::InvalidPath(format!(
                        "path {path:?} is not a directory"
                    )))
                }
                None => return Err(OmpError::NotFound(format!("path {path:?}"))),
            }
        };
        if !recursive {
            return self.list_tree(subtree);
        }
        let entries = paths::walk(&self.store, &subtree)?;
        Ok(entries
            .into_iter()
            .map(|(p, m, h)| TreeEntryOut {
                name: p,
                mode: m.as_str().to_string(),
                hash: h,
            })
            .collect())
    }

    /// Flat list of every user-file (manifest-mode) path.
    pub fn files(&self, at: Option<&str>, prefix: Option<&str>) -> Result<Vec<FileListing>> {
        let tree_root = match self.root_tree(at)? {
            Some(r) => r,
            None => return Ok(Vec::new()),
        };
        let entries = paths::walk(&self.store, &tree_root)?;
        let mut out = Vec::new();
        for (path, mode, hash) in entries {
            if mode != Mode::Manifest {
                continue;
            }
            if let Some(pfx) = prefix {
                if !path.starts_with(pfx) {
                    continue;
                }
            }
            let (_, content) = self
                .store
                .get(&hash)?
                .ok_or_else(|| OmpError::NotFound(format!("manifest {}", hash.hex())))?;
            let manifest = Manifest::parse(&content)?;
            out.push(FileListing {
                path,
                manifest_hash: hash,
                source_hash: manifest.source_hash,
                file_type: manifest.file_type,
            });
        }
        Ok(out)
    }

    pub fn commit(&self, message: &str, author: Option<AuthorOverride>) -> Result<Hash> {
        self.commit_with_keys(message, author, None).map(|(h, _)| h)
    }

    /// Same as `commit` but returns the optional list of reprobe summaries
    /// alongside the new commit hash. Empty `Vec` when the commit didn't
    /// touch any schemas.
    pub fn commit_with_summary(
        &self,
        message: &str,
        author: Option<AuthorOverride>,
    ) -> Result<(Hash, Vec<ReprobeSummary>)> {
        self.commit_with_keys(message, author, None)
    }

    /// Encrypted variant: tree entry names are sealed under `keys.path_key`
    /// and the commit message body is sealed under `keys.commit_key`.
    /// Headers (tree hash, parent hashes, author) stay plaintext so the
    /// commit DAG remains walkable for GC. See doc 13 §What is encrypted.
    pub fn commit_encrypted(
        &self,
        message: &str,
        author: Option<AuthorOverride>,
        keys: &crate::keys::TenantKeys,
    ) -> Result<Hash> {
        self.commit_with_keys(message, author, Some(keys))
            .map(|(h, _)| h)
    }

    fn commit_with_keys(
        &self,
        message: &str,
        author: Option<AuthorOverride>,
        keys: Option<&crate::keys::TenantKeys>,
    ) -> Result<(Hash, Vec<ReprobeSummary>)> {
        let _lock = self.write_lock.lock().unwrap();
        let mut idx = self.load_index()?;
        if idx.entries.is_empty() {
            return Err(OmpError::Conflict("no staged changes".into()));
        }
        let _refs_lock = self.store.lock_refs()?;

        let local = LocalConfig::load(self.store.root())?;
        let override_ = author.unwrap_or_default();
        let author_obj = Author {
            name: override_.name.unwrap_or(local.author_name.clone()),
            email: override_.email.unwrap_or(local.author_email.clone()),
            timestamp: override_.timestamp.unwrap_or_else(clock_now),
        };

        let path_key: Option<&[u8; 32]> = keys.map(|k| &k.path_key);
        let commit_key: Option<&[u8; 32]> = keys.map(|k| &k.commit_key);

        // Auto-reprobe: detect staged schemas whose content differs from
        // HEAD, walk every existing manifest of that file_type, and
        // rebuild it against the new schema. New manifests are added to
        // `idx` in-place so the tree-build loop below picks them up. The
        // schema change AND the rebuilt manifests land in a single
        // commit. See `docs/design/21-schema-reprobe.md`.
        //
        // Encrypted tenants are skipped — server can't read plaintext;
        // they orchestrate reprobe client-side.
        let reprobe_summaries: Vec<ReprobeSummary> =
            if keys.is_none() && std::env::var("OMP_DEFER_REPROBE").is_err() {
                self.run_auto_reprobe(&mut idx)?
            } else {
                Vec::new()
            };

        // Start from HEAD's tree (or empty).
        let mut root: Option<Hash> = self.head_tree()?;

        for change in idx.entries.values() {
            match &change.kind {
                StagedKind::Upsert => {
                    let hash = change
                        .hash
                        .ok_or_else(|| OmpError::internal("upsert without hash"))?;
                    let entry = Entry {
                        mode: self.stored_mode(&hash)?,
                        hash,
                    };
                    let new_root = paths::put_at_with_key(
                        &self.store,
                        root.as_ref(),
                        &change.path,
                        entry,
                        path_key,
                    )?;
                    root = Some(new_root);
                }
                StagedKind::Delete => {
                    if let Some(r) = root {
                        root = paths::delete_at_with_key(&self.store, &r, &change.path, path_key)?;
                    }
                }
            }
        }

        let root_hash = root.ok_or_else(|| OmpError::Conflict("commit would be empty".into()))?;

        let parents = self.head_commit()?.into_iter().collect();
        let commit = Commit {
            tree: root_hash,
            parents,
            author: author_obj,
            message: message.to_string(),
        };
        let commit_bytes = commit.serialize_with_commit_key(commit_key)?;
        let commit_hash = self.store.put(ObjectType::Commit.as_str(), &commit_bytes)?;

        // Advance HEAD's branch (create if needed).
        let head = refs::parse_head(&self.store.read_head()?)?;
        match head {
            refs::Head::Branch(name) => self.store.write_ref(&name, &commit_hash)?,
            refs::Head::Detached(_) => self.store.write_head(&commit_hash.hex())?,
        }

        // Clear the index.
        self.save_index(&Index::default())?;

        Ok((commit_hash, reprobe_summaries))
    }

    /// Detect schema changes in the staged index and rebuild every existing
    /// manifest of each affected file_type. Mutates `idx` in-place by
    /// inserting new `Mode::Manifest` Upsert entries. Returns one summary
    /// per affected file_type. See `docs/design/21-schema-reprobe.md`.
    fn run_auto_reprobe(&self, idx: &mut Index) -> Result<Vec<ReprobeSummary>> {
        // Find staged schema blobs.
        let mut staged_schemas: Vec<(String, Hash)> = Vec::new();
        for change in idx.entries.values() {
            if !matches!(change.kind, StagedKind::Upsert) {
                continue;
            }
            let Some(stem) = schema_file_type_from_path(&change.path) else {
                continue;
            };
            let Some(hash) = change.hash else { continue };
            staged_schemas.push((stem.to_string(), hash));
        }
        if staged_schemas.is_empty() {
            return Ok(Vec::new());
        }

        // Load existing schemas at HEAD (un-keyed — encrypted tenants
        // already opted out of this code path before we got here).
        let head_schemas_by_type = self.load_all_schemas_with_key(None)?;

        // Pre-build the probe registry once for the whole pass.
        let probes = self.current_probes()?;

        // Pre-walk HEAD's tree once and collect (path, manifest_hash) for
        // every Mode::Manifest entry. Reused across schemas (rare to have
        // more than one in a single commit, but cheap and correct).
        let head_root = self.root_tree(None)?;
        let manifest_paths: Vec<(String, Hash)> = match head_root {
            Some(root) => paths::walk(&self.store, &root)?
                .into_iter()
                .filter_map(|(p, m, h)| (m == Mode::Manifest).then_some((p, h)))
                .collect(),
            None => Vec::new(),
        };

        let repo_config = self.current_repo_config()?;
        let mut summaries: Vec<ReprobeSummary> = Vec::new();
        let mut cache: ProbeOutputCache = ProbeOutputCache::new();

        for (file_type, new_schema_hash) in staged_schemas {
            // Parse the staged schema.
            let (_, new_schema_bytes) = self.store.get(&new_schema_hash)?.ok_or_else(|| {
                OmpError::internal(format!(
                    "staged schema blob {} missing from store",
                    new_schema_hash.hex()
                ))
            })?;
            let new_schema = Schema::parse(&new_schema_bytes, &file_type)?;

            // Skip if the schema is unchanged from HEAD.
            let unchanged = head_schemas_by_type
                .get(&file_type)
                .map(|old| schemas_equal(&new_schema, old))
                .unwrap_or(false);
            if unchanged {
                continue;
            }

            let old_schema = head_schemas_by_type.get(&file_type);

            let mut count: usize = 0;
            let mut skipped: Vec<ReprobeSkip> = Vec::new();

            for (path, manifest_hash) in &manifest_paths {
                // Read the old manifest.
                let Some((_, content)) = self.store.get(manifest_hash)? else {
                    skipped.push(ReprobeSkip {
                        path: path.clone(),
                        reason: format!("manifest blob {} missing", manifest_hash.hex()),
                    });
                    continue;
                };
                let old_manifest = match Manifest::parse(&content) {
                    Ok(m) => m,
                    Err(e) => {
                        skipped.push(ReprobeSkip {
                            path: path.clone(),
                            reason: format!("manifest parse: {e}"),
                        });
                        continue;
                    }
                };
                if old_manifest.file_type != file_type {
                    continue;
                }
                // Already at the new schema (e.g. a file ingested in this
                // commit after the schema was staged).
                if old_manifest.schema_hash == new_schema_hash {
                    continue;
                }

                match self.reprobe_one(
                    path,
                    &old_manifest,
                    &new_schema,
                    new_schema_hash,
                    &new_schema_bytes,
                    old_schema,
                    &probes,
                    &repo_config,
                    &mut cache,
                ) {
                    Ok(new_manifest_hash) => {
                        idx.entries.insert(
                            path.clone(),
                            StagedChange {
                                path: path.clone(),
                                kind: StagedKind::Upsert,
                                hash: Some(new_manifest_hash),
                            },
                        );
                        count += 1;
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        let truncated = if reason.len() > 200 {
                            format!("{}…", &reason[..200])
                        } else {
                            reason
                        };
                        skipped.push(ReprobeSkip {
                            path: path.clone(),
                            reason: truncated,
                        });
                    }
                }
            }

            summaries.push(ReprobeSummary {
                file_type,
                count,
                skipped,
            });
        }

        Ok(summaries)
    }

    /// Build a new manifest for `path` against `new_schema`, reusing field
    /// values from `old_manifest` whenever the field's `Source` is
    /// unchanged. Stores the new manifest blob and returns its hash.
    /// Caller stages the upsert.
    #[allow(clippy::too_many_arguments)]
    fn reprobe_one(
        &self,
        path: &str,
        old_manifest: &Manifest,
        new_schema: &Schema,
        new_schema_hash: Hash,
        new_schema_blob: &[u8],
        old_schema: Option<&Schema>,
        probes: &HashMap<String, ProbeBlob<'static>>,
        repo_config: &RepoConfig,
        cache: &mut ProbeOutputCache,
    ) -> Result<Hash> {
        // Read the source bytes. v1 plaintext only — encrypted tenants are
        // already excluded.
        let (src_ty, source_bytes) =
            self.store.get(&old_manifest.source_hash)?.ok_or_else(|| {
                OmpError::NotFound(format!(
                    "source blob {} for {path} missing",
                    old_manifest.source_hash.hex()
                ))
            })?;
        if src_ty != "blob" {
            return Err(OmpError::IngestValidation(format!(
                "reprobe of chunked source {src_ty:?} not yet supported (path {path})"
            )));
        }

        // Carry user-provided field values forward by name. Strip out
        // anything probe-driven so the engine's user_fields handling
        // doesn't re-inject probe outputs.
        let user_fields: BTreeMap<String, FieldValue> = old_manifest
            .fields
            .iter()
            .filter(|(name, _)| {
                old_schema
                    .and_then(|s| s.fields.iter().find(|f| &f.name == *name))
                    .map(|f| matches!(f.source, crate::schema::Source::UserProvided))
                    .unwrap_or(false)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Field-level reuse: for each field whose Source is unchanged
        // between old and new schemas AND whose probe (if any) still
        // resolves to the same framed_hash, copy the value verbatim.
        let mut reused_fields: BTreeMap<String, FieldValue> = BTreeMap::new();
        let mut reused_probe_hashes: BTreeMap<String, Hash> = BTreeMap::new();
        if let Some(old) = old_schema {
            for new_field in &new_schema.fields {
                let Some(old_field) = old.fields.iter().find(|f| f.name == new_field.name) else {
                    continue;
                };
                if old_field.source != new_field.source {
                    continue;
                }
                let Some(value) = old_manifest.fields.get(&new_field.name) else {
                    continue;
                };
                if let crate::schema::Source::Probe { probe, .. } = &new_field.source {
                    let Some(loaded) = probes.get(probe) else {
                        continue;
                    };
                    let Some(old_hash) = old_manifest.probe_hashes.get(probe) else {
                        continue;
                    };
                    if *old_hash != loaded.framed_hash {
                        continue;
                    }
                    reused_probe_hashes.insert(probe.clone(), loaded.framed_hash);
                }
                reused_fields.insert(new_field.name.clone(), value.clone());
            }
        }

        // Build a synthetic IngestInput so engine::ingest_with_cache can
        // re-derive only the fields that DIDN'T match. We achieve that by
        // running ingest with a "skip-already-resolved" hook... since the
        // engine doesn't have such a hook, we instead always run the full
        // ingest and the cache + identical (source_hash, probe_hash, args)
        // makes redundant probe runs free for cached fields. The reuse
        // optimization above exists primarily for fields whose probe_hash
        // changed — the engine would re-derive them anyway, so we don't
        // materially save engine work for those. The optimization is
        // load-bearing for the cache hit-rate on UNCHANGED fields:
        // populating `cache` with reused values up-front makes the engine's
        // own probe-run path return them via cache lookup, never invoking
        // wasmtime.
        for new_field in &new_schema.fields {
            if !reused_fields.contains_key(&new_field.name) {
                continue;
            }
            if let crate::schema::Source::Probe { probe, args } = &new_field.source {
                let Some(loaded) = probes.get(probe) else {
                    continue;
                };
                let mut effective = args.clone();
                effective
                    .entry("path".to_string())
                    .or_insert_with(|| FieldValue::String(path.to_string()));
                let args_canonical = serde_json::to_string(&effective).unwrap_or_default();
                cache.insert(
                    (old_manifest.source_hash, loaded.framed_hash, args_canonical),
                    reused_fields[&new_field.name].clone(),
                );
            }
        }
        let _ = reused_probe_hashes; // captured into cache via the loop above

        let ingested_at = clock_now();
        let input = IngestInput {
            bytes: &source_bytes,
            user_fields,
            path,
            ingested_at: &ingested_at,
            streaming_builtins: None,
            content_length: source_bytes.len() as u64,
            override_source_hash: Some(old_manifest.source_hash),
        };
        let view = TreeView {
            schema: new_schema,
            schema_blob: new_schema_blob,
            probes,
            limits: self.quotas.clamp_probe(repo_config.probes),
        };
        let manifest = engine::ingest_with_cache(&input, &view, cache)?;
        // Sanity: the engine should have stamped the schema_hash from the
        // bytes we passed in.
        debug_assert_eq!(manifest.schema_hash, new_schema_hash);

        let bytes = manifest.serialize()?;
        let new_hash = self
            .store
            .put(crate::object::ObjectType::Manifest.as_str(), &bytes)?;
        Ok(new_hash)
    }

    pub fn log_commits(&self, path: Option<&str>, max: usize) -> Result<Vec<CommitView>> {
        let _ = path; // v1: no path filtering.
        let head = match refs::resolve_head(&self.store)? {
            Some(h) => h,
            None => return Ok(Vec::new()),
        };
        let commits = refs::log(&self.store, head, max)?;
        Ok(commits
            .into_iter()
            .map(|(h, c)| CommitView {
                hash: h,
                tree: c.tree,
                parents: c.parents.clone(),
                author: c.author.name.clone(),
                email: c.author.email.clone(),
                timestamp: c.author.timestamp.clone(),
                message: c.message,
            })
            .collect())
    }

    pub fn diff(&self, from: &str, to: &str, path_filter: Option<&str>) -> Result<Vec<DiffEntry>> {
        let from_commit = refs::resolve_ref(&self.store, from)?;
        let to_commit = refs::resolve_ref(&self.store, to)?;
        let from_tree = self.commit_tree(from_commit)?;
        let to_tree = self.commit_tree(to_commit)?;
        let from_entries: BTreeMap<String, (Mode, Hash)> = paths::walk(&self.store, &from_tree)?
            .into_iter()
            .map(|(p, m, h)| (p, (m, h)))
            .collect();
        let to_entries: BTreeMap<String, (Mode, Hash)> = paths::walk(&self.store, &to_tree)?
            .into_iter()
            .map(|(p, m, h)| (p, (m, h)))
            .collect();
        type DiffPair = (Option<(Mode, Hash)>, Option<(Mode, Hash)>);
        let mut all: BTreeMap<String, DiffPair> = BTreeMap::new();
        for (p, v) in from_entries {
            all.entry(p).or_default().0 = Some(v);
        }
        for (p, v) in to_entries {
            all.entry(p).or_default().1 = Some(v);
        }
        let mut out = Vec::new();
        for (p, (before, after)) in all {
            if let Some(pf) = path_filter {
                if !p.starts_with(pf) {
                    continue;
                }
            }
            let status = match (&before, &after) {
                (None, None) => DiffStatus::Unchanged,
                (None, Some(_)) => DiffStatus::Added,
                (Some(_), None) => DiffStatus::Removed,
                (Some((_, bh)), Some((_, ah))) => {
                    if bh == ah {
                        DiffStatus::Unchanged
                    } else {
                        DiffStatus::Modified
                    }
                }
            };
            if status == DiffStatus::Unchanged {
                continue;
            }
            out.push(DiffEntry {
                path: p,
                status,
                before: before.map(|x| x.1),
                after: after.map(|x| x.1),
            });
        }
        Ok(out)
    }

    pub fn branch(&self, name: &str, start: Option<&str>) -> Result<()> {
        let _lock = self.write_lock.lock().unwrap();
        let start_hash = match start {
            Some(s) => refs::resolve_ref(&self.store, s)?,
            None => refs::resolve_head(&self.store)?
                .ok_or_else(|| OmpError::Conflict("no commit to branch from".into()))?,
        };
        let ref_path = format!("refs/heads/{name}");
        if self.store.read_ref(&ref_path)?.is_some() {
            return Err(OmpError::Conflict(format!("branch {name} already exists")));
        }
        self.store.write_ref(&ref_path, &start_hash)
    }

    pub fn checkout(&self, r: &str) -> Result<()> {
        let _lock = self.write_lock.lock().unwrap();
        // Branch name shortcut.
        if !r.contains('/') && !r.starts_with("refs/") && r.parse::<Hash>().is_err() {
            let ref_path = format!("refs/heads/{r}");
            if self.store.read_ref(&ref_path)?.is_some() {
                self.store.write_head(&format!("ref: {ref_path}"))?;
                return Ok(());
            }
        }
        let hash = refs::resolve_ref(&self.store, r)?;
        self.store.write_head(&hash.hex())
    }

    pub fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        let current = refs::current_branch(&self.store)?;
        let mut out = Vec::new();
        for (name, hash) in self.store.iter_refs()? {
            if let Some(branch) = name.strip_prefix("refs/heads/") {
                out.push(BranchInfo {
                    name: branch.to_string(),
                    head: Some(hash),
                    is_current: current.as_deref() == Some(&name),
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Dry-run an ingest. `proposed_schema` substitutes the committed schema.
    pub fn test_ingest(
        &self,
        path: &str,
        bytes: &[u8],
        user_fields: Option<Fields>,
        proposed_schema: Option<&[u8]>,
    ) -> Result<Manifest> {
        self.build_manifest(
            path,
            bytes,
            user_fields.unwrap_or_default(),
            None,
            proposed_schema,
            clock_now(),
        )
    }

    // ---- Upload sessions (docs/design/12-large-files.md §Resumable upload sessions) ----

    fn upload_manager(&self) -> Result<crate::uploads::UploadManager> {
        let cfg = self.current_repo_config()?;
        Ok(crate::uploads::UploadManager::new(
            self.store.root(),
            cfg.storage.chunk_size_bytes,
        ))
    }

    /// Open a resumable upload session. Quota is checked up front against
    /// the declared total size — a 200 GB upload that would blow the tenant
    /// cap fails at byte zero, not at chunk 12,000 (doc 12 §Multi-tenancy).
    /// Does **not** hold the tenant write lock; chunk writes are lockless.
    pub fn upload_open(&self, declared_size: u64) -> Result<crate::uploads::UploadHandle> {
        // Quota: soft-check that a write of `declared_size` (plus one
        // manifest + one chunks object on commit) won't exceed the cap.
        // We ignore the exact chunks-object size here (it's ~90 B per chunk,
        // a rounding error against the file size).
        self.check_quota_for_write(declared_size, 1)?;
        self.upload_manager()?.open(declared_size)
    }

    /// Append one chunk at `offset`. Idempotent. Does **not** take the
    /// tenant write lock — PATCH writes only to `.omp/uploads/<id>/`, a
    /// session-local scratch area (doc 12 §Multi-tenancy interactions).
    pub fn upload_write(&self, id: &str, offset: u64, bytes: &[u8]) -> Result<()> {
        self.upload_manager()?.write_chunk(id, offset, bytes)
    }

    /// Finalize an upload: reassemble the session's chunks, run the
    /// chunk-aware ingest pipeline, emit the manifest, stage the upsert.
    /// This **does** take the tenant write lock (it mutates the object
    /// store and staging index).
    pub fn upload_commit(
        &self,
        id: &str,
        path: &str,
        user_fields: Option<Fields>,
        file_type_override: Option<&str>,
    ) -> Result<AddResult> {
        let _lock = self.write_lock.lock().unwrap();
        validate_path(path)?;

        let mgr = self.upload_manager()?;
        let bytes = mgr.assemble(id)?;
        let chunk_size = self.current_repo_config()?.storage.chunk_size_bytes;

        // Route through the chunk-aware path unconditionally — upload
        // sessions exist precisely for large files, and even a tiny upload
        // lands as a single chunk with no loss of generality.
        let res = if (bytes.len() as u64) >= chunk_size {
            self.add_chunked(path, &bytes, user_fields, file_type_override, chunk_size)?
        } else {
            // Small file that happened to be uploaded via a session. Use the
            // plain add path so the single-blob source_hash invariant holds.
            drop(_lock); // add() takes the lock itself
            return self.add(path, &bytes, user_fields, file_type_override);
        };
        // Commit-time cleanup of the session dir. On failure above the
        // session stays for retry; on success we reap it.
        mgr.remove(id)?;
        Ok(res)
    }

    /// Explicitly abort a session. Returns `NotFound` if the id is unknown.
    pub fn upload_abort(&self, id: &str) -> Result<()> {
        let mgr = self.upload_manager()?;
        // Load state first so an unknown id returns NotFound rather than
        // silently succeeding.
        mgr.load_state(id)?;
        mgr.remove(id)
    }

    /// Reap sessions older than the configured TTL. Exposed so `omp admin
    /// gc` or periodic tasks can call it.
    pub fn upload_gc(&self) -> Result<u64> {
        let cfg = self.current_repo_config()?;
        self.upload_manager()?
            .reap_stale(cfg.storage.upload_session_ttl_hours)
    }

    pub fn status(&self) -> Result<RepoStatus> {
        let branch = refs::current_branch(&self.store)?;
        let head = refs::resolve_head(&self.store)?;
        let idx = self.load_index()?;
        let mut staged: Vec<StagedChange> = idx.entries.into_values().collect();
        staged.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(RepoStatus {
            branch,
            head,
            staged,
        })
    }

    /// Predicate-filtered, cursor-paginated query over manifests at `at`.
    /// See `docs/design/15-query-and-discovery.md`. `predicate` of `None` is
    /// "match everything". `cursor` is opaque (caller round-trips it).
    pub fn query(
        &self,
        at: Option<&str>,
        prefix: Option<&str>,
        predicate: Option<&crate::query::Expr>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<crate::query::QueryResult> {
        // Resolve "at" to a concrete commit, so cursor-anchored pagination is
        // stable across pages even if HEAD advances mid-query.
        let resolved_at = self.resolve_at(at)?;

        // Decode cursor.
        let (cursor_commit, start_offset) = match cursor {
            Some(c) => crate::query::decode_cursor(c)?,
            None => (resolved_at.clone(), 0usize),
        };
        // If a cursor was provided, trust ITS commit (anchored snapshot).
        let effective_at = cursor_commit.clone().or(resolved_at);
        let tree_root = match self.root_tree(effective_at.as_deref())? {
            Some(r) => r,
            None => {
                return Ok(crate::query::QueryResult {
                    matches: Vec::new(),
                    next_cursor: None,
                })
            }
        };

        let entries = paths::walk(&self.store, &tree_root)?;
        // Sorted walk so cursor offsets are stable.
        let mut sorted: Vec<_> = entries
            .into_iter()
            .filter(|(_, mode, _)| *mode == Mode::Manifest)
            .filter(|(p, _, _)| match prefix {
                Some(pfx) => p.starts_with(pfx),
                None => true,
            })
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let limit = limit.clamp(1, 1000);
        let mut matches: Vec<crate::query::QueryMatch> = Vec::new();
        let mut next_offset: Option<usize> = None;

        for (idx, (path, _mode, hash)) in sorted.iter().enumerate().skip(start_offset) {
            let (_, content) = self
                .store
                .get(hash)?
                .ok_or_else(|| OmpError::NotFound(format!("manifest {}", hash.hex())))?;
            let manifest = Manifest::parse(&content)?;

            let pass = match predicate {
                None => true,
                Some(expr) => crate::query::evaluate(expr, &manifest),
            };
            if !pass {
                continue;
            }

            matches.push(crate::query::QueryMatch {
                path: path.clone(),
                manifest_hash: *hash,
                source_hash: manifest.source_hash,
                file_type: manifest.file_type,
                fields: manifest.fields,
            });
            if matches.len() >= limit {
                if idx + 1 < sorted.len() {
                    next_offset = Some(idx + 1);
                }
                break;
            }
        }

        let next_cursor =
            next_offset.map(|off| crate::query::encode_cursor(effective_at.as_deref(), off));

        Ok(crate::query::QueryResult {
            matches,
            next_cursor,
        })
    }

    /// List schemas at the given ref (default HEAD) as wire-format summaries.
    /// Walks the `schemas/` subtree, parses each `<file_type>/schema.toml`
    /// blob, and falls back to the working-directory `schemas/` dir for the
    /// pre-first-commit case (mirrors `load_all_schemas_with_key`). Per-folder
    /// layout per `docs/design/25-schema-marketplace.md`. Result is sorted by
    /// file_type for stable output.
    pub fn list_schemas(&self, at: Option<&str>) -> Result<Vec<crate::schema::SchemaSummary>> {
        let mut by_type: BTreeMap<String, crate::schema::SchemaSummary> = BTreeMap::new();
        if let Some(root) = self.root_tree(at)? {
            if let Some((Mode::Tree, subtree)) =
                paths::get_at_with_key(&self.store, "schemas", &root, None)?
            {
                let entries = paths::walk_with_key(&self.store, &subtree, None)?;
                for (name, mode, hash) in entries {
                    if mode != Mode::Blob {
                        continue;
                    }
                    let Some(stem) = schema_file_type_from_tree_name(&name) else {
                        continue;
                    };
                    if let Some((_, content)) = self.store.get(&hash)? {
                        if let Ok(s) = Schema::parse(&content, stem) {
                            by_type.insert(s.file_type.clone(), s.summary());
                        }
                    }
                }
            }
        }
        // Pre-first-commit: surface schemas dropped on disk by `init`.
        if at.is_none() {
            let schemas_dir = self.root.join("schemas");
            if schemas_dir.is_dir() {
                for entry in
                    fs::read_dir(&schemas_dir).map_err(|e| OmpError::io(&schemas_dir, e))?
                {
                    let entry = entry.map_err(|e| OmpError::io(&schemas_dir, e))?;
                    let dir = entry.path();
                    if !dir.is_dir() {
                        continue;
                    }
                    let stem = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    let p = dir.join("schema.toml");
                    if !p.is_file() {
                        continue;
                    }
                    let bytes = fs::read(&p).map_err(|e| OmpError::io(&p, e))?;
                    if let Ok(s) = Schema::parse(&bytes, stem) {
                        by_type
                            .entry(s.file_type.clone())
                            .or_insert_with(|| s.summary());
                    }
                }
            }
        }
        Ok(by_type.into_values().collect())
    }

    /// Resolve `at` to a concrete commit hash string, or None for HEAD-on-empty.
    fn resolve_at(&self, at: Option<&str>) -> Result<Option<String>> {
        match at {
            Some(s) => Ok(Some(s.to_string())),
            None => match refs::resolve_head(&self.store)? {
                Some(h) => Ok(Some(h.hex())),
                None => Ok(None),
            },
        }
    }

    // ---- internals ----

    fn build_manifest(
        &self,
        path: &str,
        bytes: &[u8],
        user_fields: Fields,
        file_type_override: Option<&str>,
        proposed_schema_bytes: Option<&[u8]>,
        ingested_at: String,
    ) -> Result<Manifest> {
        self.build_manifest_with(
            path,
            bytes,
            user_fields,
            file_type_override,
            proposed_schema_bytes,
            ingested_at,
            ChunkedIngest::None,
            None,
        )
    }

    /// Internal: extended entry point that lets the chunked-ingest path
    /// swap `source_hash` and provide streaming-builtin values without
    /// duplicating schema / MIME / probe loading. The `path_key` is
    /// threaded to the readers so encrypted-tenant ingest can resolve
    /// `schemas/` and `omp.toml` through encrypted-name trees.
    #[allow(clippy::too_many_arguments)]
    fn build_manifest_with(
        &self,
        path: &str,
        bytes: &[u8],
        user_fields: Fields,
        file_type_override: Option<&str>,
        proposed_schema_bytes: Option<&[u8]>,
        ingested_at: String,
        chunked: ChunkedIngest,
        path_key: Option<&[u8; 32]>,
    ) -> Result<Manifest> {
        let repo_config = self.current_repo_config_with_key(path_key)?;
        // MIME sniff uses the first chunk's bytes for chunked ingest; the
        // engine doesn't need them beyond type detection because probes
        // don't run on chunked content.
        let mime = match &chunked {
            ChunkedIngest::None => sniff_mime(bytes),
            ChunkedIngest::Chunked { sniff_prefix, .. } => sniff_mime(sniff_prefix),
        };

        let schemas = self.load_all_schemas_with_key(path_key)?;
        let (schema, schema_bytes) = if let Some(proposed) = proposed_schema_bytes {
            // The schema's own `file_type` field is authoritative; parse twice
            // if needed so we can pass it in as the expected filename stem.
            let peek_str = std::str::from_utf8(proposed)
                .map_err(|_| OmpError::SchemaValidation("proposed schema not UTF-8".into()))?;
            let peek: toml::Value = toml::from_str(peek_str)
                .map_err(|e| OmpError::SchemaValidation(format!("proposed schema TOML: {e}")))?;
            let stem = peek
                .get("file_type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| file_type_override.map(str::to_string))
                .ok_or_else(|| {
                    OmpError::SchemaValidation("proposed schema lacks `file_type`".into())
                })?;
            let parsed = Schema::parse(proposed, &stem)?;
            (parsed, proposed.to_vec())
        } else if let Some(schema) = engine::detect_file_type(file_type_override, &mime, &schemas) {
            let bytes = self
                .schema_bytes_at_head_with_key(&schema.file_type, path_key)?
                .ok_or_else(|| OmpError::internal("schema detected but blob missing"))?;
            (schema.clone(), bytes)
        } else {
            // Fall back to the minimal schema policy.
            match repo_config.ingest.default_schema_policy {
                crate::config::DefaultSchemaPolicy::Reject => {
                    return Err(OmpError::IngestValidation(format!(
                        "no schema matches MIME type {mime:?}; set default_schema_policy to 'minimal' or define a schema"
                    )));
                }
                crate::config::DefaultSchemaPolicy::Minimal => {
                    let parsed = Schema::parse(crate::schema::MINIMAL_SCHEMA, "_minimal")?;
                    (parsed, crate::schema::MINIMAL_SCHEMA.to_vec())
                }
            }
        };

        // Gather probes referenced by the schema. For v1, we always look into
        // HEAD's tree for probes — proposed_schema dry-runs still use the
        // committed probe set (can't do otherwise without staging them too).
        let probes = self.current_probes()?;
        let probe_names: HashSet<String> = probes.keys().cloned().collect();
        schema.validate_probe_refs(&probe_names)?;

        let (streaming_builtins, content_length, override_source_hash) = match &chunked {
            ChunkedIngest::None => (None, bytes.len() as u64, None),
            ChunkedIngest::Chunked {
                source_hash,
                file_size,
                sha256_hex,
                ..
            } => {
                let mut hex = [0u8; 64];
                hex.copy_from_slice(sha256_hex.as_bytes());
                (
                    Some(engine::StreamingBuiltins {
                        file_sha256_hex: hex,
                        file_size: *file_size,
                    }),
                    *file_size,
                    Some(*source_hash),
                )
            }
        };
        let input = IngestInput {
            bytes,
            user_fields,
            path,
            ingested_at: &ingested_at,
            streaming_builtins,
            content_length,
            override_source_hash,
        };
        let view = TreeView {
            schema: &schema,
            schema_blob: &schema_bytes,
            probes: &probes,
            // Tenant quotas clip below the repo's [probes] defaults — see
            // 11-multi-tenancy.md §Quotas. Single-tenant mode passes an
            // unlimited Quotas, leaving the repo defaults intact.
            limits: self.quotas.clamp_probe(repo_config.probes),
        };
        engine::ingest(&input, &view)
    }

    fn root_tree(&self, at: Option<&str>) -> Result<Option<Hash>> {
        match at {
            Some(expr) => {
                let commit = refs::resolve_ref(&self.store, expr)?;
                Ok(Some(self.commit_tree(commit)?))
            }
            None => self.head_tree(),
        }
    }

    fn head_tree(&self) -> Result<Option<Hash>> {
        let head = refs::resolve_head(&self.store)?;
        match head {
            Some(h) => Ok(Some(self.commit_tree(h)?)),
            None => Ok(None),
        }
    }

    fn head_commit(&self) -> Result<Option<Hash>> {
        refs::resolve_head(&self.store)
    }

    fn commit_tree(&self, commit_hash: Hash) -> Result<Hash> {
        let (ty, content) = self
            .store
            .get(&commit_hash)?
            .ok_or_else(|| OmpError::NotFound(format!("commit {}", commit_hash.hex())))?;
        if ty != ObjectType::Commit.as_str() {
            return Err(OmpError::Corrupt(format!(
                "expected commit at {}",
                commit_hash.hex()
            )));
        }
        // For encrypted commits the message body is sealed and `Commit::parse`
        // refuses without a key. The `tree` header is plaintext in both
        // modes, so parse headers only.
        let s = std::str::from_utf8(&content)
            .map_err(|_| OmpError::Corrupt("commit is not UTF-8".into()))?;
        for line in s.lines() {
            if line.is_empty() {
                break;
            }
            if let Some(rest) = line.strip_prefix("tree ") {
                return rest
                    .parse()
                    .map_err(|e| OmpError::Corrupt(format!("commit tree: {e}")));
            }
        }
        Err(OmpError::Corrupt(format!(
            "commit {} missing tree header",
            commit_hash.hex()
        )))
    }

    fn current_repo_config(&self) -> Result<RepoConfig> {
        self.current_repo_config_with_key(None)
    }

    fn current_repo_config_with_key(&self, path_key: Option<&[u8; 32]>) -> Result<RepoConfig> {
        // Prefer HEAD's omp.toml; fall back to filesystem; fall back to default.
        if let Some(root) = self.head_tree()? {
            if let Some((Mode::Blob, h)) =
                paths::get_at_with_key(&self.store, "omp.toml", &root, path_key)?
            {
                if let Some((_, content)) = self.store.get(&h)? {
                    return RepoConfig::parse(&content);
                }
            }
        }
        let fs_path = self.root.join("omp.toml");
        if fs_path.exists() {
            let bytes = fs::read(&fs_path).map_err(|e| OmpError::io(&fs_path, e))?;
            return RepoConfig::parse(&bytes);
        }
        Ok(RepoConfig::default())
    }

    fn schema_bytes_at_head_with_key(
        &self,
        file_type: &str,
        path_key: Option<&[u8; 32]>,
    ) -> Result<Option<Vec<u8>>> {
        let path = format!("schemas/{file_type}/schema.toml");
        if let Some(root) = self.head_tree()? {
            if let Some((Mode::Blob, h)) =
                paths::get_at_with_key(&self.store, &path, &root, path_key)?
            {
                if let Some((_, content)) = self.store.get(&h)? {
                    return Ok(Some(content));
                }
            }
        }
        // Fall back to the filesystem so the *very first* ingest before any
        // commit still sees the starter schemas dropped by `init`.
        let fs_path = self.root.join(&path);
        if fs_path.exists() {
            return Ok(Some(
                fs::read(&fs_path).map_err(|e| OmpError::io(&fs_path, e))?,
            ));
        }
        Ok(None)
    }

    fn load_all_schemas_with_key(
        &self,
        path_key: Option<&[u8; 32]>,
    ) -> Result<BTreeMap<String, Schema>> {
        let mut out: BTreeMap<String, Schema> = BTreeMap::new();
        // From HEAD's tree.
        if let Some(root) = self.head_tree()? {
            if let Some((Mode::Tree, subtree)) =
                paths::get_at_with_key(&self.store, "schemas", &root, path_key)?
            {
                let entries = paths::walk_with_key(&self.store, &subtree, path_key)?;
                for (name, mode, hash) in entries {
                    if mode != Mode::Blob {
                        continue;
                    }
                    if let Some(stem) = schema_file_type_from_tree_name(&name) {
                        if let Some((_, content)) = self.store.get(&hash)? {
                            if let Ok(s) = Schema::parse(&content, stem) {
                                out.insert(s.file_type.clone(), s);
                            }
                        }
                    }
                }
            }
        }
        // From the working tree — covers the pre-first-commit case.
        let schemas_dir = self.root.join("schemas");
        if schemas_dir.is_dir() {
            for entry in fs::read_dir(&schemas_dir).map_err(|e| OmpError::io(&schemas_dir, e))? {
                let entry = entry.map_err(|e| OmpError::io(&schemas_dir, e))?;
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let stem = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
                let p = dir.join("schema.toml");
                if !p.is_file() {
                    continue;
                }
                let bytes = fs::read(&p).map_err(|e| OmpError::io(&p, e))?;
                if let Ok(s) = Schema::parse(&bytes, stem) {
                    out.entry(s.file_type.clone()).or_insert(s);
                }
            }
        }
        Ok(out)
    }

    fn current_probes(&self) -> Result<HashMap<String, ProbeBlob<'static>>> {
        // Populate from the starter pack first (zero-copy via
        // `Cow::Borrowed`), then merge in anything under `probes/` in HEAD's
        // tree (`Cow::Owned` from the object store). User probes shadow
        // starter probes of the same name. See `docs/design/20-server-side-probes.md`
        // and `docs/design/05-probes.md`.
        let mut out: HashMap<String, ProbeBlob<'static>> = HashMap::new();
        for probe in STARTER_PROBES {
            let framed_hash = hash_of(ObjectType::Blob, probe.wasm);
            let manifest = crate::probes::ProbeManifest::parse(probe.manifest_toml)?;
            out.insert(
                probe.name.to_string(),
                ProbeBlob {
                    wasm: Cow::Borrowed(probe.wasm),
                    framed_hash,
                    max_input_bytes: manifest.max_input_bytes,
                },
            );
        }

        if let Some(tree_root) = self.root_tree(None)? {
            // Encrypted tenants do client-side ingest (doc 13) so server-side
            // probe walking doesn't apply. The walker returns `Unauthorized`
            // when it hits an encrypted tree without a path key — treat that
            // as "no tree-resident probes available" rather than failing the
            // whole ingest.
            let walk_result = paths::walk(&self.store, &tree_root);
            let entries = match walk_result {
                Ok(entries) => entries,
                Err(OmpError::Unauthorized(_)) => return Ok(out),
                Err(e) => return Err(e),
            };
            for (path, mode, hash) in entries {
                if mode != Mode::Blob {
                    continue;
                }
                let Some(rest) = path.strip_prefix("probes/") else {
                    continue;
                };
                // Per `docs/design/23-probe-marketplace.md`, every probe
                // lives in its own directory: `probes/<ns>/<name>/probe.wasm`
                // + `probe.toml` + optional README/source. Surface the
                // pre-doc-23 flat layout (`probes/<ns>/<name>.wasm`) as a
                // skipped entry with a warning so a half-migrated repo is
                // visible rather than silently inert.
                let Some(name_rest) = rest.strip_suffix("/probe.wasm") else {
                    if rest.ends_with(".wasm") {
                        tracing::warn!(
                            path = %path,
                            "probe is in legacy flat layout; expected `probes/<ns>/<name>/probe.wasm` (see doc 23). skipping."
                        );
                    }
                    continue;
                };
                // `name_rest` is now `<namespace>/<name>` — exactly two slash
                // components. Qualified registry key is dotted.
                if name_rest.matches('/').count() != 1 {
                    tracing::warn!(
                        path = %path,
                        "probe path has unexpected depth; expected `probes/<ns>/<name>/probe.wasm`. skipping."
                    );
                    continue;
                }
                let manifest_path = format!("probes/{name_rest}/probe.toml");
                let Some((Mode::Blob, manifest_hash)) =
                    paths::get_at(&self.store, &manifest_path, &tree_root)?
                else {
                    continue;
                };
                let qualified = name_rest.replace('/', ".");

                // If the starter pack already registered this name with the
                // same bytes (the common case after `omp init` writes the
                // starter set into the tree), skip — Borrowed is cheaper
                // than Owned and the framed_hash already matches.
                if let Some(existing) = out.get(&qualified) {
                    if existing.framed_hash == hash {
                        continue;
                    }
                    tracing::warn!(
                        probe = %qualified,
                        starter_hash = %existing.framed_hash.hex(),
                        tree_hash = %hash.hex(),
                        "tree-resident probe shadows starter pack with different bytes"
                    );
                }

                let (_, wasm_bytes) = self.store.get(&hash)?.ok_or_else(|| {
                    OmpError::Corrupt(format!(
                        "probe blob {} listed in tree but missing from store",
                        hash.hex()
                    ))
                })?;
                let (_, manifest_bytes) = self.store.get(&manifest_hash)?.ok_or_else(|| {
                    OmpError::Corrupt(format!(
                        "probe.toml blob {} listed in tree but missing from store",
                        manifest_hash.hex()
                    ))
                })?;
                let manifest = crate::probes::ProbeManifest::parse(&manifest_bytes)?;

                out.insert(
                    qualified,
                    ProbeBlob {
                        wasm: Cow::Owned(wasm_bytes),
                        framed_hash: hash,
                        max_input_bytes: manifest.max_input_bytes,
                    },
                );
            }
        }
        Ok(out)
    }

    fn current_probe_names(&self) -> Result<HashSet<String>> {
        // Mirror `current_probes` but name-only (faster; used for schema
        // validation). User-uploaded probes in the tree must be visible here
        // too, otherwise schemas referencing them would be rejected at
        // validation despite the engine knowing how to run them.
        let mut out: HashSet<String> = STARTER_PROBES.iter().map(|p| p.name.to_string()).collect();
        if let Some(tree_root) = self.root_tree(None)? {
            let entries = match paths::walk(&self.store, &tree_root) {
                Ok(e) => e,
                // Encrypted trees: see comment in `current_probes`.
                Err(OmpError::Unauthorized(_)) => return Ok(out),
                Err(e) => return Err(e),
            };
            for (path, mode, _hash) in entries {
                if mode != Mode::Blob {
                    continue;
                }
                let Some(rest) = path.strip_prefix("probes/") else {
                    continue;
                };
                let Some(name_rest) = rest.strip_suffix("/probe.wasm") else {
                    continue;
                };
                if name_rest.matches('/').count() != 1 {
                    continue;
                }
                let manifest_path = format!("probes/{name_rest}/probe.toml");
                if matches!(
                    paths::get_at(&self.store, &manifest_path, &tree_root)?,
                    Some((Mode::Blob, _))
                ) {
                    out.insert(name_rest.replace('/', "."));
                }
            }
        }
        Ok(out)
    }

    fn stored_mode(&self, hash: &Hash) -> Result<Mode> {
        let (ty, _) = self
            .store
            .get(hash)?
            .ok_or_else(|| OmpError::NotFound(format!("object {}", hash.hex())))?;
        Ok(match ty.as_str() {
            "blob" => Mode::Blob,
            "manifest" => Mode::Manifest,
            "tree" => Mode::Tree,
            other => {
                return Err(OmpError::Corrupt(format!(
                    "unexpected object type for staged entry: {other}"
                )))
            }
        })
    }

    fn list_tree(&self, hash: Hash) -> Result<Vec<TreeEntryOut>> {
        let (ty, content) = self
            .store
            .get(&hash)?
            .ok_or_else(|| OmpError::NotFound(format!("tree {}", hash.hex())))?;
        if ty != "tree" {
            return Err(OmpError::Corrupt("expected tree".into()));
        }
        let tree = Tree::parse(&content)?;
        Ok(tree
            .entries()
            .map(|(name, e)| TreeEntryOut {
                name: name.to_string(),
                mode: e.mode.as_str().to_string(),
                hash: e.hash,
            })
            .collect())
    }

    fn resolve_staged_or_head(&self, path: &str) -> Result<(Mode, Hash)> {
        let idx = self.load_index()?;
        if let Some(change) = idx.entries.get(path) {
            if change.kind == StagedKind::Delete {
                return Err(OmpError::NotFound(format!(
                    "path {path:?} is staged for deletion"
                )));
            }
            let hash = change
                .hash
                .ok_or_else(|| OmpError::internal("staged upsert without hash"))?;
            return Ok((self.stored_mode(&hash)?, hash));
        }
        let root = self
            .head_tree()?
            .ok_or_else(|| OmpError::NotFound(format!("path {path:?}: no HEAD")))?;
        paths::get_at(&self.store, path, &root)?
            .ok_or_else(|| OmpError::NotFound(format!("path {path:?}")))
    }

    fn stage_upsert(&self, path: &str, mode: Mode, hash: Hash) -> Result<()> {
        let _ = mode;
        let mut idx = self.load_index()?;
        idx.entries.insert(
            path.to_string(),
            StagedChange {
                path: path.to_string(),
                kind: StagedKind::Upsert,
                hash: Some(hash),
            },
        );
        self.save_index(&idx)
    }

    fn index_path(&self) -> PathBuf {
        self.store.root().join("index.json")
    }

    fn load_index(&self) -> Result<Index> {
        let p = self.index_path();
        if !p.exists() {
            return Ok(Index::default());
        }
        let s = fs::read_to_string(&p).map_err(|e| OmpError::io(&p, e))?;
        serde_json::from_str(&s).map_err(|e| OmpError::Corrupt(format!("index.json: {e}")))
    }

    fn save_index(&self, idx: &Index) -> Result<()> {
        let p = self.index_path();
        let s = serde_json::to_string_pretty(idx)
            .map_err(|e| OmpError::internal(format!("index serialize: {e}")))?;
        fs::write(&p, s).map_err(|e| OmpError::io(&p, e))
    }
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(OmpError::InvalidPath("empty path".into()));
    }
    if path.starts_with('/') || path.contains("//") {
        return Err(OmpError::InvalidPath(format!(
            "absolute or double-slash path: {path:?}"
        )));
    }
    if path.starts_with(".omp/") || path == ".omp" {
        return Err(OmpError::InvalidPath(
            ".omp/ is reserved for private state".into(),
        ));
    }
    paths::split(path)?;
    Ok(())
}

/// Keep the first 8 KiB of plaintext for MIME sniffing. `infer::get` only
/// examines leading bytes; 8 KiB covers every format in the starter sniff
/// table and is independent of `chunk_size_bytes`.
fn sniff_prefix_of(bytes: &[u8]) -> Vec<u8> {
    bytes[..bytes.len().min(8 * 1024)].to_vec()
}

fn sniff_mime(bytes: &[u8]) -> String {
    if let Some(kind) = infer::get(bytes) {
        return kind.mime_type().to_string();
    }
    if looks_like_text(bytes) {
        "text/plain".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let sample = &bytes[..bytes.len().min(4096)];
    if sample.contains(&0) {
        return false;
    }
    let printable = sample
        .iter()
        .filter(|&&b| {
            b == b'\n' || b == b'\r' || b == b'\t' || (0x20..0x7F).contains(&b) || b >= 0x80
        })
        .count();
    printable * 100 / sample.len() >= 95
}

/// True iff two schemas are equal in every aspect that affects manifest
/// content. Used by the auto-reprobe hook to skip rebuilds when the
/// staged schema is byte-identical to (or semantically the same as) the
/// HEAD schema. See `docs/design/21-schema-reprobe.md`.
fn schemas_equal(a: &Schema, b: &Schema) -> bool {
    if a.file_type != b.file_type
        || a.allow_extra_fields != b.allow_extra_fields
        || a.fields.len() != b.fields.len()
    {
        return false;
    }
    for (af, bf) in a.fields.iter().zip(b.fields.iter()) {
        if af.name != bf.name
            || af.required != bf.required
            || af.type_ != bf.type_
            || af.source != bf.source
            || af.fallback != bf.fallback
        {
            return false;
        }
    }
    true
}

fn clock_now() -> String {
    crate::time::now_rfc3339()
}

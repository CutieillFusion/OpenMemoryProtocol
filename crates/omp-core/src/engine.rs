//! Ingest engine. See `04-schemas.md §The ingest pipeline`.
//!
//! Given a file's bytes + caller-supplied fields + a schema (loaded from the
//! tree), resolve every field, validate against declared types, and emit a
//! manifest. Writes nothing — the caller (`omp_core::api`) is responsible for
//! staging the manifest + blob.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use crate::config::ProbeLimits;
use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::manifest::{FieldValue, Manifest};
use crate::object::{hash_of, ObjectType};
use crate::probes::{self, ProbeConfig};
use crate::schema::{Field, FieldType, Schema, Source, Transform};
use crate::INGESTER_VERSION;

/// Inputs that fully determine an ingest.
pub struct IngestInput<'a> {
    pub bytes: &'a [u8],
    /// Caller-provided fields. Keyed by field name.
    pub user_fields: BTreeMap<String, FieldValue>,
    /// Repo path the file is being ingested at. Used by probes like `file.name`.
    pub path: &'a str,
    /// Pinned clock, in RFC 3339 UTC. Callers (especially tests) inject this
    /// so manifests are reproducible.
    pub ingested_at: &'a str,
    /// Ingest-time values for universal, cheap properties that the chunked
    /// pipeline already computes in a single pass over the file. When the
    /// probe declared for `file.size` / `file.sha256` has a `max_input_bytes`
    /// smaller than the blob, or when the blob is a `chunks` object (probes
    /// can't read chunked bodies), the engine uses these values instead of
    /// invoking the WASM probe. See `docs/design/12-large-files.md
    /// §Streaming built-ins`.
    pub streaming_builtins: Option<StreamingBuiltins>,
    /// Total unchunked plaintext length. When `bytes` points at the raw file
    /// (v1 path), this equals `bytes.len()`. For chunked ingest, `bytes` is
    /// empty (no plaintext in the engine) and this carries the real length.
    /// Used to gate probe invocation against `max_input_bytes`.
    pub content_length: u64,
    /// If set, use this as `manifest.source_hash` instead of hashing
    /// `input.bytes` as a blob. Chunked ingest sets this to the hash of the
    /// `chunks` object — see `docs/design/12-large-files.md §The shape of
    /// the change`.
    pub override_source_hash: Option<Hash>,
}

/// Values computed during chunked ingest that the engine may substitute for
/// probe output when the probe cannot run. Byte-identical to what the WASM
/// probe would have returned on the same plaintext.
#[derive(Clone, Copy, Debug)]
pub struct StreamingBuiltins {
    /// SHA-256 of the plaintext file content, hex-lowercase. Matches what
    /// `file.sha256` returns on the full bytes.
    pub file_sha256_hex: [u8; 64],
    /// Byte length of the plaintext file content. Matches `file.size`.
    pub file_size: u64,
}

impl StreamingBuiltins {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        let mut hex = [0u8; 64];
        for (i, b) in digest.iter().enumerate() {
            let hi = b >> 4;
            let lo = b & 0x0f;
            hex[i * 2] = nibble_to_hex(hi);
            hex[i * 2 + 1] = nibble_to_hex(lo);
        }
        StreamingBuiltins {
            file_sha256_hex: hex,
            file_size: bytes.len() as u64,
        }
    }

    pub fn sha256_string(&self) -> String {
        String::from_utf8(self.file_sha256_hex.to_vec()).expect("hex is ascii")
    }
}

fn nibble_to_hex(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        10..=15 => b'a' + (n - 10),
        _ => unreachable!(),
    }
}

/// All the tree-resident artifacts the engine needs to resolve an ingest.
pub struct TreeView<'a> {
    /// The schema whose file_type matches the file.
    pub schema: &'a Schema,
    /// Blob bytes of the exact schema file used. Hashed for `schema_hash`.
    pub schema_blob: &'a [u8],
    /// Map from `"namespace.name"` to (wasm_blob, framed_hash).
    ///
    /// The framed hash is the tree mode-`blob` reference the tree uses;
    /// stored on the manifest so the extraction replays bit-for-bit.
    pub probes: &'a HashMap<String, ProbeBlob<'a>>,
    /// Default resource limits from `omp.toml`.
    pub limits: ProbeLimits,
}

#[derive(Clone)]
pub struct ProbeBlob<'a> {
    /// Probe wasm bytes. `Cow::Borrowed` for the embedded starter pack
    /// (zero-copy from `include_bytes!`) and `Cow::Owned` for probes loaded
    /// dynamically from the tree at ingest. See `docs/design/20-server-side-probes.md`.
    pub wasm: Cow<'a, [u8]>,
    /// Framed-object hash (`hash_of(Blob, wasm_bytes)`).
    pub framed_hash: Hash,
    /// From `[limits].max_input_bytes` in the probe's `.probe.toml`. `None`
    /// means "no explicit cap" — the engine falls back to comparing against
    /// `view.limits.memory_mb * 1 MiB` per doc 12 §max_input_bytes.
    pub max_input_bytes: Option<u64>,
}

/// The raw hash of the schema blob, matching the tree entry.
pub fn schema_hash(schema_bytes: &[u8]) -> Hash {
    hash_of(ObjectType::Blob, schema_bytes)
}

/// Cache of `(source_hash, probe_framed_hash, canonical_args)` →
/// `FieldValue`. Used during a reprobe pass to skip redundant probe runs
/// across files with identical source bytes. Single-file ingests trivially
/// pass an empty cache and fill it as they go (no benefit, no harm).
///
/// See `docs/design/21-schema-reprobe.md`.
pub type ProbeOutputCache = HashMap<(Hash, Hash, String), FieldValue>;

/// Resolve every field in `schema.fields` and return a ready-to-store manifest.
///
/// The caller is responsible for (a) writing the blob and capturing its hash
/// (via `hash_of(ObjectType::Blob, bytes)`), then (b) writing the manifest.
pub fn ingest(input: &IngestInput<'_>, view: &TreeView<'_>) -> Result<Manifest> {
    let mut cache = ProbeOutputCache::new();
    ingest_with_cache(input, view, &mut cache)
}

/// Same as `ingest` but lets the caller share a probe-output cache across
/// many calls. Reprobe passes use this so that two files with byte-identical
/// source share probe results.
pub fn ingest_with_cache(
    input: &IngestInput<'_>,
    view: &TreeView<'_>,
    cache: &mut ProbeOutputCache,
) -> Result<Manifest> {
    let mut resolved: BTreeMap<String, FieldValue> = BTreeMap::new();
    let mut probe_hashes: BTreeMap<String, Hash> = BTreeMap::new();

    for field in &view.schema.fields {
        let value = resolve_field(field, input, view, &resolved, &mut probe_hashes, cache)?;
        if field.required && value.is_null() {
            return Err(OmpError::IngestValidation(format!(
                "required field {:?} resolved to null",
                field.name
            )));
        }
        if !field.type_.accepts(&value) {
            return Err(OmpError::IngestValidation(format!(
                "field {:?}: value {:?} does not match declared type {:?}",
                field.name, value, field.type_
            )));
        }
        resolved.insert(field.name.clone(), normalize_value(value, field.type_));
    }

    // Reject caller-supplied fields that don't exist in the schema (unless
    // `allow_extra_fields = true`).
    if !view.schema.allow_extra_fields {
        for k in input.user_fields.keys() {
            if !view.schema.fields.iter().any(|f| &f.name == k) {
                return Err(OmpError::IngestValidation(format!(
                    "field {:?} is not declared in schema (and allow_extra_fields is false)"
                    , k
                )));
            }
        }
    } else {
        // Add extras verbatim.
        for (k, v) in &input.user_fields {
            if !resolved.contains_key(k) {
                resolved.insert(k.clone(), v.clone());
            }
        }
    }

    let source_hash = input
        .override_source_hash
        .unwrap_or_else(|| hash_of(ObjectType::Blob, input.bytes));

    Ok(Manifest {
        source_hash,
        file_type: view.schema.file_type.clone(),
        schema_hash: schema_hash(view.schema_blob),
        ingested_at: input.ingested_at.to_string(),
        ingester_version: INGESTER_VERSION.to_string(),
        probe_hashes,
        fields: resolved,
    })
}

fn resolve_field(
    field: &Field,
    input: &IngestInput<'_>,
    view: &TreeView<'_>,
    resolved: &BTreeMap<String, FieldValue>,
    probe_hashes: &mut BTreeMap<String, Hash>,
    cache: &mut ProbeOutputCache,
) -> Result<FieldValue> {
    let primary =
        resolve_source(&field.source, field, input, view, resolved, probe_hashes, cache)?;
    if !primary.is_null() {
        return Ok(primary);
    }
    if let Some(fb) = &field.fallback {
        let fb_val = resolve_field(fb, input, view, resolved, probe_hashes, cache)?;
        return Ok(fb_val);
    }
    Ok(primary)
}

fn resolve_source(
    source: &Source,
    field: &Field,
    input: &IngestInput<'_>,
    view: &TreeView<'_>,
    resolved: &BTreeMap<String, FieldValue>,
    probe_hashes: &mut BTreeMap<String, Hash>,
    cache: &mut ProbeOutputCache,
) -> Result<FieldValue> {
    match source {
        Source::Constant { value } => Ok(value.clone()),

        Source::Probe { probe, args } => {
            let blob = view.probes.get(probe).ok_or_else(|| {
                OmpError::SchemaValidation(format!(
                    "field {:?}: probe {:?} not available in tree",
                    field.name, probe
                ))
            })?;

            // Two reasons a probe may not run on this ingest:
            // 1. `max_input_bytes` from its `.probe.toml` (or the repo-wide
            //    `memory_mb` fallback) is below the content length. See
            //    `docs/design/12-large-files.md §Probes on large files`.
            // 2. The ingest is chunked — the engine has no plaintext in
            //    memory to feed the sandbox. Streaming built-ins can still
            //    cover `file.size` / `file.sha256` byte-identically.
            //
            // We detect the chunked case by the presence of streaming
            // built-ins; in that mode, ONLY streaming built-ins run, even if
            // the probe's declared `max_input_bytes` would otherwise admit
            // the content.
            let effective_cap: u64 = blob
                .max_input_bytes
                .unwrap_or(view.limits.memory_mb as u64 * 1024 * 1024);
            let exceeds_cap = input.content_length > effective_cap;
            let chunked = input.streaming_builtins.is_some();

            if exceeds_cap || chunked {
                if let Some(builtins) = input.streaming_builtins.as_ref() {
                    match probe.as_str() {
                        "file.size" => {
                            return Ok(FieldValue::Int(builtins.file_size as i64));
                        }
                        "file.sha256" => {
                            return Ok(FieldValue::String(builtins.sha256_string()));
                        }
                        _ => {}
                    }
                }
                // No built-in available: probe simply doesn't run. The field
                // resolves via `fallback` or to Null. We do NOT insert a
                // probe_hash — the probe didn't fire.
                return Ok(FieldValue::Null);
            }

            // Probes that take kwarg `path` pick it up from the ingest input.
            let mut effective = args.clone();
            if !effective.contains_key("path") {
                effective.insert("path".to_string(), FieldValue::String(input.path.to_string()));
            }

            // Cache lookup. Key is (source_hash, probe_framed_hash,
            // canonical_args). The source hash comes from the ingest
            // input (set explicitly by reprobe via `override_source_hash`,
            // computed on the fly for fresh ingest).
            let source_hash = input
                .override_source_hash
                .unwrap_or_else(|| hash_of(ObjectType::Blob, input.bytes));
            let args_canonical = canonical_args_key(&effective);
            let cache_key = (source_hash, blob.framed_hash, args_canonical);
            if let Some(cached) = cache.get(&cache_key) {
                probe_hashes.insert(probe.clone(), blob.framed_hash);
                return Ok(cached.clone());
            }

            let cfg = ProbeConfig {
                fuel: view.limits.fuel,
                memory_mb: view.limits.memory_mb,
                wall_clock_s: view.limits.wall_clock_s,
            };
            let out = probes::run_probe(probe, &blob.wasm, input.bytes, &effective, &cfg)?;
            probe_hashes.insert(probe.clone(), blob.framed_hash);
            cache.insert(cache_key, out.value.clone());
            Ok(out.value)
        }

        Source::UserProvided => Ok(input
            .user_fields
            .get(&field.name)
            .cloned()
            .unwrap_or(FieldValue::Null)),

        Source::Field { from, transform } => {
            let base = resolved
                .get(from)
                .cloned()
                .unwrap_or(FieldValue::Null);
            Ok(transform.apply(base))
        }
    }
}

/// Silence the unused-enum-variant warning when the only use of `Transform`
/// is inside this file.
#[allow(dead_code)]
const _: Transform = Transform::Identity;

/// Stable canonical form of a probe's `args` map for use as a cache key.
///
/// `BTreeMap` already iterates in sorted-key order, and `serde_json` for
/// untagged `FieldValue` produces deterministic output, so the resulting
/// string is identical for any two semantically-equal arg maps. Used by
/// `ProbeOutputCache`; not part of any wire format.
fn canonical_args_key(args: &BTreeMap<String, FieldValue>) -> String {
    serde_json::to_string(args).unwrap_or_else(|_| String::new())
}

/// Normalize probe outputs before storage. Primarily: if a probe returned an
/// `Int` but the field is declared `Float`, promote it.
fn normalize_value(v: FieldValue, ty: FieldType) -> FieldValue {
    match (ty, v) {
        (FieldType::Float, FieldValue::Int(i)) => FieldValue::Float(i as f64),
        (FieldType::ListFloat, FieldValue::List(items)) => FieldValue::List(
            items
                .into_iter()
                .map(|i| match i {
                    FieldValue::Int(x) => FieldValue::Float(x as f64),
                    other => other,
                })
                .collect(),
        ),
        (_, v) => v,
    }
}

/// Detect a file's type: explicit override wins; else match MIME against
/// every loaded schema's `mime_patterns` (alphabetical, first match wins).
pub fn detect_file_type<'a>(
    override_type: Option<&str>,
    mime: &str,
    schemas: &'a BTreeMap<String, Schema>,
) -> Option<&'a Schema> {
    if let Some(t) = override_type {
        return schemas.get(t);
    }
    for (_, schema) in schemas.iter() {
        for pat in &schema.mime_patterns {
            if crate::schema::mime_matches(pat, mime) {
                return Some(schema);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::starter::STARTER_PROBES;
    use crate::schema::Schema;

    fn probe_blobs<'a>() -> HashMap<String, ProbeBlob<'a>> {
        let mut out = HashMap::new();
        for p in STARTER_PROBES {
            let manifest = crate::probes::ProbeManifest::parse(p.manifest_toml).unwrap();
            out.insert(
                p.name.to_string(),
                ProbeBlob {
                    wasm: Cow::Borrowed(p.wasm),
                    framed_hash: hash_of(ObjectType::Blob, p.wasm),
                    max_input_bytes: manifest.max_input_bytes,
                },
            );
        }
        out
    }

    const TEST_SCHEMA: &[u8] = br#"file_type = "text"
mime_patterns = ["text/*"]

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"

[fields.title]
source = "user_provided"
type = "string"
required = false
  [fields.title.fallback]
  source = "constant"
  value = "untitled"

[fields.slug]
source = "field"
from = "title"
transform = "slugify"
type = "string"
"#;

    #[test]
    fn ingest_text_file_without_title_uses_fallback() {
        let schema = Schema::parse(TEST_SCHEMA, "text").unwrap();
        let blobs = probe_blobs();
        let view = TreeView {
            schema: &schema,
            schema_blob: TEST_SCHEMA,
            probes: &blobs,
            limits: ProbeLimits {
                memory_mb: 64,
                fuel: 1_000_000_000,
                wall_clock_s: 10,
            },
        };
        let input = IngestInput {
            bytes: b"hello\nworld\n",
            user_fields: BTreeMap::new(),
            path: "docs/readme.md",
            ingested_at: "2026-04-22T00:00:00Z",
            streaming_builtins: None,
            content_length: 12,
            override_source_hash: None,
        };
        let m = ingest(&input, &view).unwrap();
        assert_eq!(m.fields.get("byte_size"), Some(&FieldValue::Int(12)));
        // Title fell back to the constant "untitled".
        assert_eq!(
            m.fields.get("title"),
            Some(&FieldValue::String("untitled".into()))
        );
        // Slug comes from title after fallback.
        assert_eq!(
            m.fields.get("slug"),
            Some(&FieldValue::String("untitled".into()))
        );
        // probe_hashes populated for the probe-sourced field.
        assert!(m.probe_hashes.contains_key("file.size"));
    }

    #[test]
    fn user_provided_title_beats_fallback() {
        let schema = Schema::parse(TEST_SCHEMA, "text").unwrap();
        let blobs = probe_blobs();
        let view = TreeView {
            schema: &schema,
            schema_blob: TEST_SCHEMA,
            probes: &blobs,
            limits: <ProbeLimits as DefaultProbeLimits>::default_probe_limits(),
        };
        let mut user = BTreeMap::new();
        user.insert(
            "title".to_string(),
            FieldValue::String("Hello World".into()),
        );
        let input = IngestInput {
            bytes: b"hi",
            user_fields: user,
            path: "a.md",
            ingested_at: "2026-04-22T00:00:00Z",
            streaming_builtins: None,
            content_length: 2,
            override_source_hash: None,
        };
        let m = ingest(&input, &view).unwrap();
        assert_eq!(
            m.fields.get("title"),
            Some(&FieldValue::String("Hello World".into()))
        );
        assert_eq!(
            m.fields.get("slug"),
            Some(&FieldValue::String("hello-world".into()))
        );
    }

    #[test]
    fn extra_user_field_rejected_by_default() {
        let schema = Schema::parse(TEST_SCHEMA, "text").unwrap();
        let blobs = probe_blobs();
        let view = TreeView {
            schema: &schema,
            schema_blob: TEST_SCHEMA,
            probes: &blobs,
            limits: <ProbeLimits as DefaultProbeLimits>::default_probe_limits(),
        };
        let mut user = BTreeMap::new();
        user.insert("unknown".to_string(), FieldValue::Int(1));
        let input = IngestInput {
            bytes: b"hi",
            user_fields: user,
            path: "a.md",
            ingested_at: "2026-04-22T00:00:00Z",
            streaming_builtins: None,
            content_length: 2,
            override_source_hash: None,
        };
        let err = ingest(&input, &view).unwrap_err();
        assert!(matches!(err, OmpError::IngestValidation(_)));
    }

    #[test]
    fn required_field_missing_fails() {
        let schema_bytes = br#"file_type = "text"
mime_patterns = ["text/*"]

[fields.title]
source = "user_provided"
type = "string"
required = true
"#;
        let schema = Schema::parse(schema_bytes, "text").unwrap();
        let blobs = probe_blobs();
        let view = TreeView {
            schema: &schema,
            schema_blob: schema_bytes,
            probes: &blobs,
            limits: <ProbeLimits as DefaultProbeLimits>::default_probe_limits(),
        };
        let input = IngestInput {
            bytes: b"hi",
            user_fields: BTreeMap::new(),
            path: "a.md",
            ingested_at: "2026-04-22T00:00:00Z",
            streaming_builtins: None,
            content_length: 2,
            override_source_hash: None,
        };
        let err = ingest(&input, &view).unwrap_err();
        assert!(matches!(err, OmpError::IngestValidation(_)));
    }

    #[test]
    fn detect_file_type_uses_mime_patterns() {
        use std::collections::BTreeMap;
        let text = Schema::parse(
            br#"file_type = "text"
mime_patterns = ["text/*"]
"#,
            "text",
        )
        .unwrap();
        let pdf = Schema::parse(
            br#"file_type = "pdf"
mime_patterns = ["application/pdf"]
"#,
            "pdf",
        )
        .unwrap();
        let mut schemas = BTreeMap::new();
        schemas.insert("text".to_string(), text);
        schemas.insert("pdf".to_string(), pdf);
        let got = detect_file_type(None, "text/markdown", &schemas).unwrap();
        assert_eq!(got.file_type, "text");
        let got = detect_file_type(None, "application/pdf", &schemas).unwrap();
        assert_eq!(got.file_type, "pdf");
        assert!(detect_file_type(None, "image/png", &schemas).is_none());
        // Explicit override.
        let got = detect_file_type(Some("pdf"), "text/plain", &schemas).unwrap();
        assert_eq!(got.file_type, "pdf");
    }

    trait DefaultProbeLimits {
        fn default_probe_limits() -> ProbeLimits;
    }
    impl DefaultProbeLimits for ProbeLimits {
        fn default_probe_limits() -> ProbeLimits {
            ProbeLimits {
                memory_mb: 64,
                fuel: 1_000_000_000,
                wall_clock_s: 10,
            }
        }
    }
}

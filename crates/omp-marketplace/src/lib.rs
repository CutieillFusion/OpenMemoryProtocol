//! `omp-marketplace` — public registry for community-published probes.
//!
//! See `docs/design/23-probe-marketplace.md` for the design. The crate
//! exposes an axum router and a `MarketplaceState` that holds the catalog
//! (JSON-on-disk) and the blob store (filesystem). The gateway routes
//! `/marketplace/probes*` here and owns `/marketplace/install/<id>` itself
//! (so the marketplace stays stateless about consumer tenants).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

mod blobs;
mod catalog;

use blobs::BlobStore;
use catalog::{Catalog, CatalogEntry};

mod schemas;

const MAX_PUBLISH_BYTES: usize = 50 * 1024 * 1024; // 50 MiB per publish
const DEFAULT_BUILD_WALL_CLOCK_SECS: u64 = 90;

/// In-process build configuration used when a publisher uploads source.
/// The marketplace runs cargo on its own pod (same approach as `omp-builder`)
/// rather than calling out to the builder service, so the publish flow stays
/// a single request: source in, wasm out (or 422 + log on failure).
#[derive(Clone, Debug)]
pub struct BuildSettings {
    pub probe_common_path: PathBuf,
    pub scratch_root: PathBuf,
    pub wall_clock_secs: u64,
}

impl Default for BuildSettings {
    fn default() -> Self {
        Self {
            probe_common_path: PathBuf::from("../../probes-src/probe-common"),
            scratch_root: std::env::temp_dir().join("omp-marketplace-build"),
            wall_clock_secs: DEFAULT_BUILD_WALL_CLOCK_SECS,
        }
    }
}

#[derive(Clone)]
pub struct MarketplaceState {
    inner: Arc<Inner>,
}

struct Inner {
    catalog: Mutex<Catalog>,
    schema_catalog: Mutex<schemas::SchemaCatalog>,
    blobs: BlobStore,
    /// When `Some`, publish/yank endpoints require an `X-OMP-Tenant-Context`
    /// header signed by this key (the gateway's signer). When `None`, the
    /// service is in dev/demo mode — any context is accepted. The flag
    /// flips on the presence of `--verifying-key` at startup.
    verifier: Option<VerifyingKey>,
    build: BuildSettings,
}

impl MarketplaceState {
    pub fn open(
        data_root: PathBuf,
        verifier: Option<VerifyingKey>,
        build: BuildSettings,
    ) -> Result<Self> {
        let catalog_path = data_root.join("catalog_probes.json");
        let legacy_catalog_path = data_root.join("catalog.json");
        // One-time migration: if the legacy single-catalog file exists and the
        // new probes file does not, rename it. Saves operators a manual step
        // when upgrading past the schema-marketplace split.
        if !catalog_path.exists() && legacy_catalog_path.exists() {
            std::fs::rename(&legacy_catalog_path, &catalog_path)?;
        }
        let schema_catalog_path = data_root.join("catalog_schemas.json");
        let blobs_root = data_root.join("blobs");
        let catalog = Catalog::open(&catalog_path)?;
        let schema_catalog = schemas::SchemaCatalog::open(&schema_catalog_path)?;
        let blobs = BlobStore::open(&blobs_root)?;
        std::fs::create_dir_all(&build.scratch_root)?;
        Ok(Self {
            inner: Arc::new(Inner {
                catalog: Mutex::new(catalog),
                schema_catalog: Mutex::new(schema_catalog),
                blobs,
                verifier,
                build,
            }),
        })
    }

    pub(crate) fn blobs(&self) -> &BlobStore {
        &self.inner.blobs
    }
}

pub fn router(state: MarketplaceState) -> Router {
    // Routes are mounted under `/marketplace/probes` so the gateway can
    // forward incoming `/marketplace/probes/...` requests as-is (mirroring
    // how `omp-builder` exposes `/probes/build*` because that's what the
    // gateway forwards). `/healthz` stays at root for simple liveness
    // probing without prefix knowledge.
    Router::new()
        .route("/healthz", get(healthz))
        .route("/marketplace/probes", get(list_probes).post(publish_probe))
        .route(
            "/marketplace/probes/:id",
            get(get_probe).patch(patch_probe).delete(yank_probe),
        )
        .route("/marketplace/probes/:id/blobs/:hash", get(get_blob))
        .merge(schemas::router())
        .layer(DefaultBodyLimit::max(MAX_PUBLISH_BYTES))
        .with_state(state)
}

async fn healthz() -> Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "ok": true, "service": "omp-marketplace" })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Auth helper — verify the gateway-issued TenantContext and pull `sub`.
// ---------------------------------------------------------------------------

pub(crate) fn require_authed_publisher(
    state: &MarketplaceState,
    headers: &HeaderMap,
) -> Result<(String, String), Response> {
    // (tenant_id, sub) tuple
    let raw = match headers.get(omp_tenant_ctx::HEADER_NAME) {
        Some(v) => match v.to_str() {
            Ok(s) => s,
            Err(_) => {
                return Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    "bad_context",
                    "X-OMP-Tenant-Context not utf8",
                ));
            }
        },
        None => {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "missing_context",
                "X-OMP-Tenant-Context required",
            ));
        }
    };
    let ctx = match state.inner.verifier {
        Some(ref vk) => match omp_tenant_ctx::TenantContext::verify(raw, vk) {
            Ok(c) => c,
            Err(e) => {
                return Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    "bad_context",
                    &format!("verify: {e}"),
                ));
            }
        },
        None => match omp_tenant_ctx::TenantContext::decode_unverified(raw) {
            Ok(c) => c,
            Err(e) => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "bad_context",
                    &format!("decode: {e}"),
                ));
            }
        },
    };
    let sub = match ctx.sub {
        Some(s) if !s.is_empty() => s,
        _ => {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "no_sub",
                "publishing requires a WorkOS-authenticated session (no `sub` in context)",
            ));
        }
    };
    Ok((ctx.tenant_id, sub))
}

// ---------------------------------------------------------------------------
// GET /probes — list/search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub publisher_sub: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    50
}

async fn list_probes(
    State(state): State<MarketplaceState>,
    Query(q): Query<ListQuery>,
) -> Response {
    let catalog = state.inner.catalog.lock().await;
    let mut hits: Vec<&CatalogEntry> = catalog
        .all()
        .filter(|e| e.yanked_at.is_none())
        .filter(|e| q.namespace.as_deref().map_or(true, |n| e.namespace == n))
        .filter(|e| q.name.as_deref().map_or(true, |n| e.name == n))
        .filter(|e| q.publisher_sub.as_deref().map_or(true, |s| e.publisher_sub == s))
        .filter(|e| {
            q.q.as_deref().map_or(true, |needle| {
                let n = needle.to_ascii_lowercase();
                e.description
                    .as_deref()
                    .map_or(false, |d| d.to_ascii_lowercase().contains(&n))
                    || e.name.to_ascii_lowercase().contains(&n)
                    || e.namespace.to_ascii_lowercase().contains(&n)
            })
        })
        .collect();
    // Newest first.
    hits.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    let cloned: Vec<CatalogEntry> = hits.into_iter().take(q.limit).cloned().collect();
    (StatusCode::OK, axum::Json(serde_json::json!({ "probes": cloned }))).into_response()
}

// ---------------------------------------------------------------------------
// GET /probes/:id
// ---------------------------------------------------------------------------

async fn get_probe(State(state): State<MarketplaceState>, Path(id): Path<String>) -> Response {
    let catalog = state.inner.catalog.lock().await;
    match catalog.get(&id) {
        Some(entry) if entry.yanked_at.is_none() => {
            // Best-effort manifest preview by reading the manifest blob.
            let manifest_preview = state
                .inner
                .blobs
                .get(&entry.manifest_hash)
                .ok()
                .and_then(|opt| opt)
                .and_then(|bytes| String::from_utf8(bytes).ok());
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "probe": entry,
                    "manifest_preview": manifest_preview,
                })),
            )
                .into_response()
        }
        Some(_) => error_response(StatusCode::GONE, "yanked", "probe was yanked"),
        None => error_response(StatusCode::NOT_FOUND, "not_found", "no such probe id"),
    }
}

// ---------------------------------------------------------------------------
// GET /probes/:id/blobs/:hash
// ---------------------------------------------------------------------------

async fn get_blob(
    State(state): State<MarketplaceState>,
    Path((id, hash)): Path<(String, String)>,
) -> Response {
    {
        let catalog = state.inner.catalog.lock().await;
        match catalog.get(&id) {
            Some(entry) => {
                if entry.wasm_hash != hash
                    && entry.manifest_hash != hash
                    && entry.readme_hash.as_deref() != Some(&hash)
                    && entry.source_hash.as_deref() != Some(&hash)
                {
                    return error_response(
                        StatusCode::NOT_FOUND,
                        "blob_not_part_of_probe",
                        "this hash is not one of the blobs in this probe",
                    );
                }
            }
            None => {
                return error_response(StatusCode::NOT_FOUND, "not_found", "no such probe id");
            }
        }
    }
    match state.inner.blobs.get(&hash) {
        Ok(Some(bytes)) => {
            (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/octet-stream")], bytes)
                .into_response()
        }
        Ok(None) => error_response(StatusCode::NOT_FOUND, "blob_missing", "blob not on disk"),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "blob_io", &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// POST /probes — publish
// ---------------------------------------------------------------------------

async fn publish_probe(
    State(state): State<MarketplaceState>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Response {
    let (_tenant, publisher_sub) = match require_authed_publisher(&state, &headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let mut namespace = String::new();
    let mut name = String::new();
    let mut version = String::new();
    let mut description: Option<String> = None;
    let mut manifest: Option<Vec<u8>> = None;
    let mut readme: Option<Vec<u8>> = None;
    let mut source: Option<Vec<u8>> = None;
    let mut got_wasm_field = false;

    while let Ok(Some(field)) = form.next_field().await {
        let field_name = match field.name() {
            Some(n) => n.to_string(),
            None => continue,
        };
        match field_name.as_str() {
            "namespace" => namespace = field.text().await.unwrap_or_default(),
            "name" => name = field.text().await.unwrap_or_default(),
            "version" => version = field.text().await.unwrap_or_default(),
            "description" => description = field.text().await.ok().filter(|s| !s.is_empty()),
            "manifest" => manifest = field.bytes().await.ok().map(|b| b.to_vec()),
            "readme" => readme = field.bytes().await.ok().map(|b| b.to_vec()),
            "source" => source = field.bytes().await.ok().map(|b| b.to_vec()),
            "wasm" => {
                // Drain the field body so the multipart stream advances, but
                // refuse the request: publishers may not upload binaries.
                let _ = field.bytes().await;
                got_wasm_field = true;
            }
            _ => {}
        }
    }

    if got_wasm_field {
        return error_response(
            StatusCode::BAD_REQUEST,
            "wasm_upload_forbidden",
            "publishers must upload `source` (Rust lib.rs); OMP builds the wasm server-side",
        );
    }

    if namespace.is_empty() || name.is_empty() || version.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_fields",
            "namespace, name, version are required",
        );
    }
    if !is_safe_ident(&namespace) || !is_safe_ident(&name) || !is_safe_ident(&version) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "bad_ident",
            "namespace/name/version must be [a-zA-Z0-9._-]+ and at most 64 chars each",
        );
    }
    let source = match source {
        Some(b) if !b.is_empty() => b,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "missing_source",
                "missing `source` field (Rust lib.rs)",
            )
        }
    };
    let manifest = match manifest {
        Some(b) if !b.is_empty() => b,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "missing_manifest",
                "missing `manifest` field",
            )
        }
    };

    // Defense in depth: even though the `wasm` multipart field is refused
    // outright above, a publisher could try to smuggle a `.wasm` blob in via
    // the `source` field. The wasm magic header `\0asm\x01\x00\x00\x00` is
    // valid UTF-8 (all bytes < 0x80), so the UTF-8 check below would not
    // catch it; reject it explicitly.
    if source.len() >= 4 && &source[..4] == b"\0asm" {
        return error_response(
            StatusCode::BAD_REQUEST,
            "wasm_upload_forbidden",
            "source field looks like a wasm module; upload Rust lib.rs source instead",
        );
    }
    let source_str = match std::str::from_utf8(&source) {
        Ok(s) => s.to_string(),
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "bad_source",
                "source must be UTF-8 (single Rust lib.rs file)",
            )
        }
    };
    if std::str::from_utf8(&manifest).is_err() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "bad_manifest",
            "manifest must be UTF-8 TOML",
        );
    }

    // Pre-checked early so we don't waste 30 cargo seconds on a duplicate.
    let id = entry_id(&publisher_sub, &namespace, &name, &version);
    {
        let catalog = state.inner.catalog.lock().await;
        if let Some(existing) = catalog.get(&id) {
            if existing.yanked_at.is_none() {
                return error_response(
                    StatusCode::CONFLICT,
                    "version_exists",
                    "this publisher already published this namespace/name/version",
                );
            }
        }
    }

    // Server-side build. Every publish (including version bumps) recompiles
    // from source, so the on-disk wasm always matches the published source.
    let build = &state.inner.build;
    let built = match omp_builder::build_inline(
        &build.scratch_root,
        &build.probe_common_path,
        &source_str,
        build.wall_clock_secs,
    )
    .await
    {
        Ok(ok) => ok,
        Err(err) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(serde_json::json!({
                    "error": {
                        "code": "build_failed",
                        "message": err.reason,
                        "details": { "log": err.log },
                    }
                })),
            )
                .into_response();
        }
    };
    let wasm = built.wasm;

    let wasm_hash = sha256_hex(&wasm);
    let manifest_hash = sha256_hex(&manifest);
    let readme_hash = readme.as_ref().map(|b| sha256_hex(b));
    let source_hash = Some(sha256_hex(&source));

    if let Err(e) = state.inner.blobs.put(&wasm_hash, &wasm) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "blob_put", &e.to_string());
    }
    if let Err(e) = state.inner.blobs.put(&manifest_hash, &manifest) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "blob_put", &e.to_string());
    }
    if let (Some(h), Some(b)) = (readme_hash.as_ref(), readme.as_ref()) {
        if let Err(e) = state.inner.blobs.put(h, b) {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "blob_put", &e.to_string());
        }
    }
    if let Some(h) = source_hash.as_ref() {
        if let Err(e) = state.inner.blobs.put(h, &source) {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "blob_put", &e.to_string());
        }
    }

    let now = now_unix();
    let entry = CatalogEntry {
        id: id.clone(),
        publisher_sub: publisher_sub.clone(),
        namespace: namespace.clone(),
        name: name.clone(),
        version: version.clone(),
        description,
        wasm_hash,
        manifest_hash,
        readme_hash,
        source_hash,
        published_at: now,
        yanked_at: None,
        downloads: 0,
    };

    let mut catalog = state.inner.catalog.lock().await;
    if let Err(e) = catalog.upsert(entry.clone()) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "catalog_io", &e.to_string());
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "probe": entry, "build_log": built.log })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// PATCH /probes/:id — metadata edit (publisher-only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ProbePatch {
    #[serde(default)]
    pub description: Option<String>,
    /// New README markdown body. Stored as a fresh blob; old readme blob is
    /// left in place (content-addressed; might still be referenced by
    /// historical entries).
    #[serde(default)]
    pub readme: Option<String>,
}

async fn patch_probe(
    State(state): State<MarketplaceState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(patch): axum::Json<ProbePatch>,
) -> Response {
    let (_tenant, sub) = match require_authed_publisher(&state, &headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let mut catalog = state.inner.catalog.lock().await;
    let entry = match catalog.get(&id) {
        Some(e) => e.clone(),
        None => return error_response(StatusCode::NOT_FOUND, "not_found", "no such probe id"),
    };
    if entry.publisher_sub != sub {
        return error_response(
            StatusCode::FORBIDDEN,
            "not_publisher",
            "only the original publisher can edit this probe",
        );
    }
    let mut updated = entry;
    if let Some(d) = patch.description {
        updated.description = if d.is_empty() { None } else { Some(d) };
    }
    if let Some(r) = patch.readme {
        if r.is_empty() {
            updated.readme_hash = None;
        } else {
            let bytes = r.into_bytes();
            let h = sha256_hex(&bytes);
            if let Err(e) = state.inner.blobs.put(&h, &bytes) {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "blob_put",
                    &e.to_string(),
                );
            }
            updated.readme_hash = Some(h);
        }
    }
    if let Err(e) = catalog.upsert(updated.clone()) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "catalog_io", &e.to_string());
    }
    (StatusCode::OK, axum::Json(serde_json::json!({ "probe": updated }))).into_response()
}

// ---------------------------------------------------------------------------
// DELETE /probes/:id — yank (publisher-only)
// ---------------------------------------------------------------------------

async fn yank_probe(
    State(state): State<MarketplaceState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (_tenant, sub) = match require_authed_publisher(&state, &headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let mut catalog = state.inner.catalog.lock().await;
    let entry = match catalog.get(&id) {
        Some(e) => e.clone(),
        None => return error_response(StatusCode::NOT_FOUND, "not_found", "no such probe id"),
    };
    if entry.publisher_sub != sub {
        return error_response(
            StatusCode::FORBIDDEN,
            "not_publisher",
            "only the original publisher can yank this probe",
        );
    }
    if entry.yanked_at.is_some() {
        return (StatusCode::OK, axum::Json(serde_json::json!({ "ok": true, "already_yanked": true })))
            .into_response();
    }
    let mut updated = entry;
    updated.yanked_at = Some(now_unix());
    if let Err(e) = catalog.upsert(updated) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "catalog_io", &e.to_string());
    }
    (StatusCode::OK, axum::Json(serde_json::json!({ "ok": true }))).into_response()
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub(crate) fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in h {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn entry_id(publisher_sub: &str, namespace: &str, name: &str, version: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(publisher_sub.as_bytes());
    h.update(b"\0");
    h.update(namespace.as_bytes());
    h.update(b"\0");
    h.update(name.as_bytes());
    h.update(b"\0");
    h.update(version.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub(crate) fn is_safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResponse {
    pub probe: CatalogEntry,
}

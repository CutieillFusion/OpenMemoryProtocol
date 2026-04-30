//! Schema marketplace handlers.
//!
//! Mirrors the probe marketplace (publish / list / get / patch / yank), but
//! without the build step — schemas are pure TOML data, validated by parsing
//! into a minimal struct that matches `omp-core`'s `Schema::parse` shape.
//! See `docs/design/25-schema-marketplace.md`.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::{
    extract::{Multipart, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    error_response, is_safe_ident, now_unix, require_authed_publisher, sha256_hex,
    MarketplaceState,
};

// ---------------------------------------------------------------------------
// On-disk catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaCatalogEntry {
    pub id: String,
    pub publisher_sub: String,
    pub file_type: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub schema_hash: String,
    #[serde(default)]
    pub readme_hash: Option<String>,
    pub published_at: u64,
    #[serde(default)]
    pub yanked_at: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    entries: HashMap<String, SchemaCatalogEntry>,
}

#[derive(Debug)]
pub struct SchemaCatalog {
    path: PathBuf,
    state: OnDisk,
}

impl SchemaCatalog {
    pub fn open(path: &Path) -> Result<Self> {
        let state = if path.exists() {
            let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            if bytes.is_empty() {
                OnDisk::default()
            } else {
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?
            }
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            OnDisk::default()
        };
        Ok(Self {
            path: path.to_path_buf(),
            state,
        })
    }

    fn all(&self) -> impl Iterator<Item = &SchemaCatalogEntry> {
        self.state.entries.values()
    }

    fn get(&self, id: &str) -> Option<&SchemaCatalogEntry> {
        self.state.entries.get(id)
    }

    fn upsert(&mut self, entry: SchemaCatalogEntry) -> Result<()> {
        self.state.entries.insert(entry.id.clone(), entry);
        self.flush()
    }

    fn flush(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<MarketplaceState> {
    Router::new()
        .route(
            "/marketplace/schemas",
            get(list_schemas).post(publish_schema),
        )
        .route(
            "/marketplace/schemas/:id",
            get(get_schema).patch(patch_schema).delete(yank_schema),
        )
        .route(
            "/marketplace/schemas/:id/blobs/:hash",
            get(get_schema_blob),
        )
}

// ---------------------------------------------------------------------------
// Lightweight schema validation. Avoids pulling in `omp-core` (and its
// runtime deps) just for syntax validation; we re-derive the minimum shape
// described in `docs/design/04-schemas.md`. Full validation against probe
// references happens at install time on the consumer side.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MinimalSchema {
    file_type: String,
    mime_patterns: Vec<String>,
}

fn validate_schema_toml(bytes: &[u8]) -> Result<String, String> {
    let s = std::str::from_utf8(bytes).map_err(|_| "schema must be UTF-8".to_string())?;
    let parsed: MinimalSchema =
        toml::from_str(s).map_err(|e| format!("schema TOML: {e}"))?;
    if parsed.file_type.is_empty() {
        return Err("schema: file_type must be non-empty".into());
    }
    if !is_safe_ident(&parsed.file_type) {
        return Err(
            "schema: file_type must be [a-zA-Z0-9._-]+ and at most 64 chars".into(),
        );
    }
    if parsed.mime_patterns.is_empty() {
        return Err("schema: mime_patterns must be non-empty".into());
    }
    Ok(parsed.file_type)
}

// ---------------------------------------------------------------------------
// GET /marketplace/schemas
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub file_type: Option<String>,
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

async fn list_schemas(
    State(state): State<MarketplaceState>,
    Query(q): Query<ListQuery>,
) -> Response {
    let catalog = state.inner_schema_catalog().lock().await;
    let mut hits: Vec<&SchemaCatalogEntry> = catalog
        .all()
        .filter(|e| e.yanked_at.is_none())
        .filter(|e| q.file_type.as_deref().map_or(true, |t| e.file_type == t))
        .filter(|e| {
            q.publisher_sub
                .as_deref()
                .map_or(true, |s| e.publisher_sub == s)
        })
        .filter(|e| {
            q.q.as_deref().map_or(true, |needle| {
                let n = needle.to_ascii_lowercase();
                e.file_type.to_ascii_lowercase().contains(&n)
                    || e.description
                        .as_deref()
                        .map_or(false, |d| d.to_ascii_lowercase().contains(&n))
            })
        })
        .collect();
    hits.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    let cloned: Vec<SchemaCatalogEntry> =
        hits.into_iter().take(q.limit).cloned().collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "schemas": cloned })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /marketplace/schemas/:id
// ---------------------------------------------------------------------------

async fn get_schema(
    State(state): State<MarketplaceState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let catalog = state.inner_schema_catalog().lock().await;
    match catalog.get(&id) {
        Some(entry) if entry.yanked_at.is_none() => {
            let preview = state
                .blobs()
                .get(&entry.schema_hash)
                .ok()
                .and_then(|opt| opt)
                .and_then(|bytes| String::from_utf8(bytes).ok());
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "schema": entry,
                    "schema_preview": preview,
                })),
            )
                .into_response()
        }
        Some(_) => error_response(StatusCode::GONE, "yanked", "schema was yanked"),
        None => error_response(StatusCode::NOT_FOUND, "not_found", "no such schema id"),
    }
}

// ---------------------------------------------------------------------------
// GET /marketplace/schemas/:id/blobs/:hash
// ---------------------------------------------------------------------------

async fn get_schema_blob(
    State(state): State<MarketplaceState>,
    AxumPath((id, hash)): AxumPath<(String, String)>,
) -> Response {
    {
        let catalog = state.inner_schema_catalog().lock().await;
        match catalog.get(&id) {
            Some(entry) => {
                if entry.schema_hash != hash && entry.readme_hash.as_deref() != Some(&hash) {
                    return error_response(
                        StatusCode::NOT_FOUND,
                        "blob_not_part_of_schema",
                        "this hash is not one of the blobs in this schema",
                    );
                }
            }
            None => {
                return error_response(StatusCode::NOT_FOUND, "not_found", "no such schema id");
            }
        }
    }
    match state.blobs().get(&hash) {
        Ok(Some(bytes)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "blob_missing", "blob not on disk"),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "blob_io",
            &e.to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// POST /marketplace/schemas
// ---------------------------------------------------------------------------

async fn publish_schema(
    State(state): State<MarketplaceState>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Response {
    let (_tenant, publisher_sub) = match require_authed_publisher(&state, &headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let mut version = String::new();
    let mut description: Option<String> = None;
    let mut schema: Option<Vec<u8>> = None;
    let mut readme: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = form.next_field().await {
        let field_name = match field.name() {
            Some(n) => n.to_string(),
            None => continue,
        };
        match field_name.as_str() {
            "version" => version = field.text().await.unwrap_or_default(),
            "description" => description = field.text().await.ok().filter(|s| !s.is_empty()),
            "schema" => schema = field.bytes().await.ok().map(|b| b.to_vec()),
            "readme" => readme = field.bytes().await.ok().map(|b| b.to_vec()),
            _ => {}
        }
    }

    if version.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_fields",
            "version is required",
        );
    }
    if !is_safe_ident(&version) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "bad_ident",
            "version must be [a-zA-Z0-9._-]+ and at most 64 chars",
        );
    }
    let schema = match schema {
        Some(b) if !b.is_empty() => b,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "missing_schema",
                "missing `schema` field (schema.toml body)",
            )
        }
    };
    let file_type = match validate_schema_toml(&schema) {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, "bad_schema", &e),
    };

    let id = schema_entry_id(&publisher_sub, &file_type, &version);
    {
        let catalog = state.inner_schema_catalog().lock().await;
        if let Some(existing) = catalog.get(&id) {
            if existing.yanked_at.is_none() {
                return error_response(
                    StatusCode::CONFLICT,
                    "version_exists",
                    "this publisher already published this file_type/version",
                );
            }
        }
    }

    let schema_hash = sha256_hex(&schema);
    let readme_hash = readme.as_ref().map(|b| sha256_hex(b));

    if let Err(e) = state.blobs().put(&schema_hash, &schema) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "blob_put", &e.to_string());
    }
    if let (Some(h), Some(b)) = (readme_hash.as_ref(), readme.as_ref()) {
        if let Err(e) = state.blobs().put(h, b) {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "blob_put", &e.to_string());
        }
    }

    let entry = SchemaCatalogEntry {
        id: id.clone(),
        publisher_sub,
        file_type,
        version,
        description,
        schema_hash,
        readme_hash,
        published_at: now_unix(),
        yanked_at: None,
    };

    let mut catalog = state.inner_schema_catalog().lock().await;
    if let Err(e) = catalog.upsert(entry.clone()) {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_io",
            &e.to_string(),
        );
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "schema": entry })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// PATCH /marketplace/schemas/:id
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SchemaPatch {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub readme: Option<String>,
}

async fn patch_schema(
    State(state): State<MarketplaceState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    axum::Json(patch): axum::Json<SchemaPatch>,
) -> Response {
    let (_tenant, sub) = match require_authed_publisher(&state, &headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let mut catalog = state.inner_schema_catalog().lock().await;
    let entry = match catalog.get(&id) {
        Some(e) => e.clone(),
        None => {
            return error_response(StatusCode::NOT_FOUND, "not_found", "no such schema id");
        }
    };
    if entry.publisher_sub != sub {
        return error_response(
            StatusCode::FORBIDDEN,
            "not_publisher",
            "only the original publisher can edit this schema",
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
            if let Err(e) = state.blobs().put(&h, &bytes) {
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
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_io",
            &e.to_string(),
        );
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "schema": updated })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// DELETE /marketplace/schemas/:id (yank)
// ---------------------------------------------------------------------------

async fn yank_schema(
    State(state): State<MarketplaceState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let (_tenant, sub) = match require_authed_publisher(&state, &headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let mut catalog = state.inner_schema_catalog().lock().await;
    let entry = match catalog.get(&id) {
        Some(e) => e.clone(),
        None => {
            return error_response(StatusCode::NOT_FOUND, "not_found", "no such schema id");
        }
    };
    if entry.publisher_sub != sub {
        return error_response(
            StatusCode::FORBIDDEN,
            "not_publisher",
            "only the original publisher can yank this schema",
        );
    }
    if entry.yanked_at.is_some() {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "ok": true, "already_yanked": true })),
        )
            .into_response();
    }
    let mut updated = entry;
    updated.yanked_at = Some(now_unix());
    if let Err(e) = catalog.upsert(updated) {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_io",
            &e.to_string(),
        );
    }
    (StatusCode::OK, axum::Json(serde_json::json!({ "ok": true }))).into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn schema_entry_id(publisher_sub: &str, file_type: &str, version: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(publisher_sub.as_bytes());
    h.update(b"\0");
    h.update(b"schema\0");
    h.update(file_type.as_bytes());
    h.update(b"\0");
    h.update(version.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// Internal accessor — keeps the marketplace state's mutex private to the
// crate while letting the schema module reach the schema catalog.
impl MarketplaceState {
    pub(crate) fn inner_schema_catalog(&self) -> &tokio::sync::Mutex<SchemaCatalog> {
        &self.inner.schema_catalog
    }
}

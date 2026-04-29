//! Axum routes exposing `omp_core::api`. Every handler:
//! 1. Calls `auth::resolve` to pin the request to one tenant's `Repo`.
//! 2. Translates request → api call → JSON response.
//!
//! Multi-tenancy is structurally enforced: nothing in this module takes a
//! tenant id as an argument. A handler only sees the `Repo` it was handed
//! by the middleware.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use omp_core::api::{AuthorOverride, Fields, ShowResult};
use omp_core::manifest::FieldValue;
use omp_core::OmpError;

use crate::auth::{self, TenantRepo};
use crate::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    crate::metrics::init();
    Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(crate::health::livez))
        .route("/readyz", get(crate::health::readyz))
        .route("/startupz", get(crate::health::startupz))
        .route("/metrics", get(crate::health::metrics))
        .route("/status", get(status))
        .route("/files", get(list_files).post(post_file))
        .route(
            "/files/*path",
            get(get_file).patch(patch_fields).delete(delete_file),
        )
        .route("/bytes/*path", get(get_bytes))
        .route("/tree", get(tree_root))
        .route("/tree/*path", get(tree_path))
        .route("/commit", post(commit_route))
        .route("/log", get(log_route))
        .route("/diff", get(diff_route))
        .route("/branches", get(list_branches).post(create_branch))
        .route("/checkout", post(checkout_route))
        .route("/test/ingest", post(test_ingest))
        .route("/query", get(query_route))
        .route("/schemas", get(list_schemas_route))
        .route("/audit", get(audit_route))
        .route("/watch", get(watch_route))
        // Resumable upload sessions (docs/design/12-large-files.md).
        .route("/uploads", post(post_upload))
        .route(
            "/uploads/:id",
            axum::routing::patch(patch_upload_chunk).delete(delete_upload),
        )
        .route("/uploads/:id/commit", post(post_upload_commit))
        .with_state(state)
        .layer(axum::middleware::from_fn(crate::metrics::record_request))
}

// --- responses ---

fn to_response(err: OmpError) -> Response {
    let code = err.code();
    let status =
        StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = json!({
        "error": {
            "code": code.as_str(),
            "message": err.to_string(),
        }
    });
    (status, Json(body)).into_response()
}

fn ok_json<T: Serialize>(v: T) -> Response {
    (
        StatusCode::OK,
        Json(serde_json::to_value(v).unwrap_or(serde_json::Value::Null)),
    )
        .into_response()
}

// ---- compact-view helpers --------------------------------------------------
//
// The LLM interacting with OMP does not benefit from content-addressable
// hashes at the default JSON surface — they're 64-char hex strings that burn
// tokens and don't help the model decide anything. We strip them on read
// endpoints unless the caller passes `?verbose=true`.
//
// Commit hashes stay in /log (they're needed for `?at=<hash>` time-travel).

fn strip_keys(v: &mut serde_json::Value, keys: &[&str]) {
    if let Some(obj) = v.as_object_mut() {
        for k in keys {
            obj.remove(*k);
        }
    }
}

fn compact_manifest(v: &mut serde_json::Value) {
    strip_keys(
        v,
        &[
            "source_hash",
            "schema_hash",
            "probe_hashes",
            "ingester_version",
        ],
    );
}

fn compact_tree_entries(v: &mut serde_json::Value) {
    if let Some(arr) = v.as_array_mut() {
        for item in arr {
            strip_keys(item, &["hash"]);
        }
    }
}

fn compact_file_list(v: &mut serde_json::Value) {
    if let Some(arr) = v.as_array_mut() {
        for item in arr {
            strip_keys(item, &["manifest_hash", "source_hash"]);
        }
    }
}

fn compact_commit_list(v: &mut serde_json::Value) {
    if let Some(arr) = v.as_array_mut() {
        for item in arr {
            strip_keys(item, &["tree", "parents"]);
        }
    }
}

fn ok_compact<T: Serialize>(v: T, compact: impl FnOnce(&mut serde_json::Value)) -> Response {
    let mut val = serde_json::to_value(v).unwrap_or(serde_json::Value::Null);
    compact(&mut val);
    (StatusCode::OK, Json(val)).into_response()
}

async fn resolve_repo(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<TenantRepo, Response> {
    auth::resolve(state, headers).await.map_err(to_response)
}

/// Reject requests from encrypted tenants that try to hit a server-side
/// ingest / validation path. Returns `Ok(())` for plaintext tenants and
/// for endpoints that don't depend on plaintext.
fn require_plaintext_mode(tr: &TenantRepo, endpoint: &str) -> std::result::Result<(), Response> {
    if tr.encryption_mode.is_encrypted() {
        return Err(to_response(OmpError::EncryptionModeMismatch(format!(
            "endpoint {endpoint} expects plaintext objects; this tenant is encrypted — perform ingest client-side and upload ciphertext objects directly"
        ))));
    }
    Ok(())
}

// --- handlers ---

async fn healthz() -> Response {
    ok_json(json!({ "ok": true }))
}

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.status() {
        Ok(s) => ok_json(s),
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize, Default)]
struct ListFilesQuery {
    at: Option<String>,
    prefix: Option<String>,
    /// Include `manifest_hash` and `source_hash` per entry. Default off.
    #[serde(default)]
    verbose: bool,
}

async fn list_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ListFilesQuery>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.files(q.at.as_deref(), q.prefix.as_deref()) {
        Ok(list) => {
            if q.verbose {
                ok_json(list)
            } else {
                ok_compact(list, compact_file_list)
            }
        }
        Err(e) => to_response(e),
    }
}

async fn post_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    if let Err(r) = require_plaintext_mode(&tr, "POST /files") {
        return r;
    }
    let mut path: Option<String> = None;
    let mut file_bytes: Option<Bytes> = None;
    let mut file_type: Option<String> = None;
    let mut fields: Fields = BTreeMap::new();
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "path" => path = Some(field.text().await.unwrap_or_default()),
            "file" => file_bytes = Some(field.bytes().await.unwrap_or_default()),
            "file_type" => file_type = Some(field.text().await.unwrap_or_default()),
            n if n.starts_with("fields[") && n.ends_with(']') => {
                let key = &n[7..n.len() - 1];
                let value = field.text().await.unwrap_or_default();
                fields.insert(key.to_string(), parse_scalar(&value));
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    let Some(path) = path else {
        return to_response(OmpError::InvalidPath("missing path".into()));
    };
    let Some(bytes) = file_bytes else {
        return to_response(OmpError::InvalidPath("missing file".into()));
    };
    match tr
        .repo
        .add(&path, &bytes, Some(fields), file_type.as_deref())
    {
        Ok(res) => ok_json(res),
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize, Default)]
struct AtQuery {
    at: Option<String>,
    #[serde(default)]
    recursive: bool,
    /// Include provenance hashes (source_hash, schema_hash, probe_hashes,
    /// ingester_version for manifests; the entry hash for tree listings).
    /// Default off — these are useful for replay and audit, but noisy in
    /// day-to-day LLM browsing.
    #[serde(default)]
    verbose: bool,
    /// Read from the staging index instead of the committed tree. Lets the
    /// UI render a file that was uploaded or installed-from-marketplace
    /// but not yet committed. `at` is ignored when `staged=true`.
    #[serde(default)]
    staged: bool,
}

async fn get_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    Query(q): Query<AtQuery>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let result = if q.staged {
        tr.repo.show_staged(&path)
    } else {
        tr.repo.show(&path, q.at.as_deref())
    };
    match result {
        Ok(ShowResult::Manifest {
            manifest, render, ..
        }) => {
            // Inline `render` as a sibling of the manifest's serialized
            // fields so the response stays a single flat object — the UI
            // currently treats the body as a Manifest and just sees one
            // extra top-level key.
            let mut value = serde_json::to_value(manifest).unwrap_or(serde_json::Value::Null);
            if !q.verbose {
                compact_manifest(&mut value);
            }
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "render".to_string(),
                    serde_json::to_value(render).unwrap_or(serde_json::Value::Null),
                );
            }
            (StatusCode::OK, Json(value)).into_response()
        }
        Ok(ShowResult::Blob {
            blob_hash,
            size,
            render,
            ..
        }) => {
            // Blob responses are hash-centric by design (the caller asked for
            // a blob-addressable object) — always include.
            ok_json(json!({ "kind": "blob", "hash": blob_hash, "size": size, "render": render }))
        }
        Ok(ShowResult::Tree { entries, .. }) => {
            if q.verbose {
                ok_json(entries)
            } else {
                ok_compact(entries, compact_tree_entries)
            }
        }
        Err(e) => to_response(e),
    }
}

async fn get_bytes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    Query(q): Query<AtQuery>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let result = if q.staged {
        tr.repo.bytes_of_staged(&path)
    } else {
        tr.repo.bytes_of(&path, q.at.as_deref())
    };
    match result {
        Ok(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => to_response(e),
    }
}

async fn patch_fields(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    Json(body): Json<BTreeMap<String, serde_json::Value>>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    if let Err(r) = require_plaintext_mode(&tr, "PATCH /files/*path") {
        return r;
    };
    let mut updates: Fields = BTreeMap::new();
    for (k, v) in body {
        updates.insert(k, json_to_field(&v));
    }
    match tr.repo.patch_fields(&path, updates) {
        Ok(m) => ok_json(m),
        Err(e) => to_response(e),
    }
}

async fn delete_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.remove(&path) {
        Ok(()) => ok_json(json!({"ok": true})),
        Err(e) => to_response(e),
    }
}

async fn tree_root(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AtQuery>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let result = if q.staged {
        tr.repo.ls_staged()
    } else {
        tr.repo.ls("", q.at.as_deref(), q.recursive)
    };
    match result {
        Ok(entries) => {
            if q.verbose {
                ok_json(entries)
            } else {
                ok_compact(entries, compact_tree_entries)
            }
        }
        Err(e) => to_response(e),
    }
}

async fn tree_path(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    Query(q): Query<AtQuery>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let result = if q.staged {
        // Staged listing ignores subpath; the index is path-flat.
        tr.repo.ls_staged()
    } else {
        tr.repo.ls(&path, q.at.as_deref(), q.recursive)
    };
    match result {
        Ok(entries) => {
            if q.verbose {
                ok_json(entries)
            } else {
                ok_compact(entries, compact_tree_entries)
            }
        }
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize)]
struct CommitBody {
    message: String,
    #[serde(default)]
    author: Option<CommitAuthor>,
}

#[derive(Deserialize)]
struct CommitAuthor {
    name: Option<String>,
    email: Option<String>,
    timestamp: Option<String>,
}

async fn commit_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CommitBody>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let override_ = body.author.map(|a| AuthorOverride {
        name: a.name,
        email: a.email,
        timestamp: a.timestamp,
    });
    let tenant_id = tr.repo.tenant().as_str().to_string();
    match tr.repo.commit_with_summary(&body.message, override_) {
        Ok((h, reprobed)) => {
            // Publish commit.created on the event bus. Per doc 16, this fires
            // *after* the commit is durable in the store. Best-effort: a
            // failed publish logs WARN but does not surface to the client.
            let bus = state.events.clone();
            let payload = omp_events::payload::CommitCreated {
                branch: omp_core::refs::current_branch(tr.repo.store())
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                commit_hash: h.hex(),
                parent_hashes: vec![],
                paths_touched: vec![],
            };
            if let Ok(env) = omp_events::envelope_for(
                omp_events::event_type::COMMIT_CREATED,
                &tenant_id,
                None,
                None,
                &payload,
            ) {
                tokio::spawn(async move {
                    if let Err(e) = bus.publish(env).await {
                        tracing::warn!(error = %e, "publish commit.created failed");
                    }
                });
            }
            // Reprobe summary, when present (see doc 21). Empty array
            // when the commit didn't touch any schemas — clients should
            // treat absence as "nothing to report".
            let mut body = json!({ "hash": h });
            if !reprobed.is_empty() {
                body["reprobed"] = serde_json::to_value(&reprobed).unwrap_or(json!(null));
            }
            ok_json(body)
        }
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize, Default)]
struct LogQuery {
    path: Option<String>,
    #[serde(default = "default_log_max")]
    max: usize,
    /// Include `tree` and `parents` hashes per entry. Default off.
    /// Commit `hash` stays in the response regardless — callers need it for
    /// `?at=<hash>` time-travel queries.
    #[serde(default)]
    verbose: bool,
}

fn default_log_max() -> usize {
    50
}

async fn log_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<LogQuery>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.log_commits(q.path.as_deref(), q.max) {
        Ok(log) => {
            if q.verbose {
                ok_json(log)
            } else {
                ok_compact(log, compact_commit_list)
            }
        }
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize)]
struct DiffQuery {
    from: String,
    to: String,
    #[serde(default)]
    path: Option<String>,
}

async fn diff_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<DiffQuery>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.diff(&q.from, &q.to, q.path.as_deref()) {
        Ok(d) => ok_json(d),
        Err(e) => to_response(e),
    }
}

async fn list_branches(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.list_branches() {
        Ok(b) => ok_json(b),
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize)]
struct CreateBranchBody {
    name: String,
    #[serde(default)]
    start: Option<String>,
}

async fn create_branch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(b): Json<CreateBranchBody>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.branch(&b.name, b.start.as_deref()) {
        Ok(()) => ok_json(json!({"ok": true})),
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize)]
struct CheckoutBody {
    #[serde(rename = "ref")]
    ref_: String,
}

async fn checkout_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(b): Json<CheckoutBody>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.checkout(&b.ref_) {
        Ok(()) => ok_json(json!({"ok": true})),
        Err(e) => to_response(e),
    }
}

// ---- /query (docs/design/15-query-and-discovery.md) -----------------------

#[derive(Deserialize, Default)]
struct QueryParams {
    /// Predicate expression. Optional — absence means "match everything".
    #[serde(default)]
    r#where: Option<String>,
    /// Restrict the walk to paths starting with this prefix.
    #[serde(default)]
    prefix: Option<String>,
    /// Time-travel anchor: any ref expression. Defaults to HEAD.
    #[serde(default)]
    at: Option<String>,
    /// Opaque cursor from a previous response.
    #[serde(default)]
    cursor: Option<String>,
    /// Page size. Clamped to [1, 1000]; default 100.
    #[serde(default = "default_query_limit")]
    limit: usize,
}

fn default_query_limit() -> usize {
    100
}

async fn query_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<QueryParams>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let parsed = match q.r#where.as_deref() {
        Some(s) if !s.is_empty() => match omp_core::query::parse(s) {
            Ok(e) => Some(e),
            Err(err) => {
                let body = json!({
                    "error": {
                        "code": "bad_query",
                        "message": err.to_string(),
                    }
                });
                return (StatusCode::BAD_REQUEST, Json(body)).into_response();
            }
        },
        _ => None,
    };
    match tr.repo.query(
        q.at.as_deref(),
        q.prefix.as_deref(),
        parsed.as_ref(),
        q.cursor.as_deref(),
        q.limit,
    ) {
        Ok(r) => ok_json(r),
        Err(e) => to_response(e),
    }
}

// ---- /schemas (drives web-UI query autocomplete) -------------------------
//
// Lists schema summaries at the given ref (default HEAD). Returns just the
// fields/types the autocompleter needs — no probe wiring or fallback shape.

#[derive(Deserialize, Default)]
struct SchemasParams {
    #[serde(default)]
    at: Option<String>,
}

async fn list_schemas_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<SchemasParams>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.list_schemas(q.at.as_deref()) {
        Ok(s) => ok_json(s),
        Err(e) => to_response(e),
    }
}

// ---- /watch (docs/design/15-query-and-discovery.md § Watch) ----------------
//
// SSE endpoint that projects events from the event bus to the caller. This
// makes "the change feed is the broker projection" literally true (see
// `docs/design/15-query-and-discovery.md` and `16-event-streaming.md`).

async fn watch_route(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    use axum::response::sse::{Event, Sse};
    use futures::StreamExt;

    let mut sub = state.events.subscribe();
    let stream = async_stream::stream! {
        while let Some(env) = sub.next().await {
            let payload = serde_json::json!({
                "type": env.r#type,
                "tenant": env.tenant,
                "occurred_at": env.occurred_at,
                "trace_id": env.trace_id,
            });
            yield Ok::<_, std::convert::Infallible>(
                Event::default().event(env.r#type.clone()).data(payload.to_string())
            );
        }
    };
    Sse::new(stream.boxed()).keep_alive(axum::response::sse::KeepAlive::default())
}

// ---- /audit (docs/design/18-observability.md § Audit log) -----------------

#[derive(Deserialize, Default)]
struct AuditQuery {
    /// Maximum number of entries to return. Default 100.
    #[serde(default = "default_audit_limit")]
    limit: usize,
}

fn default_audit_limit() -> usize {
    100
}

async fn audit_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let limit = q.limit.clamp(1, 1000);
    match omp_core::audit::read_chain_verified(tr.repo.store(), limit) {
        Ok((entries, verified)) => ok_json(json!({
            "entries": entries,
            "verified": verified,
        })),
        Err(e) => to_response(e),
    }
}

async fn test_ingest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    if let Err(r) = require_plaintext_mode(&tr, "POST /test/ingest") {
        return r;
    }
    let mut path: Option<String> = None;
    let mut file_bytes: Option<Bytes> = None;
    let mut proposed_schema: Option<Bytes> = None;
    let mut fields: Fields = BTreeMap::new();
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "path" => path = Some(field.text().await.unwrap_or_default()),
            "file" => file_bytes = Some(field.bytes().await.unwrap_or_default()),
            "proposed_schema" => proposed_schema = Some(field.bytes().await.unwrap_or_default()),
            n if n.starts_with("fields[") && n.ends_with(']') => {
                let key = &n[7..n.len() - 1];
                let value = field.text().await.unwrap_or_default();
                fields.insert(key.to_string(), parse_scalar(&value));
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    let Some(path) = path else {
        return to_response(OmpError::InvalidPath("missing path".into()));
    };
    let Some(bytes) = file_bytes else {
        return to_response(OmpError::InvalidPath("missing file".into()));
    };
    match tr
        .repo
        .test_ingest(&path, &bytes, Some(fields), proposed_schema.as_deref())
    {
        Ok(m) => ok_json(m),
        Err(e) => to_response(e),
    }
}

// --- helpers ---

fn parse_scalar(v: &str) -> FieldValue {
    if let Ok(i) = v.parse::<i64>() {
        return FieldValue::Int(i);
    }
    if let Ok(f) = v.parse::<f64>() {
        return FieldValue::Float(f);
    }
    if v == "true" {
        return FieldValue::Bool(true);
    }
    if v == "false" {
        return FieldValue::Bool(false);
    }
    FieldValue::String(v.to_string())
}

fn json_to_field(v: &serde_json::Value) -> FieldValue {
    match v {
        serde_json::Value::Null => FieldValue::Null,
        serde_json::Value::Bool(b) => FieldValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                FieldValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                FieldValue::Float(f)
            } else {
                FieldValue::Null
            }
        }
        serde_json::Value::String(s) => FieldValue::String(s.clone()),
        serde_json::Value::Array(arr) => FieldValue::List(arr.iter().map(json_to_field).collect()),
        serde_json::Value::Object(map) => {
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in map {
                out.insert(k.clone(), json_to_field(v));
            }
            FieldValue::Object(out)
        }
    }
}

// ---- Upload-session routes (docs/design/12-large-files.md) -----------------

#[derive(Deserialize)]
struct OpenUploadBody {
    declared_size: u64,
}

async fn post_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<OpenUploadBody>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    if let Err(r) = require_plaintext_mode(&tr, "POST /uploads") {
        return r;
    }
    match tr.repo.upload_open(body.declared_size) {
        Ok(h) => (StatusCode::CREATED, Json(serde_json::to_value(h).unwrap())).into_response(),
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize, Default)]
struct UploadChunkQuery {
    offset: u64,
}

async fn patch_upload_chunk(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<UploadChunkQuery>,
    body: Bytes,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.upload_write(&id, q.offset, &body) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => to_response(e),
    }
}

#[derive(Deserialize)]
struct CommitUploadBody {
    path: String,
    #[serde(default)]
    file_type: Option<String>,
    #[serde(default)]
    fields: Option<serde_json::Map<String, serde_json::Value>>,
}

async fn post_upload_commit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CommitUploadBody>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    if let Err(r) = require_plaintext_mode(&tr, "POST /uploads/:id/commit") {
        return r;
    }
    let fields = body.fields.map(|m| {
        let mut out: Fields = BTreeMap::new();
        for (k, v) in m {
            out.insert(k, json_to_field(&v));
        }
        out
    });
    match tr
        .repo
        .upload_commit(&id, &body.path, fields, body.file_type.as_deref())
    {
        Ok(r) => ok_json(r),
        Err(e) => to_response(e),
    }
}

async fn delete_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let tr = match resolve_repo(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    match tr.repo.upload_abort(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => to_response(e),
    }
}

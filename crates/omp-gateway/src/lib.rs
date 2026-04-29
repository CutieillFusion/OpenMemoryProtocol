//! `omp-gateway` — HTTP edge service that fronts a fleet of `omp-server` shard
//! backends.
//!
//! Per `docs/design/14-microservice-decomposition.md`:
//! - Auth lives here. Bearer tokens map to tenant ids via the registry.
//! - Tenant routing lives here. A consistent hash on tenant id picks one of N
//!   backend shards.
//! - The gateway issues a short-lived `TenantContext` signed with its Ed25519
//!   key and forwards it as `X-OMP-Tenant-Context` for downstream observability
//!   and (future) verification.
//!
//! The gateway is the only service exposed externally. Internal shards are
//! reached only through it.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Router,
};
use omp_core::registry::hash_token;
use omp_tenant_ctx::GatewaySigner;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod auth;
#[cfg(feature = "embed-ui")]
mod ui;

static TENANT_CTX_HEADER_NAME: HeaderName = HeaderName::from_static(omp_tenant_ctx::HEADER_NAME);

const HOP_BY_HOP_HEADERS: &[&str] = &["connection", "transfer-encoding", "content-length"];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    /// Backend shard URLs (e.g. `["http://shard-0:8000", "http://shard-1:8000"]`).
    pub shards: Vec<String>,
    /// Tenants known to this gateway. Maps a sha256-of-token (hex) → tenant id.
    /// Tokens themselves are never stored.
    #[serde(default)]
    pub tokens: HashMap<String, String>,
    /// If true, also accept `Authorization: Bearer dev-<tenant>` for any tenant.
    /// Convenient for local development; never enable in production.
    #[serde(default)]
    pub allow_dev_tokens: bool,
    /// Optional `omp-builder` URL. When set, requests under `/probes/build*`
    /// route there instead of to a shard. When unset, those routes return
    /// `503 builder_unavailable`. See `docs/design/20-server-side-probes.md`.
    #[serde(default)]
    pub builder: Option<String>,
    /// Optional `omp-marketplace` URL. Same pattern as `builder`. When set,
    /// `/marketplace/probes*` routes there; `/marketplace/install/<id>` is a
    /// gateway-owned endpoint that pulls blobs from the marketplace and
    /// stages them on the caller's shard. When unset, `/marketplace/*`
    /// returns `503 marketplace_unavailable`. See
    /// `docs/design/23-probe-marketplace.md`.
    #[serde(default)]
    pub marketplace: Option<String>,
    /// Optional WorkOS / generic-OIDC config. When `None`, the gateway
    /// behaves identically to the pre-WorkOS design — only Bearer tokens are
    /// resolved, no `/auth/*` routes are mounted. See
    /// `docs/design/22-workos-auth.md`.
    #[serde(default)]
    pub workos: Option<auth::WorkOsConfig>,
}

impl GatewayConfig {
    pub fn from_toml_path(p: &std::path::Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(p)?;
        Ok(toml::from_str(&s)?)
    }
}

/// Resolved auth identity for a request. `tenant` is mandatory; `sub` is
/// the WorkOS user id when the request was authenticated via the session
/// cookie path, and `None` for Bearer token / machine clients.
#[derive(Debug, Clone)]
pub struct Principal {
    pub tenant: String,
    pub sub: Option<String>,
}

#[derive(Clone)]
pub struct GatewayState {
    pub config: Arc<GatewayConfig>,
    pub signer: Arc<GatewaySigner>,
    pub client: reqwest::Client,
    /// Discovered OIDC endpoints + the WorkOS config block. `None` when the
    /// feature is off (matches `config.workos.is_none()`).
    pub oidc: Option<Arc<auth::OidcRuntime>>,
}

impl GatewayState {
    pub fn new(config: GatewayConfig, signer: GatewaySigner) -> Self {
        Self::new_with_oidc(config, signer, None)
    }

    pub fn new_with_oidc(
        config: GatewayConfig,
        signer: GatewaySigner,
        oidc: Option<auth::OidcRuntime>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        Self {
            config: Arc::new(config),
            signer: Arc::new(signer),
            client,
            oidc: oidc.map(Arc::new),
        }
    }

    /// Resolve the request to a tenant id. Cookie path runs first; Bearer
    /// path runs second so a stale token never overrides a live session
    /// (per `docs/design/22-workos-auth.md` §`resolve_tenant`).
    pub fn resolve_tenant(&self, headers: &HeaderMap) -> Option<String> {
        self.resolve_principal(headers).map(|p| p.tenant)
    }

    /// Resolve to the full principal: tenant id + optional WorkOS user id.
    /// `sub` is `Some` only on the cookie path (WorkOS); Bearer machine
    /// clients have no associated user identity so it stays `None`.
    pub fn resolve_principal(&self, headers: &HeaderMap) -> Option<Principal> {
        if let Some(claims) = auth::resolve_session_cookie(headers, &self.signer.verifying_key()) {
            return Some(Principal {
                tenant: claims.tenant,
                sub: Some(claims.sub),
            });
        }
        self.resolve_bearer_token(headers).map(|tenant| Principal {
            tenant,
            sub: None,
        })
    }

    /// Resolve a Bearer token to a tenant id, if recognized.
    pub fn resolve_bearer_token(&self, headers: &HeaderMap) -> Option<String> {
        let auth = headers
            .get(axum::http::header::AUTHORIZATION)?
            .to_str()
            .ok()?;
        let token = auth.strip_prefix("Bearer ")?.trim();

        if self.config.allow_dev_tokens {
            if let Some(t) = token.strip_prefix("dev-") {
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }

        self.config.tokens.get(&hash_token(token)).cloned()
    }

    /// What the frontend's `/status` probe needs to render the right gate.
    /// The gateway always fronts bearer-auth-required shards unless WorkOS is
    /// on, so the choice collapses to two values. ("no-auth" only makes sense
    /// when the shard is reached directly without a gateway in front, which
    /// is the embedded single-tenant case the frontend handles via a 200
    /// fallthrough at the proxy layer.)
    pub fn auth_mode_label(&self) -> &'static str {
        if self.oidc.is_some() {
            "workos"
        } else {
            "token"
        }
    }

    /// Pick a shard for a tenant by sha256(tenant) mod N.
    pub fn shard_for(&self, tenant: &str) -> Option<&str> {
        if self.config.shards.is_empty() {
            return None;
        }
        let mut hasher = Sha256::new();
        hasher.update(tenant.as_bytes());
        let h = hasher.finalize();
        let bucket = u64::from_be_bytes(h[..8].try_into().unwrap_or([0; 8]));
        let idx = (bucket as usize) % self.config.shards.len();
        Some(self.config.shards[idx].as_str())
    }
}

pub fn router(state: GatewayState) -> Router {
    let mut r = Router::new()
        .route("/healthz", any(healthz))
        .route("/status", any(status_handler));

    // OIDC routes only when WorkOS is configured. `/auth/logout` accepts
    // both GET and POST so the frontend can use a top-level form post and
    // the browser can follow the redirect chain through `end_session_endpoint`.
    if state.oidc.is_some() {
        r = r
            .route("/auth/login", get(auth::login))
            .route("/auth/callback", get(auth::callback))
            .route("/auth/refresh", get(auth::refresh))
            .route("/auth/me", get(auth::me))
            .route("/auth/logout", get(auth::logout).post(auth::logout));
    }

    // Marketplace install endpoint: gateway-owned (not proxied), so the
    // marketplace stays stateless about consumer tenants. Pulls blobs from
    // the marketplace and stages them on the caller's shard via the
    // existing /files multipart path. See doc 23.
    if state.config.marketplace.is_some() {
        r = r.route("/marketplace/install/:id", post(install_from_marketplace));
    }

    // Mount the embedded SvelteKit UI at `/`, `/ui`, and `/ui/*`. These
    // routes win over the proxy fallback because they're explicit. API
    // endpoints (everything under root that isn't `/healthz`, `/`, or
    // `/ui*`) flow through `proxy` exactly as before.
    #[cfg(feature = "embed-ui")]
    let r = r.merge(ui::router::<GatewayState>());

    r.fallback(any(proxy)).with_state(state)
}

/// `/status` has two callers with two needs:
///   1. The frontend's `probeAuth` (unauthed): wants to know `auth_mode` so
///      it can render the right gate, without needing a token first.
///   2. Authenticated app code: wants the shard's `RepoStatus` JSON.
///
/// Resolution: if the request is unauthed, answer locally with `{ok,
/// auth_mode}` in WorkOS mode (the probe contract) or 401 in token mode
/// (preserves the pre-WorkOS contract: "401 means a token is required"). If
/// the request is authed, fall through to the same proxy code path as every
/// other API endpoint, returning the shard's `RepoStatus`.
async fn status_handler(State(state): State<GatewayState>, req: Request<Body>) -> Response {
    let tenant = state.resolve_tenant(req.headers());
    if tenant.is_none() {
        return match state.auth_mode_label() {
            "workos" => (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "ok": true,
                    "auth_mode": "workos",
                })),
            )
                .into_response(),
            _ => error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or unknown bearer token",
            ),
        };
    }
    proxy(State(state), req).await
}

async fn healthz() -> Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "ok": true,
            "service": "omp-gateway"
        })),
    )
        .into_response()
}

async fn proxy(State(state): State<GatewayState>, req: Request<Body>) -> Response {
    // 1. Auth.
    let principal = match state.resolve_principal(req.headers()) {
        Some(p) => p,
        None => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or unknown bearer token",
            );
        }
    };
    let tenant = principal.tenant.clone();

    // 2. Pick the upstream by path prefix. `/probes/build*` lands on the
    //    `omp-builder` service (per doc 20); `/marketplace/probes*` lands on
    //    `omp-marketplace` (per doc 23); everything else flows to the
    //    tenant's shard exactly as before.
    let path_for_routing = req.uri().path();
    let upstream_base = if path_for_routing.starts_with("/probes/build") {
        match state.config.builder.as_deref() {
            Some(url) => url.to_string(),
            None => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "builder_unavailable",
                    "no omp-builder configured for this gateway",
                );
            }
        }
    } else if path_for_routing.starts_with("/marketplace/probes") {
        match state.config.marketplace.as_deref() {
            Some(url) => url.to_string(),
            None => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "marketplace_unavailable",
                    "no omp-marketplace configured for this gateway",
                );
            }
        }
    } else {
        match state.shard_for(&tenant) {
            Some(s) => s.to_string(),
            None => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no_shards",
                    "no backend shards configured",
                );
            }
        }
    };

    // 3. Issue a signed tenant context. Carry `sub` so `omp-marketplace`
    //    (and any other downstream that records actor identity) can record
    //    the authenticated WorkOS user without trusting client-side data.
    let ctx = match state
        .signer
        .issue_default_with_sub(&tenant, Vec::new(), principal.sub.clone())
    {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ctx_sign",
                &e.to_string(),
            );
        }
    };
    let ctx_b64 = match ctx.encode() {
        Ok(s) => s,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ctx_encode",
                &e.to_string(),
            );
        }
    };

    // 4. Build the upstream URL.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let upstream_url = format!("{}{}", upstream_base.trim_end_matches('/'), path_and_query);

    // 5. Forward.
    let method = req.method().clone();
    let mut headers = req.headers().clone();
    if let Ok(val) = HeaderValue::from_str(&ctx_b64) {
        headers.insert(TENANT_CTX_HEADER_NAME.clone(), val);
    }
    headers.remove(axum::http::header::HOST);
    headers.remove(axum::http::header::CONNECTION);
    headers.remove(axum::http::header::TRANSFER_ENCODING);
    headers.remove(axum::http::header::UPGRADE);

    let body = match axum::body::to_bytes(req.into_body(), 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, "body_read", &e.to_string());
        }
    };

    let mut req_builder = state.client.request(method.clone(), &upstream_url);
    for (k, v) in headers.iter() {
        req_builder = req_builder.header(k.as_str(), v.as_bytes());
    }
    req_builder = req_builder.body(body.to_vec());

    let upstream_resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                &e.to_string(),
            );
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();

    // SSE responses (`/watch`, per docs/design/16-event-streaming.md) must be
    // forwarded as a stream, not buffered — buffering would hold every event
    // until the upstream closes, which is forever.
    let is_sse = resp_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_start().starts_with("text/event-stream"))
        .unwrap_or(false);

    // Translate internal 412 → external 409 conflict (per doc 14 §Idempotency
    // and ref CAS). Backend uses 412 for If-Match precondition failure;
    // the public error vocabulary is `409 conflict`.
    let final_status = if status == StatusCode::PRECONDITION_FAILED {
        StatusCode::CONFLICT
    } else {
        status
    };
    let mut out = Response::builder().status(final_status);
    if let Some(map) = out.headers_mut() {
        for (k, v) in resp_headers.iter() {
            if HOP_BY_HOP_HEADERS.contains(&k.as_str()) {
                continue;
            }
            if let Ok(value) = HeaderValue::from_bytes(v.as_bytes()) {
                map.insert(k.clone(), value);
            }
        }
        if is_sse {
            map.insert(
                HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            );
            map.insert(
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            );
        }
    }

    let body = if is_sse {
        Body::from_stream(upstream_resp.bytes_stream())
    } else {
        let resp_bytes = match upstream_resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return error_response(StatusCode::BAD_GATEWAY, "upstream_body", &e.to_string());
            }
        };
        Body::from(resp_bytes)
    };

    out.body(body).unwrap_or_else(|_| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "build_response",
            "failed to build response",
        )
    })
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": {
                "code": code,
                "message": message,
            }
        })),
    )
        .into_response()
}

/// `POST /marketplace/install/:id` — gateway-owned per
/// `docs/design/23-probe-marketplace.md`. Resolves the caller's tenant,
/// fetches the catalog entry from the marketplace, downloads each blob,
/// and stages them under `probes/<ns>/<name>/{probe.wasm,probe.toml,README.md}`
/// on the caller's shard via a multipart POST to `/files`. Returns the
/// staged manifest hashes; the user clicks Commit on the existing Commit
/// page to make it durable.
async fn install_from_marketplace(
    State(state): State<GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    let principal = match state.resolve_principal(&headers) {
        Some(p) => p,
        None => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or unknown bearer token",
            );
        }
    };
    let tenant = principal.tenant.clone();
    let marketplace = match state.config.marketplace.as_deref() {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "marketplace_unavailable",
                "no omp-marketplace configured for this gateway",
            );
        }
    };
    let shard = match state.shard_for(&tenant) {
        Some(s) => s.trim_end_matches('/').to_string(),
        None => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_shards",
                "no backend shards configured",
            );
        }
    };

    // 1. Fetch catalog entry.
    let probe: serde_json::Value = match state
        .client
        .get(format!("{marketplace}/marketplace/probes/{id}"))
        .send()
        .await
    {
        Ok(resp) => match resp.error_for_status() {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(e) => {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "marketplace_decode",
                        &e.to_string(),
                    );
                }
            },
            Err(e) => {
                let code = if e.status().map(|s| s.as_u16()) == Some(404) {
                    "not_found"
                } else {
                    "marketplace_status"
                };
                return error_response(StatusCode::BAD_GATEWAY, code, &e.to_string());
            }
        },
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "marketplace_unreachable",
                &e.to_string(),
            );
        }
    };

    let entry = match probe.get("probe") {
        Some(v) => v,
        None => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "marketplace_shape",
                "marketplace did not return {probe: ...}",
            );
        }
    };
    let namespace = entry
        .get("namespace")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let wasm_hash = entry.get("wasm_hash").and_then(|v| v.as_str()).unwrap_or("");
    let manifest_hash = entry
        .get("manifest_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let readme_hash = entry
        .get("readme_hash")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    if namespace.is_empty() || name.is_empty() || wasm_hash.is_empty() || manifest_hash.is_empty() {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "marketplace_shape",
            "incomplete catalog entry",
        );
    }

    // Build a TenantContext for shard ingest.
    let ctx = match state
        .signer
        .issue_default_with_sub(&tenant, Vec::new(), principal.sub.clone())
    {
        Ok(c) => c,
        Err(e) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "ctx_sign", &e.to_string());
        }
    };
    let ctx_b64 = match ctx.encode() {
        Ok(s) => s,
        Err(e) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "ctx_encode", &e.to_string());
        }
    };

    // 2. For each blob, fetch from marketplace and stage on shard.
    let mut staged: Vec<serde_json::Value> = Vec::new();
    let mut blobs: Vec<(&str, &str)> = vec![("probe.wasm", wasm_hash), ("probe.toml", manifest_hash)];
    if let Some(h) = readme_hash {
        blobs.push(("README.md", h));
    }

    for (filename, hash) in blobs {
        let blob_url = format!("{marketplace}/marketplace/probes/{id}/blobs/{hash}");
        let bytes = match state.client.get(&blob_url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(r) => match r.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        return error_response(
                            StatusCode::BAD_GATEWAY,
                            "blob_read",
                            &e.to_string(),
                        );
                    }
                },
                Err(e) => {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "blob_status",
                        &e.to_string(),
                    );
                }
            },
            Err(e) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "blob_unreachable",
                    &e.to_string(),
                );
            }
        };
        let staged_path = format!("probes/{namespace}/{name}/{filename}");
        let form = reqwest::multipart::Form::new()
            .text("path", staged_path.clone())
            .part(
                "file",
                reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string()),
            );
        let resp = state
            .client
            .post(format!("{shard}/files"))
            .header(TENANT_CTX_HEADER_NAME.as_str(), ctx_b64.clone())
            .multipart(form)
            .send()
            .await;
        match resp {
            Ok(r) => {
                let status = r.status();
                if !status.is_success() {
                    let body = r.text().await.unwrap_or_default();
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "shard_stage",
                        &format!("staging {staged_path} got {status}: {body}"),
                    );
                }
                if let Ok(json) = r.json::<serde_json::Value>().await {
                    staged.push(json);
                }
            }
            Err(e) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "shard_unreachable",
                    &e.to_string(),
                );
            }
        }
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "ok": true,
            "namespace": namespace,
            "name": name,
            "staged": staged,
        })),
    )
        .into_response()
}

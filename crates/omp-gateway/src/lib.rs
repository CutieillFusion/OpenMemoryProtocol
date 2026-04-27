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
    routing::any,
    Router,
};
use omp_core::registry::hash_token;
use omp_tenant_ctx::GatewaySigner;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
}

impl GatewayConfig {
    pub fn from_toml_path(p: &std::path::Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(p)?;
        Ok(toml::from_str(&s)?)
    }
}

#[derive(Clone)]
pub struct GatewayState {
    pub config: Arc<GatewayConfig>,
    pub signer: Arc<GatewaySigner>,
    pub client: reqwest::Client,
}

impl GatewayState {
    pub fn new(config: GatewayConfig, signer: GatewaySigner) -> Self {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        Self {
            config: Arc::new(config),
            signer: Arc::new(signer),
            client,
        }
    }

    /// Resolve a Bearer token to a tenant id, if recognized.
    pub fn resolve_tenant(&self, headers: &HeaderMap) -> Option<String> {
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
    let r = Router::new().route("/healthz", any(healthz));

    // Mount the embedded SvelteKit UI at `/`, `/ui`, and `/ui/*`. These
    // routes win over the proxy fallback because they're explicit. API
    // endpoints (everything under root that isn't `/healthz`, `/`, or
    // `/ui*`) flow through `proxy` exactly as before.
    #[cfg(feature = "embed-ui")]
    let r = r.merge(ui::router::<GatewayState>());

    r.fallback(any(proxy)).with_state(state)
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
    let tenant = match state.resolve_tenant(req.headers()) {
        Some(t) => t,
        None => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or unknown bearer token",
            );
        }
    };

    // 2. Pick the upstream by path prefix. `/probes/build*` lands on the
    //    `omp-builder` service (per doc 20); everything else flows to the
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

    // 3. Issue a signed tenant context.
    let ctx = match state.signer.issue_default(&tenant, Vec::new()) {
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

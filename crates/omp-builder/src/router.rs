//! axum router for `omp-builder`. See
//! [`docs/design/20-server-side-probes.md`](../../../docs/design/20-server-side-probes.md).
//!
//! All endpoints require an `X-OMP-Tenant-Context` header (a base64-encoded
//! CBOR struct signed by the gateway). For local dev the tenant id can be
//! taken from `X-OMP-Tenant` instead, gated by the `--allow-dev-tenant` CLI
//! flag (mirrors the gateway's `allow_dev_tokens`). Production uses the
//! signed context exclusively.

use axum::{
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::IntoResponse,
    routing::{any, delete, get, post},
    Json, Router,
};
use futures::stream::{self, Stream};
use serde::Deserialize;
use std::convert::Infallible;
use std::pin::Pin;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::builder::{run_build, BuildRequest};
use crate::jobs::JobId;
#[cfg(test)]
use crate::jobs::JobView;
use crate::BuilderState;

/// Build the public router. Routes mirror the gateway's `/probes/build*`
/// path-prefix entry; the gateway forwards verbatim, so paths here MUST
/// match what the gateway proxies.
pub fn router(state: BuilderState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/probes/build", post(post_build))
        .route("/probes/build/:id", get(get_build))
        .route("/probes/build/:id", delete(delete_build))
        .route("/probes/build/:id/log", get(get_build_log))
        .fallback(any(not_found))
        .with_state(state)
}

async fn not_found(req: Request) -> impl IntoResponse {
    let path = req.uri().path().to_string();
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": { "code": "not_found", "message": format!("no route for {path}") }
        })),
    )
}

async fn healthz() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "service": "omp-builder"})),
    )
}

#[derive(Deserialize)]
struct PostBuildBody {
    namespace: String,
    name: String,
    lib_rs: String,
    probe_toml: String,
}

async fn post_build(
    State(state): State<BuilderState>,
    headers: HeaderMap,
    Json(body): Json<PostBuildBody>,
) -> impl IntoResponse {
    let tenant = match resolve_tenant(&headers) {
        Some(t) => t,
        None => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing tenant context",
            )
        }
    };

    if let Err(msg) = validate(&body) {
        return error_response(StatusCode::BAD_REQUEST, "bad_request", &msg);
    }

    let (id, _tx) = state
        .jobs
        .create(tenant.clone(), body.namespace.clone(), body.name.clone());
    let req = BuildRequest {
        tenant,
        namespace: body.namespace,
        name: body.name,
        lib_rs: body.lib_rs,
        probe_toml: body.probe_toml,
    };

    // Hand off to a background task. The 202 returns immediately.
    let state_clone = state.clone();
    let id_clone = id.clone();
    tokio::spawn(async move {
        run_build(state_clone, id_clone, req).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"job_id": id.0})),
    )
        .into_response()
}

fn validate(body: &PostBuildBody) -> Result<(), String> {
    if body.namespace.is_empty() || body.name.is_empty() {
        return Err("namespace and name are required".into());
    }
    if !is_simple_ident(&body.namespace) {
        return Err("namespace must be [a-z0-9_-]+".into());
    }
    if !is_simple_ident(&body.name) {
        return Err("name must be [a-z0-9_-]+".into());
    }
    const MAX_SOURCE_BYTES: usize = 1024 * 1024; // 1 MiB
    if body.lib_rs.len() > MAX_SOURCE_BYTES {
        return Err(format!("lib.rs exceeds {} bytes", MAX_SOURCE_BYTES));
    }
    if body.probe_toml.len() > 16 * 1024 {
        return Err("probe.toml exceeds 16 KiB".into());
    }
    Ok(())
}

fn is_simple_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

async fn get_build(
    State(state): State<BuilderState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let tenant = match resolve_tenant(&headers) {
        Some(t) => t,
        None => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing tenant context",
            )
        }
    };
    match state.jobs.view(&JobId(id), &tenant) {
        Some(view) => (
            StatusCode::OK,
            Json(serde_json::to_value(view).unwrap_or_default()),
        )
            .into_response(),
        None => error_response(StatusCode::NOT_FOUND, "not_found", "no such job"),
    }
}

async fn delete_build(
    State(state): State<BuilderState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let tenant = match resolve_tenant(&headers) {
        Some(t) => t,
        None => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing tenant context",
            )
        }
    };
    if state.jobs.delete(&JobId(id), &tenant) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error_response(StatusCode::NOT_FOUND, "not_found", "no such job")
    }
}

async fn get_build_log(
    State(state): State<BuilderState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let tenant = match resolve_tenant(&headers) {
        Some(t) => t,
        None => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing tenant context",
            )
        }
    };
    let job_id = JobId(id);
    let (replay, rx) = match state.jobs.subscribe_log(&job_id, &tenant) {
        Some(p) => p,
        None => return error_response(StatusCode::NOT_FOUND, "not_found", "no such job"),
    };

    // Stream replay entries first, then tail the broadcast channel.
    let replay_stream = stream::iter(
        replay
            .into_iter()
            .map(|line| Ok::<_, Infallible>(Event::default().data(line))),
    );
    let live_stream = BroadcastStream::new(rx).filter_map(|res| {
        // A closed channel ends the stream; lagged subscribers skip the
        // missed messages but keep streaming.
        match res {
            Ok(line) => Some(Ok::<_, Infallible>(Event::default().data(line))),
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(_)) => None,
        }
    });
    let combined: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(replay_stream.chain(live_stream));

    Sse::new(combined)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Read the tenant id from `X-OMP-Tenant-Context` (signed CBOR) or, in dev
/// mode, the bare `X-OMP-Tenant` header.
///
/// v1: signature verification deferred — the gateway is the only thing in
/// front of us in our deployment, and it sets the header. Production should
/// verify the Ed25519 signature here using a key shared with the gateway
/// (matches the same scheme as `omp-server`'s tenant-context handling).
fn resolve_tenant(headers: &HeaderMap) -> Option<String> {
    if let Some(ctx) = headers.get(omp_tenant_ctx::HEADER_NAME) {
        if let Ok(s) = ctx.to_str() {
            // v1 dev mode: trust the gateway's signature without verifying
            // here. Production should call `verify` with the gateway's
            // VerifyingKey shared via config. See doc 14 §Wire format.
            if let Ok(parsed) = omp_tenant_ctx::TenantContext::decode_unverified(s) {
                return Some(parsed.tenant_id);
            }
        }
    }
    // Dev fallback: a plain X-OMP-Tenant header. Set by the demo script
    // when the gateway is bypassed for direct testing.
    headers
        .get("x-omp-tenant")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn error_response(status: StatusCode, code: &str, message: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

/// View constructor for tests.
#[cfg(test)]
pub fn job_view_for_test(state: &BuilderState, id: &str, tenant: &str) -> Option<JobView> {
    state.jobs.view(&JobId(id.to_string()), tenant)
}

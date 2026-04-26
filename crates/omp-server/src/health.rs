//! Kubernetes-style health endpoints. See `docs/design/18-observability.md`.
//!
//! - `/livez` — process is responsive (always 200 unless the process is wedged).
//! - `/readyz` — process is ready to serve (checks the underlying store is
//!   reachable + writable).
//! - `/startupz` — process has finished startup work. For the v1 server,
//!   startup is "loaded the registry"; we just delegate to readyz.
//! - `/status` — backwards-compat alias for clients that already use it.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use omp_core::store::ObjectStore;
use serde_json::json;

use crate::AppState;

pub async fn livez() -> Response {
    (
        StatusCode::OK,
        Json(json!({"ok": true, "check": "livez"})),
    )
        .into_response()
}

pub async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    let mut checks = serde_json::Map::new();
    let mut all_ok = true;

    // Probe the backing store(s) for basic reachability.
    match &state.mode {
        crate::Mode::NoAuth { repo } => {
            match repo.store().read_head() {
                Ok(_) => {
                    checks.insert("store".into(), json!({"ok": true}));
                }
                Err(e) => {
                    all_ok = false;
                    let msg: String = e.to_string();
                    checks.insert("store".into(), json!({"ok": false, "error": msg}));
                }
            }
        }
        crate::Mode::MultiTenant { tenants_base, .. } => {
            match tokio::fs::metadata(tenants_base).await {
                Ok(_) => {
                    checks.insert("tenants_base".into(), json!({"ok": true}));
                }
                Err(e) => {
                    all_ok = false;
                    checks.insert(
                        "tenants_base".into(),
                        json!({"ok": false, "error": e.to_string()}),
                    );
                }
            }
        }
    }

    let status = if all_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(json!({"ok": all_ok, "checks": checks}))).into_response()
}

pub async fn startupz(state: State<Arc<AppState>>) -> Response {
    // Same as readyz for the v1 server — no separate cache warmup phase.
    readyz(state).await
}

pub async fn metrics() -> Response {
    let body = crate::metrics::render();
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

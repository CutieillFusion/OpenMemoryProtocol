//! Integration tests for the observability surface (docs/design/18-observability.md):
//! /metrics, /livez, /readyz, /startupz, /audit.

use std::sync::Arc;
use std::time::Duration;

use omp_core::api::Repo;
use omp_core::audit::{append, AuditEntry, AuditValue};
use omp_server::{routes, AppState};
use tempfile::TempDir;
use tokio::net::TcpListener;

async fn spawn(td_path: std::path::PathBuf) -> (Arc<Repo>, String) {
    let repo = Arc::new(Repo::init(&td_path).unwrap());
    let state = Arc::new(AppState::single(repo.clone()));
    let app = routes::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    (repo, format!("http://{addr}"))
}

#[tokio::test]
async fn livez_returns_200() {
    let td = TempDir::new().unwrap();
    let (_repo, base) = spawn(td.path().to_path_buf()).await;
    let resp = reqwest::get(format!("{base}/livez")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"].as_bool(), Some(true));
}

#[tokio::test]
async fn readyz_returns_200_with_store_check() {
    let td = TempDir::new().unwrap();
    let (_repo, base) = spawn(td.path().to_path_buf()).await;
    let resp = reqwest::get(format!("{base}/readyz")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"].as_bool(), Some(true));
    assert_eq!(body["checks"]["store"]["ok"].as_bool(), Some(true));
}

#[tokio::test]
async fn startupz_returns_200() {
    let td = TempDir::new().unwrap();
    let (_repo, base) = spawn(td.path().to_path_buf()).await;
    let resp = reqwest::get(format!("{base}/startupz")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn metrics_endpoint_emits_prometheus_format() {
    let td = TempDir::new().unwrap();
    let (_repo, base) = spawn(td.path().to_path_buf()).await;

    // Generate some requests to populate counters/histograms.
    for _ in 0..3 {
        reqwest::get(format!("{base}/livez")).await.unwrap();
    }

    let resp = reqwest::get(format!("{base}/metrics")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/plain"),
        "expected prometheus text/plain content-type, got {ct}"
    );
    let body = resp.text().await.unwrap();
    // Standard Prometheus format includes # HELP / # TYPE lines.
    assert!(body.contains("# TYPE"), "metrics missing # TYPE: {body}");
    // Our request counter should be there.
    assert!(
        body.contains("omp_request_total"),
        "metrics missing omp_request_total: {body}"
    );
    // And the histogram.
    assert!(
        body.contains("omp_request_duration_seconds"),
        "metrics missing omp_request_duration_seconds: {body}"
    );
}

#[tokio::test]
async fn audit_endpoint_returns_chain() {
    let td = TempDir::new().unwrap();
    let (repo, base) = spawn(td.path().to_path_buf()).await;

    // Seed some entries via the audit module directly. (Wiring audit into
    // commit/auth handlers is a follow-up; this test exercises the wire
    // surface.)
    append(
        repo.store(),
        AuditEntry::new("alice", "auth.token.accepted", "tok-abc")
            .with_detail("ip", AuditValue::String("127.0.0.1".into())),
    )
    .unwrap();
    append(
        repo.store(),
        AuditEntry::new("alice", "commit.created", "alice")
            .with_detail("paths_touched", AuditValue::Int(3)),
    )
    .unwrap();
    append(
        repo.store(),
        AuditEntry::new("alice", "quota.warning", "system")
            .with_detail("usage_pct", AuditValue::Int(85)),
    )
    .unwrap();

    let body: serde_json::Value = reqwest::get(format!("{base}/audit"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["verified"].as_bool(), Some(true));
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 3);
    // Newest first.
    assert_eq!(entries[0]["event"].as_str(), Some("quota.warning"));
    assert_eq!(entries[1]["event"].as_str(), Some("commit.created"));
    assert_eq!(entries[2]["event"].as_str(), Some("auth.token.accepted"));
    // Genesis entry has no parent.
    assert!(entries[2]["parent"].is_null());
}

#[tokio::test]
async fn audit_limit_truncates() {
    let td = TempDir::new().unwrap();
    let (repo, base) = spawn(td.path().to_path_buf()).await;
    for i in 0..7 {
        append(
            repo.store(),
            AuditEntry::new("alice", "test", &format!("actor-{i}")),
        )
        .unwrap();
    }
    let body: serde_json::Value = reqwest::get(format!("{base}/audit?limit=3"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
}

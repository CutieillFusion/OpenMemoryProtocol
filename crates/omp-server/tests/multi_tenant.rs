//! Multi-tenancy integration tests:
//! - Bearer-token auth gates routes (401 without a valid token).
//! - Two tenants have isolated storage (alice can't see bob's files).
//! - Over-quota returns 429 quota_exceeded.

use std::sync::Arc;
use std::time::Duration;

use omp_core::registry::{default_registry_path, Quotas, TenantRegistry};
use omp_core::tenant::TenantId;
use omp_server::{routes, AppState};
use tempfile::TempDir;
use tokio::net::TcpListener;

async fn spawn_multi(
    td: &TempDir,
    tenants_base: &std::path::Path,
) -> (String, Vec<(TenantId, String)>) {
    // Seed the registry with alice and bob.
    let registry_path = default_registry_path(tenants_base);
    std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    let mut reg = TenantRegistry::default();
    let alice_tok = reg
        .create(TenantId::new("alice").unwrap(), Quotas::unlimited())
        .unwrap();
    let bob_tok = reg
        .create(TenantId::new("bob").unwrap(), Quotas::unlimited())
        .unwrap();
    reg.save(&registry_path).unwrap();
    let _ = td; // keep td alive as long as callers hold it

    let state = AppState::multi(tenants_base.to_path_buf(), registry_path).unwrap();
    let app = routes::router(Arc::new(state));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (
        format!("http://{addr}"),
        vec![
            (TenantId::new("alice").unwrap(), alice_tok),
            (TenantId::new("bob").unwrap(), bob_tok),
        ],
    )
}

async fn post_multipart(
    url: &str,
    token: Option<&str>,
    parts: Vec<(&str, &[u8], Option<&str>)>,
) -> reqwest::Response {
    let mut form = reqwest::multipart::Form::new();
    for (name, bytes, text) in parts {
        if let Some(s) = text {
            form = form.text(name.to_string(), s.to_string());
        } else {
            form = form.part(
                name.to_string(),
                reqwest::multipart::Part::bytes(bytes.to_vec())
                    .file_name("f")
                    .mime_str("application/octet-stream")
                    .unwrap(),
            );
        }
    }
    let mut req = reqwest::Client::new().post(url).multipart(form);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn auth_required_without_token() {
    let td = TempDir::new().unwrap();
    let tb = td.path().join("tenants");
    let (base, _tokens) = spawn_multi(&td, &tb).await;

    // No token — expect 401.
    let res = reqwest::Client::new()
        .get(format!("{base}/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unauthorized");

    // Garbage token — still 401.
    let res = reqwest::Client::new()
        .get(format!("{base}/status"))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn tenants_are_isolated() {
    let td = TempDir::new().unwrap();
    let tb = td.path().join("tenants");
    let (base, tokens) = spawn_multi(&td, &tb).await;
    let alice = tokens
        .iter()
        .find(|(id, _)| id.as_str() == "alice")
        .map(|(_, t)| t.clone())
        .unwrap();
    let bob = tokens
        .iter()
        .find(|(id, _)| id.as_str() == "bob")
        .map(|(_, t)| t.clone())
        .unwrap();

    // Alice uploads a file.
    let res = post_multipart(
        &format!("{base}/files"),
        Some(&alice),
        vec![
            ("path", b"alice-secret.md", Some("alice-secret.md")),
            ("file", b"alice is here", None),
            ("file_type", b"text", Some("text")),
        ],
    )
    .await;
    assert!(
        res.status().is_success(),
        "alice upload failed: {} {:?}",
        res.status(),
        res.text().await
    );

    // Bob lists files — his namespace should be empty.
    let bob_files: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/files"))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bob_files.as_array().map(|a| a.len()), Some(0));

    // Alice can see her file on disk via /files.
    let alice_files: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/files"))
        .bearer_auth(&alice)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Not yet committed, so /files at HEAD is still empty — but the staging
    // appeared in /status.
    let alice_status: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/status"))
        .bearer_auth(&alice)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let staged = alice_status["staged"].as_array().expect("staged array");
    assert!(
        staged
            .iter()
            .any(|e| e["path"] == "alice-secret.md"),
        "alice's staged entry missing: {alice_status}"
    );
    assert!(alice_files.is_array());

    // Bob's status has no staged alice.
    let bob_status: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/status"))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let bob_staged = bob_status["staged"].as_array().unwrap();
    assert!(
        !bob_staged.iter().any(|e| e["path"] == "alice-secret.md"),
        "bob should not see alice's staged entry: {bob_status}"
    );

    // Each tenant got its own directory on disk.
    assert!(tb.join("alice/.omp").is_dir(), "alice repo missing");
    assert!(tb.join("bob/.omp").is_dir(), "bob repo missing");
}

#[tokio::test]
async fn quota_exceeded_returns_429() {
    let td = TempDir::new().unwrap();
    let tb = td.path().join("tenants");
    let registry_path = default_registry_path(&tb);
    std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    let mut reg = TenantRegistry::default();
    let tok = reg
        .create(
            TenantId::new("tiny").unwrap(),
            Quotas {
                bytes: Some(1024),
                ..Quotas::unlimited()
            },
        )
        .unwrap();
    reg.save(&registry_path).unwrap();

    let state = AppState::multi(tb.clone(), registry_path).unwrap();
    let app = routes::router(Arc::new(state));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // First upload pushes the repo's own init materials past 1 KB on disk.
    // The quota check fires at some point; we keep uploading until it does.
    let url = format!("http://{addr}/files");
    let mut saw_429 = false;
    for i in 0..20 {
        let big = vec![b'x'; 1024];
        let path = format!("big-{i}.md");
        let res = post_multipart(
            &url,
            Some(&tok),
            vec![
                ("path", path.as_bytes(), Some(&path)),
                ("file", &big, None),
                ("file_type", b"text", Some("text")),
            ],
        )
        .await;
        if res.status() == 429 {
            let body: serde_json::Value = res.json().await.unwrap();
            assert_eq!(body["error"]["code"], "quota_exceeded");
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "expected a 429 quota_exceeded before 20 uploads");
}

//! HTTP-layer dispatch tests for encrypted tenants.
//!
//! See `docs/design/13-end-to-end-encryption.md §The ingest pipeline moves
//! to the client`: server-side ingest endpoints must refuse to run when
//! the tenant's registry entry declares `Encrypted` mode.

use std::sync::Arc;
use std::time::Duration;

use omp_core::registry::{default_registry_path, Quotas, TenantRegistry};
use omp_core::tenant::TenantId;
use omp_server::{routes, AppState};
use tempfile::TempDir;
use tokio::net::TcpListener;

async fn spawn_encrypted_tenant(td: &TempDir) -> (String, String) {
    let tenants_base = td.path().join("tenants");
    let registry_path = default_registry_path(&tenants_base);
    std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    let mut reg = TenantRegistry::default();
    // The pub key would normally come from the client's first-time setup.
    // For this test any 32-byte value works — the server never uses it
    // beyond storing it in the registry.
    let identity_pub = [0xabu8; 32];
    let token = reg
        .create_encrypted(
            TenantId::new("alice").unwrap(),
            Quotas::unlimited(),
            identity_pub,
        )
        .unwrap();
    reg.save(&registry_path).unwrap();

    let state = AppState::multi(tenants_base.clone(), registry_path).unwrap();
    let app = routes::router(Arc::new(state));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (format!("http://{addr}"), token)
}

async fn spawn_plaintext_tenant(td: &TempDir) -> (String, String) {
    let tenants_base = td.path().join("tenants");
    let registry_path = default_registry_path(&tenants_base);
    std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    let mut reg = TenantRegistry::default();
    let token = reg
        .create(TenantId::new("alice").unwrap(), Quotas::unlimited())
        .unwrap();
    reg.save(&registry_path).unwrap();

    let state = AppState::multi(tenants_base.clone(), registry_path).unwrap();
    let app = routes::router(Arc::new(state));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (format!("http://{addr}"), token)
}

#[tokio::test]
async fn encrypted_tenant_rejects_post_files() {
    let td = TempDir::new().unwrap();
    let (base, token) = spawn_encrypted_tenant(&td).await;

    // Try the server-side ingest path.
    let form = reqwest::multipart::Form::new()
        .text("path", "a.txt")
        .text("file_type", "text")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"plaintext".to_vec())
                .file_name("f")
                .mime_str("text/plain")
                .unwrap(),
        );
    let r = reqwest::Client::new()
        .post(format!("{base}/files"))
        .bearer_auth(&token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "encrypted tenant must reject POST /files");
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "encryption_mode_mismatch");
}

#[tokio::test]
async fn encrypted_tenant_rejects_test_ingest() {
    let td = TempDir::new().unwrap();
    let (base, token) = spawn_encrypted_tenant(&td).await;

    let form = reqwest::multipart::Form::new()
        .text("path", "a.txt")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"x".to_vec())
                .file_name("f")
                .mime_str("text/plain")
                .unwrap(),
        );
    let r = reqwest::Client::new()
        .post(format!("{base}/test/ingest"))
        .bearer_auth(&token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
}

#[tokio::test]
async fn encrypted_tenant_rejects_upload_session() {
    // Encrypted tenants can't use upload sessions at all: the commit step
    // would run server-side ingest, which an encrypted tenant bypasses by
    // construction. Gate fails fast at `POST /uploads` rather than letting
    // the client discover the mismatch at commit time.
    let td = TempDir::new().unwrap();
    let (base, token) = spawn_encrypted_tenant(&td).await;
    let client = reqwest::Client::new();
    let r = client
        .post(format!("{base}/uploads"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "declared_size": 4 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "encryption_mode_mismatch");
}

#[tokio::test]
async fn plaintext_tenant_post_files_still_works() {
    // Negative regression: the dispatch guard must not break plaintext mode.
    let td = TempDir::new().unwrap();
    let (base, token) = spawn_plaintext_tenant(&td).await;

    let form = reqwest::multipart::Form::new()
        .text("path", "a.txt")
        .text("file_type", "text")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"hello\n".to_vec())
                .file_name("f")
                .mime_str("text/plain")
                .unwrap(),
        );
    let r = reqwest::Client::new()
        .post(format!("{base}/files"))
        .bearer_auth(&token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "plaintext flow got {}", r.status());
}

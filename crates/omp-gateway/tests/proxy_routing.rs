//! End-to-end test of the gateway's tenant-based routing.
//!
//! Spins up two backend `omp-server` shards (single-tenant `--no-auth` mode,
//! each with its own temp repo) and one `omp-gateway` configured to route
//! between them based on a sha256(tenant_id) hash. Verifies:
//!  - Request authenticated with tenant `alice`'s token reaches one shard.
//!  - Request authenticated with tenant `bob`'s token reaches the *other* shard.
//!  - Forwarded request includes a signed `X-OMP-Tenant-Context` header.
//!  - Unauthorized request (missing/wrong token) is rejected at the gateway.
//!  - The 412→409 status translation kicks in when the upstream responds with
//!    `412 Precondition Failed`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{routing::get, Router};
use omp_core::api::Repo;
use omp_core::registry::hash_token;
use omp_gateway::{router as gateway_router, GatewayConfig, GatewayState};
use omp_server::{routes, AppState};
use omp_tenant_ctx::{GatewaySigner, TenantContext};
use tempfile::TempDir;
use tokio::net::TcpListener;

async fn spawn_omp_server(td_path: std::path::PathBuf) -> String {
    let repo = Repo::init(&td_path).unwrap();
    let state = Arc::new(AppState::single(Arc::new(repo)));
    let app = routes::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn routes_alice_and_bob_to_different_shards() {
    let _ = tracing_subscriber::fmt::try_init();

    let td_a = TempDir::new().unwrap();
    let td_b = TempDir::new().unwrap();
    let shard_a = spawn_omp_server(td_a.path().to_path_buf()).await;
    let shard_b = spawn_omp_server(td_b.path().to_path_buf()).await;

    // Build the token map.
    let mut tokens = HashMap::new();
    tokens.insert(hash_token("alice-token"), "alice".to_string());
    tokens.insert(hash_token("bob-token"), "bob".to_string());

    let cfg = GatewayConfig {
        shards: vec![shard_a.clone(), shard_b.clone()],
        tokens,
        allow_dev_tokens: false,
    };
    let signer = GatewaySigner::generate();
    let verifying = signer.verifying_key();
    let state = GatewayState::new(cfg, signer);

    // Pre-compute which shard each tenant SHOULD go to (so the test asserts
    // the *implementation*, not just *some* routing).
    let alice_shard = state.shard_for("alice").unwrap().to_string();
    let bob_shard = state.shard_for("bob").unwrap().to_string();
    assert_ne!(
        alice_shard, bob_shard,
        "test invariant: alice and bob should hash to distinct shards"
    );

    // Stand up the gateway.
    let app = gateway_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = reqwest::Client::new();
    let gw_url = format!("http://{gw_addr}");

    // Add a unique file to alice's expected shard via the gateway.
    let alice_form = reqwest::multipart::Form::new()
        .text("path", "alice-only.txt")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"hi from alice".to_vec())
                .file_name("alice-only.txt"),
        );
    let resp = client
        .post(format!("{gw_url}/files"))
        .header("Authorization", "Bearer alice-token")
        .multipart(alice_form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "alice POST should succeed");
    let resp = client
        .post(format!("{gw_url}/commit"))
        .header("Authorization", "Bearer alice-token")
        .json(&serde_json::json!({"message": "alice add"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "alice commit should succeed");

    // Same for bob, distinct path.
    let bob_form = reqwest::multipart::Form::new()
        .text("path", "bob-only.txt")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"hi from bob".to_vec()).file_name("bob-only.txt"),
        );
    let resp = client
        .post(format!("{gw_url}/files"))
        .header("Authorization", "Bearer bob-token")
        .multipart(bob_form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "bob POST should succeed");
    let resp = client
        .post(format!("{gw_url}/commit"))
        .header("Authorization", "Bearer bob-token")
        .json(&serde_json::json!({"message": "bob add"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "bob commit should succeed");

    // Inspect both shards directly. alice's file must be on alice_shard;
    // bob's must be on bob_shard. /files returns the flat manifest list
    // post-commit and is unambiguous about file presence.
    let direct_files = |shard: &str| {
        let url = format!("{shard}/files");
        let c = client.clone();
        async move {
            c.get(&url)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let alice_view = direct_files(&alice_shard).await;
    let bob_view = direct_files(&bob_shard).await;

    // The /files response is an array of {path, file_type, ...} entries.
    let path_in = |v: &serde_json::Value, p: &str| {
        v.as_array()
            .map(|arr| {
                arr.iter()
                    .any(|e| e.get("path").and_then(|n| n.as_str()) == Some(p))
            })
            .unwrap_or(false)
    };
    assert!(
        path_in(&alice_view, "alice-only.txt"),
        "alice's file should be on alice's shard; got: {alice_view}"
    );
    assert!(
        path_in(&bob_view, "bob-only.txt"),
        "bob's file should be on bob's shard; got: {bob_view}"
    );
    assert!(
        !path_in(&bob_view, "alice-only.txt"),
        "alice's file should NOT be on bob's shard"
    );
    assert!(
        !path_in(&alice_view, "bob-only.txt"),
        "bob's file should NOT be on alice's shard"
    );

    // Tenant context propagation: spin up a tiny capturing server, point one
    // of the gateway's shards at it, and verify the X-OMP-Tenant-Context
    // header arrives with a valid signature.
    let captured: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));
    let captured_for_handler = captured.clone();
    let capture_app = Router::new().route(
        "/anything",
        get(move |headers: axum::http::HeaderMap| {
            let captured = captured_for_handler.clone();
            async move {
                let v = headers
                    .get("x-omp-tenant-context")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                *captured.lock().await = v;
                "ok"
            }
        }),
    );
    let cap_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cap_addr = cap_listener.local_addr().unwrap();
    let cap_url = format!("http://{cap_addr}");
    tokio::spawn(async move {
        let _ = axum::serve(cap_listener, capture_app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Build a NEW gateway pointing only at the capturing shard so every
    // request lands there (regardless of tenant hash).
    let mut tokens = HashMap::new();
    tokens.insert(hash_token("carol-token"), "carol".to_string());
    let cfg2 = GatewayConfig {
        shards: vec![cap_url.clone()],
        tokens,
        allow_dev_tokens: false,
    };
    let signer2 = GatewaySigner::generate();
    let vk2 = signer2.verifying_key();
    let state2 = GatewayState::new(cfg2, signer2);
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw2_addr = listener2.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener2, gateway_router(state2)).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = client
        .get(format!("http://{gw2_addr}/anything"))
        .header("Authorization", "Bearer carol-token")
        .send()
        .await
        .unwrap();
    let observed = captured.lock().await.clone().expect("ctx header captured");
    let verified = TenantContext::verify(&observed, &vk2).expect("verify ctx");
    assert_eq!(verified.tenant_id, "carol");

    // Verifying with the WRONG key must fail (sanity).
    assert!(TenantContext::verify(&observed, &verifying).is_err());
}

#[tokio::test]
async fn rejects_unauthorized_requests() {
    let td = TempDir::new().unwrap();
    let shard = spawn_omp_server(td.path().to_path_buf()).await;

    let cfg = GatewayConfig {
        shards: vec![shard],
        tokens: HashMap::new(),
        allow_dev_tokens: false,
    };
    let state = GatewayState::new(cfg, GatewaySigner::generate());
    let app = gateway_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = reqwest::Client::new();

    // No auth header at all.
    let resp = client
        .get(format!("http://{gw_addr}/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Wrong token.
    let resp = client
        .get(format!("http://{gw_addr}/status"))
        .header("Authorization", "Bearer not-a-real-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn dev_tokens_resolve_when_enabled() {
    let td = TempDir::new().unwrap();
    let shard = spawn_omp_server(td.path().to_path_buf()).await;

    let cfg = GatewayConfig {
        shards: vec![shard],
        tokens: HashMap::new(),
        allow_dev_tokens: true,
    };
    let state = GatewayState::new(cfg, GatewaySigner::generate());
    let app = gateway_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{gw_addr}/status"))
        .header("Authorization", "Bearer dev-anyone")
        .send()
        .await
        .unwrap();
    // The dev-token resolves to tenant "anyone"; the omp-server in --no-auth
    // mode happily serves status for any caller.
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn translates_412_to_409() {
    // Stand up a fake upstream that always returns 412.
    let app = Router::new().route("/anything", get(|| async { (axum::http::StatusCode::PRECONDITION_FAILED, "no") }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_url = format!("http://{upstream_addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut tokens = HashMap::new();
    tokens.insert(hash_token("dave-token"), "dave".to_string());
    let cfg = GatewayConfig {
        shards: vec![upstream_url],
        tokens,
        allow_dev_tokens: false,
    };
    let state = GatewayState::new(cfg, GatewaySigner::generate());
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener2.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener2, gateway_router(state)).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{gw_addr}/anything"))
        .header("Authorization", "Bearer dave-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "412 Precondition Failed should translate to 409 Conflict");
}

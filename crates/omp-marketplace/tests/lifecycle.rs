//! Integration tests for the marketplace publish/patch/yank flows added in
//! `docs/design/23-probe-marketplace.md` (PATCH + source-only publish) and
//! `docs/design/25-schema-marketplace.md` (schema marketplace).
//!
//! The probe-publish test invokes a real cargo build (~30s on cold cache);
//! the schema tests are pure JSON + filesystem and run in milliseconds.

use std::path::PathBuf;
use std::time::Duration;

use omp_marketplace::{router, BuildSettings, MarketplaceState};
use omp_tenant_ctx::{TenantContext, HEADER_NAME};
use reqwest::multipart;
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

async fn spawn_marketplace(data_root: PathBuf, scratch_root: PathBuf) -> String {
    let build = BuildSettings {
        probe_common_path: workspace_root().join("probes-src/probe-common"),
        scratch_root,
        wall_clock_secs: 240, // generous for cold builds in CI
    };
    // verifier=None puts the marketplace in dev/demo mode where any decoded
    // X-OMP-Tenant-Context is accepted.
    let state = MarketplaceState::open(data_root, None, build).expect("open state");
    let app = router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("http://{addr}")
}

/// Build an unsigned `X-OMP-Tenant-Context` header value claiming `sub`.
/// In dev/demo mode the marketplace decodes the envelope without verifying
/// the signature, so an empty signature is fine for tests.
fn ctx_header(sub: &str) -> String {
    let ctx = TenantContext {
        tenant_id: "test-tenant".into(),
        quotas_ref: Vec::new(),
        exp_unix: chrono_now_secs() + 60,
        sub: Some(sub.to_string()),
        signature: Vec::new(),
    };
    ctx.encode().expect("encode ctx")
}

fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const TRIVIAL_SCHEMA_TOML: &str = r#"file_type = "txt"
mime_patterns = ["text/plain"]

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"
"#;

// ---------------------------------------------------------------------------
// Schema marketplace — fast, no build
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schema_publish_patch_yank_lifecycle() {
    let data = TempDir::new().unwrap();
    let scratch = TempDir::new().unwrap();
    let url = spawn_marketplace(data.path().into(), scratch.path().into()).await;
    let client = reqwest::Client::new();

    // Publish as alice.
    let alice = ctx_header("alice");
    let form = multipart::Form::new()
        .text("version", "0.1.0")
        .text("description", "trivial txt schema")
        .part(
            "schema",
            multipart::Part::bytes(TRIVIAL_SCHEMA_TOML.as_bytes().to_vec())
                .file_name("schema.toml"),
        );
    let resp = client
        .post(format!("{url}/marketplace/schemas"))
        .header(HEADER_NAME, &alice)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "publish should succeed");
    let body: Value = resp.json().await.unwrap();
    let id = body["schema"]["id"]
        .as_str()
        .expect("id in body")
        .to_string();
    assert_eq!(body["schema"]["file_type"], "txt");
    assert_eq!(body["schema"]["publisher_sub"], "alice");

    // Republish same (publisher, file_type, version) → 409.
    let form2 = multipart::Form::new()
        .text("version", "0.1.0")
        .part(
            "schema",
            multipart::Part::bytes(TRIVIAL_SCHEMA_TOML.as_bytes().to_vec())
                .file_name("schema.toml"),
        );
    let resp = client
        .post(format!("{url}/marketplace/schemas"))
        .header(HEADER_NAME, &alice)
        .multipart(form2)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // PATCH metadata as alice → succeeds.
    let resp = client
        .patch(format!("{url}/marketplace/schemas/{id}"))
        .header(HEADER_NAME, &alice)
        .json(&serde_json::json!({ "description": "edited!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["schema"]["description"], "edited!");

    // PATCH as bob → 403.
    let bob = ctx_header("bob");
    let resp = client
        .patch(format!("{url}/marketplace/schemas/{id}"))
        .header(HEADER_NAME, &bob)
        .json(&serde_json::json!({ "description": "stolen" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Yank as bob → 403.
    let resp = client
        .delete(format!("{url}/marketplace/schemas/{id}"))
        .header(HEADER_NAME, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Yank as alice → 200.
    let resp = client
        .delete(format!("{url}/marketplace/schemas/{id}"))
        .header(HEADER_NAME, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Yanked schema disappears from list.
    let resp = client
        .get(format!("{url}/marketplace/schemas"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let schemas = body["schemas"].as_array().unwrap();
    assert!(
        schemas.iter().all(|s| s["id"].as_str() != Some(&id)),
        "yanked schema should not appear in list"
    );

    // GET on yanked entry returns 410 Gone.
    let resp = client
        .get(format!("{url}/marketplace/schemas/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
}

#[tokio::test]
async fn schema_publish_rejects_malformed_toml() {
    let data = TempDir::new().unwrap();
    let scratch = TempDir::new().unwrap();
    let url = spawn_marketplace(data.path().into(), scratch.path().into()).await;
    let client = reqwest::Client::new();
    let alice = ctx_header("alice");

    // Missing mime_patterns.
    let form = multipart::Form::new()
        .text("version", "0.1.0")
        .part(
            "schema",
            multipart::Part::bytes(b"file_type = \"txt\"\n".to_vec())
                .file_name("schema.toml"),
        );
    let resp = client
        .post(format!("{url}/marketplace/schemas"))
        .header(HEADER_NAME, &alice)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "bad_schema");
}

// ---------------------------------------------------------------------------
// Probe marketplace — exercises the cargo build path
// ---------------------------------------------------------------------------

const TRIVIAL_PROBE_LIB_RS: &str = r#"
//! Minimal probe used to exercise the publish-from-source path.
use ciborium::value::Value as Cbor;
pub use probe_common::{alloc, free};

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);
    probe_common::return_value(Cbor::Integer((bytes.len() as i64).into()))
}
"#;

const TRIVIAL_PROBE_TOML: &str = r#"name = "test.byte_count"
returns = "int"
accepts_kwargs = []
"#;

#[tokio::test]
async fn probe_publish_from_source_then_patch_then_yank() {
    let _ = tracing_subscriber::fmt::try_init();
    let data = TempDir::new().unwrap();
    let scratch = TempDir::new().unwrap();
    let url = spawn_marketplace(data.path().into(), scratch.path().into()).await;
    let client = reqwest::Client::new();
    let alice = ctx_header("alice");

    // Reject pre-built wasm uploads outright.
    let form = multipart::Form::new()
        .text("namespace", "test")
        .text("name", "byte_count")
        .text("version", "0.1.0")
        .part(
            "wasm",
            multipart::Part::bytes(b"\0asm\x01\x00\x00\x00".to_vec()).file_name("probe.wasm"),
        )
        .part(
            "manifest",
            multipart::Part::bytes(TRIVIAL_PROBE_TOML.as_bytes().to_vec())
                .file_name("probe.toml"),
        );
    let resp = client
        .post(format!("{url}/marketplace/probes"))
        .header(HEADER_NAME, &alice)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "wasm_upload_forbidden");

    // Smuggling attempt: post wasm bytes via the `source` field. The wasm
    // magic header is valid UTF-8, so we need an explicit check (not just
    // the from_utf8 guard) to catch it.
    let form = multipart::Form::new()
        .text("namespace", "test")
        .text("name", "byte_count")
        .text("version", "0.1.0")
        .part(
            "source",
            multipart::Part::bytes(b"\0asm\x01\x00\x00\x00".to_vec()).file_name("lib.rs"),
        )
        .part(
            "manifest",
            multipart::Part::bytes(TRIVIAL_PROBE_TOML.as_bytes().to_vec())
                .file_name("probe.toml"),
        );
    let resp = client
        .post(format!("{url}/marketplace/probes"))
        .header(HEADER_NAME, &alice)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "wasm_upload_forbidden");

    // Real publish: source → server build → catalog row.
    let form = multipart::Form::new()
        .text("namespace", "test")
        .text("name", "byte_count")
        .text("version", "0.1.0")
        .text("description", "counts file bytes")
        .part(
            "source",
            multipart::Part::bytes(TRIVIAL_PROBE_LIB_RS.as_bytes().to_vec())
                .file_name("lib.rs"),
        )
        .part(
            "manifest",
            multipart::Part::bytes(TRIVIAL_PROBE_TOML.as_bytes().to_vec())
                .file_name("probe.toml"),
        );
    let resp = client
        .post(format!("{url}/marketplace/probes"))
        .header(HEADER_NAME, &alice)
        .multipart(form)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "publish should succeed; body: {}",
        resp.text().await.unwrap_or_default()
    );
    let body: Value = resp.json().await.unwrap();
    let id = body["probe"]["id"].as_str().unwrap().to_string();
    let wasm_hash = body["probe"]["wasm_hash"].as_str().unwrap().to_string();
    let source_hash = body["probe"]["source_hash"].as_str().unwrap().to_string();
    assert_ne!(wasm_hash, source_hash, "wasm and source must hash differently");
    assert_eq!(body["probe"]["publisher_sub"], "alice");

    // The wasm blob is fetchable and starts with the wasm magic.
    let resp = client
        .get(format!("{url}/marketplace/probes/{id}/blobs/{wasm_hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.bytes().await.unwrap();
    assert!(
        bytes.len() > 4 && &bytes[..4] == b"\0asm",
        "fetched wasm blob is not a wasm module"
    );
    // PATCH metadata as alice.
    let resp = client
        .patch(format!("{url}/marketplace/probes/{id}"))
        .header(HEADER_NAME, &alice)
        .json(&serde_json::json!({ "description": "edited!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["probe"]["description"], "edited!");
    // wasm_hash and source_hash unchanged after metadata edit.
    assert_eq!(body["probe"]["wasm_hash"], wasm_hash);
    assert_eq!(body["probe"]["source_hash"], source_hash);

    // PATCH as bob → 403.
    let bob = ctx_header("bob");
    let resp = client
        .patch(format!("{url}/marketplace/probes/{id}"))
        .header(HEADER_NAME, &bob)
        .json(&serde_json::json!({ "description": "stolen" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Yank as alice.
    let resp = client
        .delete(format!("{url}/marketplace/probes/{id}"))
        .header(HEADER_NAME, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

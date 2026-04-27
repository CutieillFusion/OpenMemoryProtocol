//! End-to-end integration test for `omp-builder`.
//!
//! Stands up the service on a random port, POSTs a trivial probe, polls
//! until terminal state, and verifies the response shape. Uses the
//! workspace's `probes-src/probe-common` as the path dependency.
//!
//! Note: this test invokes `cargo build --release --target
//! wasm32-unknown-unknown` against a freshly-stamped Cargo project, which
//! takes ~30s on a cold cache. Subsequent runs hit cargo's incremental
//! cache and are much faster.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use omp_builder::{router, BuilderConfig, BuilderState};
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;

fn workspace_root() -> PathBuf {
    // Tests run from the crate dir (crates/omp-builder); workspace root is
    // two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

async fn spawn_builder(scratch: PathBuf) -> String {
    let config = BuilderConfig {
        scratch_root: scratch,
        probe_common_path: workspace_root().join("probes-src/probe-common"),
        wall_clock_secs: 180, // generous for cold builds in CI
        max_concurrent_builds: 2,
    };
    let state = BuilderState::new(config);
    let app = router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

const TRIVIAL_PROBE_LIB_RS: &str = r#"
//! Trivial probe — returns `true` if the input file contains the literal
//! string "This is a test string", `false` otherwise.

use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

const NEEDLE: &[u8] = b"This is a test string";

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);
    let hit = bytes.windows(NEEDLE.len()).any(|w| w == NEEDLE);
    probe_common::return_value(Cbor::Bool(hit))
}
"#;

const TRIVIAL_PROBE_TOML: &str = r#"name = "text.is_test_string"
returns = "bool"
accepts_kwargs = []
description = "Returns true if the file contains \"This is a test string\"."

[limits]
memory_mb = 32
fuel = 100000000
wall_clock_s = 5
"#;

#[tokio::test]
async fn build_compiles_a_real_probe_to_wasm() {
    let _ = tracing_subscriber::fmt::try_init();
    let scratch = TempDir::new().unwrap();
    let url = spawn_builder(scratch.path().to_path_buf()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/probes/build"))
        .header("X-OMP-Tenant", "alice")
        .json(&serde_json::json!({
            "namespace": "text",
            "name": "is_test_string",
            "lib_rs": TRIVIAL_PROBE_LIB_RS,
            "probe_toml": TRIVIAL_PROBE_TOML,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "expected 202 accepted");
    let body: Value = resp.json().await.unwrap();
    let job_id = body["job_id"]
        .as_str()
        .expect("job_id in 202 body")
        .to_string();

    // Poll until terminal.
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let mut state = String::new();
    let mut final_body: Value = Value::Null;
    while std::time::Instant::now() < deadline {
        let r = client
            .get(format!("{url}/probes/build/{job_id}"))
            .header("X-OMP-Tenant", "alice")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        final_body = r.json().await.unwrap();
        state = final_body["state"].as_str().unwrap_or("").to_string();
        if matches!(state.as_str(), "ok" | "failed" | "cancelled") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    assert_eq!(
        state, "ok",
        "build did not succeed; final body = {final_body:#?}"
    );

    let artifacts = final_body["artifacts"]
        .as_array()
        .expect("artifacts on a successful build");
    assert_eq!(
        artifacts.len(),
        3,
        ".wasm + .probe.toml + .rs expected; got {artifacts:?}"
    );
    let paths: Vec<&str> = artifacts
        .iter()
        .filter_map(|a| a["path"].as_str())
        .collect();
    assert!(paths.contains(&"probes/text/is_test_string.wasm"));
    assert!(paths.contains(&"probes/text/is_test_string.probe.toml"));
    assert!(paths.contains(&"probes/text/is_test_string.rs"));

    // Check the wasm artifact has the right magic header (\0asm).
    use base64::Engine;
    let wasm_b64 = artifacts
        .iter()
        .find(|a| a["path"].as_str() == Some("probes/text/is_test_string.wasm"))
        .unwrap()["bytes_b64"]
        .as_str()
        .unwrap();
    let wasm_bytes = base64::engine::general_purpose::STANDARD
        .decode(wasm_b64)
        .unwrap();
    assert!(
        wasm_bytes.len() > 4 && &wasm_bytes[0..4] == b"\0asm",
        "first four bytes are not the wasm magic; build produced {} bytes",
        wasm_bytes.len()
    );
}

#[tokio::test]
async fn unauthenticated_post_is_401() {
    let scratch = TempDir::new().unwrap();
    let url = spawn_builder(scratch.path().to_path_buf()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/probes/build"))
        // no X-OMP-Tenant header
        .json(&serde_json::json!({
            "namespace": "x",
            "name": "y",
            "lib_rs": "",
            "probe_toml": "",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn cross_tenant_get_is_404() {
    let _ = tracing_subscriber::fmt::try_init();
    let scratch = TempDir::new().unwrap();
    let url = spawn_builder(scratch.path().to_path_buf()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    // alice creates a build (which will fail to compile but that's fine —
    // we only need a valid job id in alice's namespace).
    let resp = client
        .post(format!("{url}/probes/build"))
        .header("X-OMP-Tenant", "alice")
        .json(&serde_json::json!({
            "namespace": "x",
            "name": "y",
            "lib_rs": "this is not valid rust",
            "probe_toml": "name = \"x.y\"\nreturns = \"null\"\naccepts_kwargs = []\n",
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let job_id = body["job_id"].as_str().unwrap().to_string();

    // bob asks for alice's job id; must look like 404, not "found".
    let r = client
        .get(format!("{url}/probes/build/{job_id}"))
        .header("X-OMP-Tenant", "bob")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn validation_rejects_bad_namespace() {
    let scratch = TempDir::new().unwrap();
    let url = spawn_builder(scratch.path().to_path_buf()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/probes/build"))
        .header("X-OMP-Tenant", "alice")
        .json(&serde_json::json!({
            "namespace": "Invalid CAPS",
            "name": "y",
            "lib_rs": "",
            "probe_toml": "",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
async fn healthz_returns_ok() {
    let scratch = TempDir::new().unwrap();
    let url = spawn_builder(scratch.path().to_path_buf()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{url}/healthz")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["service"], "omp-builder");
    let _: Arc<()> = Arc::new(());
}

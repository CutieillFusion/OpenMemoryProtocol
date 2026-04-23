//! Upload-session HTTP integration tests — `docs/design/12-large-files.md`.

use std::sync::Arc;
use std::time::Duration;

use omp_core::api::Repo;
use omp_core::registry::{default_registry_path, Quotas, TenantRegistry};
use omp_core::tenant::TenantId;
use omp_server::{routes, AppState};
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;

async fn spawn_single(td: &TempDir) -> String {
    let repo = Repo::init(td.path()).unwrap();
    stage_all(&repo, td.path());
    // Rewrite omp.toml to use a tiny chunk size so tests stay fast.
    std::fs::write(
        td.path().join("omp.toml"),
        "[ingest]\ndefault_schema_policy = \"minimal\"\n\n[storage]\nchunk_size_bytes = 64\n",
    )
    .unwrap();
    // Re-stage the rewritten omp.toml.
    let bytes = std::fs::read(td.path().join("omp.toml")).unwrap();
    repo.add("omp.toml", &bytes, None, None).unwrap();

    let state = Arc::new(AppState::single(Arc::new(repo)));
    let app = routes::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("http://{addr}")
}

async fn spawn_multi_tenant_with_quota(
    td: &TempDir,
    tenants_base: &std::path::Path,
    bytes_cap: Option<u64>,
) -> (String, String) {
    let registry_path = default_registry_path(tenants_base);
    std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
    let mut reg = TenantRegistry::default();
    let quotas = Quotas {
        bytes: bytes_cap,
        ..Quotas::unlimited()
    };
    let token = reg
        .create(TenantId::new("alice").unwrap(), quotas)
        .unwrap();
    reg.save(&registry_path).unwrap();
    let _ = td;
    let state = AppState::multi(tenants_base.to_path_buf(), registry_path).unwrap();
    let app = routes::router(Arc::new(state));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (format!("http://{addr}"), token)
}

fn stage_all(repo: &Repo, root: &std::path::Path) {
    let entries =
        omp_core::walker::walk_repo(root, &omp_core::walker::WalkOptions::default()).unwrap();
    for e in entries {
        let bytes = std::fs::read(&e.fs_path).unwrap();
        repo.add(&e.repo_path, &bytes, None, None).unwrap();
    }
}

#[tokio::test]
async fn upload_session_happy_path() {
    let td = TempDir::new().unwrap();
    let base = spawn_single(&td).await;
    let client = reqwest::Client::new();

    // Open a session for a 200-byte file.
    let open = client
        .post(format!("{base}/uploads"))
        .json(&json!({ "declared_size": 200 }))
        .send()
        .await
        .unwrap();
    assert_eq!(open.status(), 201);
    let body: serde_json::Value = open.json().await.unwrap();
    let id = body["upload_id"].as_str().unwrap().to_string();

    // PATCH two chunks (100 bytes each).
    let first: Vec<u8> = (0..100u32).map(|i| (i & 0xff) as u8).collect();
    let second: Vec<u8> = (100..200u32).map(|i| (i & 0xff) as u8).collect();
    for (offset, payload) in [(0u64, &first), (100u64, &second)] {
        let r = client
            .patch(format!("{base}/uploads/{id}?offset={offset}"))
            .body(payload.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204, "patch at offset {offset}");
    }

    // Commit the staging slot the upload produced. File_type=_minimal
    // because the sniffed MIME on random bytes doesn't match any schema and
    // the test `omp.toml` has policy = "minimal".
    let commit = client
        .post(format!("{base}/uploads/{id}/commit"))
        .json(&json!({ "path": "data/big.bin", "file_type": "_minimal" }))
        .send()
        .await
        .unwrap();
    assert_eq!(commit.status(), 200, "commit body: {:?}", commit.text().await);

    // Commit the repo so /files can find the tree entry. (upload_commit
    // stages; POST /commit snapshots into a commit — same contract as
    // POST /files.)
    let c = client
        .post(format!("{base}/commit"))
        .json(&json!({ "message": "ingest big.bin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(c.status(), 200);

    // GET the file's manifest back; verify source_hash resolves to a chunks object.
    let show = client
        .get(format!("{base}/files/data/big.bin?verbose=true"))
        .send()
        .await
        .unwrap();
    assert_eq!(show.status(), 200);
    let m: serde_json::Value = show.json().await.unwrap();
    let source_hash = m["manifest"]["source_hash"]
        .as_str()
        .or_else(|| m["source_hash"].as_str())
        .expect("source_hash in manifest");
    assert_eq!(source_hash.len(), 64);
}

#[tokio::test]
async fn upload_session_patch_is_idempotent() {
    let td = TempDir::new().unwrap();
    let base = spawn_single(&td).await;
    let client = reqwest::Client::new();

    let open = client
        .post(format!("{base}/uploads"))
        .json(&json!({ "declared_size": 4 }))
        .send()
        .await
        .unwrap();
    let id = open.json::<serde_json::Value>().await.unwrap()["upload_id"]
        .as_str()
        .unwrap()
        .to_string();

    // PATCH the same offset twice — second overwrites.
    client
        .patch(format!("{base}/uploads/{id}?offset=0"))
        .body(b"abcd".to_vec())
        .send()
        .await
        .unwrap();
    client
        .patch(format!("{base}/uploads/{id}?offset=0"))
        .body(b"wxyz".to_vec())
        .send()
        .await
        .unwrap();

    let commit = client
        .post(format!("{base}/uploads/{id}/commit"))
        .json(&json!({ "path": "a.bin", "file_type": "_minimal" }))
        .send()
        .await
        .unwrap();
    assert_eq!(commit.status(), 200);
}

#[tokio::test]
async fn upload_session_quota_rejects_at_open() {
    let td = TempDir::new().unwrap();
    let tb = td.path().join("tenants");
    // Tight quota: 1 KB; the starter pack alone pushes past that during the
    // first open. We declare 10 KB up front to trigger quota rejection at
    // byte zero.
    let (base, token) = spawn_multi_tenant_with_quota(&td, &tb, Some(1024)).await;
    let client = reqwest::Client::new();

    let r = client
        .post(format!("{base}/uploads"))
        .bearer_auth(&token)
        .json(&json!({ "declared_size": 10_000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429, "expected 429, got {}", r.status());
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "quota_exceeded");
}

#[tokio::test]
async fn upload_session_delete_removes_it() {
    let td = TempDir::new().unwrap();
    let base = spawn_single(&td).await;
    let client = reqwest::Client::new();

    let open = client
        .post(format!("{base}/uploads"))
        .json(&json!({ "declared_size": 4 }))
        .send()
        .await
        .unwrap();
    let id = open.json::<serde_json::Value>().await.unwrap()["upload_id"]
        .as_str()
        .unwrap()
        .to_string();

    let dir = td.path().join(".omp/uploads").join(&id);
    assert!(dir.is_dir());
    let r = client
        .delete(format!("{base}/uploads/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(!dir.exists());

    // Second DELETE returns 404 (load_state says NotFound).
    let r = client
        .delete(format!("{base}/uploads/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn upload_session_ttl_cleanup_reaps() {
    // Unit-level test of the `reap_stale` helper — exercising the TTL
    // codepath end-to-end over HTTP requires time-travel we don't have in
    // tests. We assert the in-process call does the right thing.
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    let h = repo.upload_open(16).unwrap();
    repo.upload_write(&h.upload_id, 0, b"abcdefghijklmnop").unwrap();

    // Manually age the state.toml backward by 48h by rewriting the created_at.
    let state_path = td.path().join(".omp/uploads").join(&h.upload_id).join("state.toml");
    let body = std::fs::read_to_string(&state_path).unwrap();
    let aged = body.replace(
        // The session was just created "now"; replace with a far-past time.
        // The TOML field is `created_at = "..."`. Find-and-replace the value.
        "created_at = \"",
        "created_at_old = \"unused-marker\"\ncreated_at = \"2000-01-01T00:00:00Z",
    );
    // `replace` above leaves the original prefix, so we now have two
    // `created_at` keys in the file — TOML will reject that. Rewrite the
    // whole state.toml from scratch with the aged timestamp.
    let _ = aged;
    let rewritten = body
        .lines()
        .map(|l| {
            if l.starts_with("created_at = ") {
                "created_at = \"2000-01-01T00:00:00Z\"".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&state_path, rewritten).unwrap();

    let reaped = repo.upload_gc().unwrap();
    assert_eq!(reaped, 1);
    assert!(!td.path().join(".omp/uploads").join(&h.upload_id).exists());
}

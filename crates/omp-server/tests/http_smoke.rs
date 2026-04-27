//! HTTP smoke test: spin the server on a loopback port, hit a few routes.

use std::sync::Arc;
use std::time::Duration;

use omp_core::api::Repo;
use omp_server::{routes, AppState};
use tempfile::TempDir;
use tokio::net::TcpListener;

#[tokio::test]
async fn status_and_tree_routes_respond() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_all(&repo, td.path());

    let state = Arc::new(AppState::single(Arc::new(repo)));
    let app = routes::router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = reqwest::Client::new();

    let status = client
        .get(format!("http://{addr}/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    let body: serde_json::Value = status.json().await.unwrap();
    assert!(
        body.get("staged").is_some(),
        "status body missing staged: {body}"
    );
}

fn stage_all(repo: &Repo, root: &std::path::Path) {
    let entries =
        omp_core::walker::walk_repo(root, &omp_core::walker::WalkOptions::default()).unwrap();
    for e in entries {
        let bytes = std::fs::read(&e.fs_path).unwrap();
        repo.add(&e.repo_path, &bytes, None, None).unwrap();
    }
}

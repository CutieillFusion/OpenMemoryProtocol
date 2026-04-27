//! Integration tests for `GET /query` (docs/design/15-query-and-discovery.md).
//! Spins up the HTTP server, ingests a small set of manifests, and verifies
//! predicate filtering, cursor pagination, and error handling end-to-end.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use omp_core::api::Repo;
use omp_core::manifest::FieldValue;
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

/// Stage one text-typed file with custom user fields, then commit.
fn add_text(repo: &Repo, path: &str, content: &str, fields: Vec<(&str, FieldValue)>) {
    let mut map: BTreeMap<String, FieldValue> = BTreeMap::new();
    for (k, v) in fields {
        map.insert(k.to_string(), v);
    }
    repo.add(path, content.as_bytes(), Some(map), Some("text"))
        .unwrap_or_else(|e| panic!("add {path}: {e}"));
}

#[tokio::test]
async fn query_with_no_predicate_returns_all_user_files() {
    let td = TempDir::new().unwrap();
    let (repo, base) = spawn(td.path().to_path_buf()).await;

    add_text(&repo, "docs/a.md", "hi", vec![]);
    add_text(&repo, "docs/b.md", "hi", vec![]);
    repo.commit("seed", None).unwrap();

    let body: serde_json::Value = reqwest::get(format!("{base}/query"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let m = body.get("matches").unwrap().as_array().unwrap();
    let paths: Vec<&str> = m.iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert!(paths.iter().any(|p| *p == "docs/a.md"));
    assert!(paths.iter().any(|p| *p == "docs/b.md"));
}

#[tokio::test]
async fn query_filters_by_user_field() {
    let td = TempDir::new().unwrap();
    let (repo, base) = spawn(td.path().to_path_buf()).await;

    add_text(
        &repo,
        "policies/a.md",
        "P",
        vec![(
            "tags",
            FieldValue::List(vec![FieldValue::String("policy".into())]),
        )],
    );
    add_text(
        &repo,
        "drafts/b.md",
        "D",
        vec![(
            "tags",
            FieldValue::List(vec![FieldValue::String("draft".into())]),
        )],
    );
    repo.commit("seed", None).unwrap();

    let url = format!(
        "{base}/query?where={}",
        urlencoding::encode("tags contains \"policy\"")
    );
    let body: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    let paths: Vec<&str> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["policies/a.md"]);
}

#[tokio::test]
async fn query_filters_by_top_level_file_type() {
    let td = TempDir::new().unwrap();
    let (repo, base) = spawn(td.path().to_path_buf()).await;
    add_text(&repo, "a.md", "hi", vec![]);
    repo.commit("seed", None).unwrap();

    let url = format!(
        "{base}/query?where={}",
        urlencoding::encode("file_type = \"text\"")
    );
    let body: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    let paths: Vec<&str> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"a.md"));

    let url_zero = format!(
        "{base}/query?where={}",
        urlencoding::encode("file_type = \"pdf\"")
    );
    let body: serde_json::Value = reqwest::get(&url_zero).await.unwrap().json().await.unwrap();
    assert!(body["matches"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn query_with_bad_predicate_returns_400() {
    let td = TempDir::new().unwrap();
    let (_repo, base) = spawn(td.path().to_path_buf()).await;
    let url = format!(
        "{base}/query?where={}",
        urlencoding::encode("file_type === \"pdf\"")
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"].as_str(), Some("bad_query"));
}

#[tokio::test]
async fn query_pagination_via_cursor() {
    let td = TempDir::new().unwrap();
    let (repo, base) = spawn(td.path().to_path_buf()).await;

    for i in 0..7 {
        add_text(&repo, &format!("doc-{i:02}.md"), "x", vec![]);
    }
    repo.commit("seed", None).unwrap();

    // Page 1: limit=3
    let body: serde_json::Value = reqwest::get(format!("{base}/query?limit=3"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let page1: Vec<String> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    let cursor = body["next_cursor"]
        .as_str()
        .expect("first page should set next_cursor")
        .to_string();
    assert_eq!(page1.len(), 3);

    // Page 2: cursor + limit=3
    let body: serde_json::Value = reqwest::get(format!(
        "{base}/query?limit=3&cursor={}",
        urlencoding::encode(&cursor)
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let page2: Vec<String> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(page2.len(), 3);
    let cursor2 = body["next_cursor"]
        .as_str()
        .expect("page 2 cursor")
        .to_string();

    // Page 3: only the seed doc-06.md left.
    let body: serde_json::Value = reqwest::get(format!(
        "{base}/query?limit=3&cursor={}",
        urlencoding::encode(&cursor2)
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let page3: Vec<String> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(page3.len(), 1);
    assert_eq!(page3[0], "doc-06.md");
    assert!(
        body["next_cursor"].is_null(),
        "last page should not set next_cursor"
    );

    // Together, page1 + page2 + page3 should cover all 7 docs distinctly,
    // sorted lexicographically.
    let mut all = page1.clone();
    all.extend(page2.iter().cloned());
    all.extend(page3.iter().cloned());
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(all, sorted);
    let unique: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(unique.len(), all.len());
}

#[tokio::test]
async fn query_with_compound_predicate() {
    let td = TempDir::new().unwrap();
    let (repo, base) = spawn(td.path().to_path_buf()).await;

    // Vary the file size so the probe-populated `byte_size` field becomes
    // a useful predicate axis. The text schema declares byte_size as int.
    add_text(
        &repo,
        "a.md",
        &"x".repeat(1000), // big policy doc
        vec![(
            "tags",
            FieldValue::List(vec![FieldValue::String("policy".into())]),
        )],
    );
    add_text(
        &repo,
        "b.md",
        "x", // tiny policy doc
        vec![(
            "tags",
            FieldValue::List(vec![FieldValue::String("policy".into())]),
        )],
    );
    add_text(
        &repo,
        "c.md",
        &"x".repeat(2000), // big draft doc
        vec![(
            "tags",
            FieldValue::List(vec![FieldValue::String("draft".into())]),
        )],
    );
    repo.commit("seed", None).unwrap();

    let q = "tags contains \"policy\" AND byte_size > 100";
    let url = format!("{base}/query?where={}", urlencoding::encode(q));
    let body: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    let paths: Vec<&str> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["a.md"], "only a.md matches both clauses");
}

#[tokio::test]
async fn query_prefix_restricts_walk() {
    let td = TempDir::new().unwrap();
    let (repo, base) = spawn(td.path().to_path_buf()).await;

    add_text(&repo, "policies/p1.md", "x", vec![]);
    add_text(&repo, "policies/p2.md", "x", vec![]);
    add_text(&repo, "drafts/d1.md", "x", vec![]);
    repo.commit("seed", None).unwrap();

    let body: serde_json::Value = reqwest::get(format!("{base}/query?prefix=policies/"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let paths: Vec<&str> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.iter().all(|p| p.starts_with("policies/")));
    assert!(paths.iter().any(|p| *p == "policies/p1.md"));
    assert!(paths.iter().any(|p| *p == "policies/p2.md"));
    assert!(!paths.iter().any(|p| *p == "drafts/d1.md"));
}

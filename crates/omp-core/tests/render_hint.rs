//! `Repo::show()` must surface the schema's render hint, resolving the
//! schema **at the manifest's commit** so time-travel returns the right
//! hint for the right snapshot.

use omp_core::api::{AuthorOverride, Fields, Repo, ShowResult};
use omp_core::manifest::FieldValue;
use omp_core::schema::RenderKind;
use tempfile::TempDir;

fn fixed_author_at(ts: &str) -> AuthorOverride {
    AuthorOverride {
        name: Some("test".into()),
        email: Some("test@local".into()),
        timestamp: Some(ts.into()),
    }
}

fn stage_starter_artifacts(repo: &Repo) {
    let root = repo.root().to_path_buf();
    let opts = omp_core::walker::WalkOptions::default();
    let entries = omp_core::walker::walk_repo(&root, &opts).unwrap();
    for e in entries {
        let bytes = std::fs::read(&e.fs_path).unwrap();
        repo.add(&e.repo_path, &bytes, None, None).unwrap();
    }
}

#[test]
fn show_returns_render_hint_from_schema() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    // Replace the starter text schema with one that explicitly opts into
    // markdown rendering, then stage and commit the snapshot.
    let custom_text = br#"file_type = "text"
mime_patterns = ["text/*"]

[render]
kind = "markdown"
max_inline_bytes = 32768

[fields.title]
source = "user_provided"
type = "string"
"#;
    std::fs::write(td.path().join("schemas/text.schema"), custom_text).unwrap();

    stage_starter_artifacts(&repo);
    let mut u = Fields::new();
    u.insert("title".into(), FieldValue::String("Hello".into()));
    repo.add("docs/a.md", b"# Hi\n", Some(u), Some("text"))
        .unwrap();
    repo.commit("init", Some(fixed_author_at("2026-04-22T00:00:00Z")))
        .unwrap();

    let shown = repo.show("docs/a.md", None).unwrap();
    let render = match shown {
        ShowResult::Manifest { render, .. } => render,
        other => panic!("expected Manifest, got {other:?}"),
    };
    assert_eq!(render.kind, RenderKind::Markdown);
    assert_eq!(render.max_inline_bytes, Some(32768));
}

#[test]
fn show_falls_back_to_binary_when_schema_missing() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);

    // Ingest under the starter `text` schema.
    let mut u = Fields::new();
    u.insert("title".into(), FieldValue::String("v1".into()));
    repo.add("a.md", b"hi\n", Some(u), Some("text")).unwrap();
    repo.commit("init", Some(fixed_author_at("2026-04-22T00:00:00Z")))
        .unwrap();

    // Now stage a delete of the schema, commit again.
    repo.remove("schemas/text.schema").unwrap();
    repo.commit(
        "drop schema",
        Some(fixed_author_at("2026-04-22T01:00:00Z")),
    )
    .unwrap();

    // The manifest still references file_type="text", but the schema has
    // been removed at HEAD. show() must not error — it falls back to Binary.
    let shown = repo.show("a.md", None).unwrap();
    let render = match shown {
        ShowResult::Manifest { render, .. } => render,
        other => panic!("expected Manifest, got {other:?}"),
    };
    assert_eq!(render.kind, RenderKind::Binary);
}

#[test]
fn show_render_hint_time_travels_with_schema() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    // v1: text schema declares [render] kind = "text".
    let v1_schema = br#"file_type = "text"
mime_patterns = ["text/*"]

[render]
kind = "text"

[fields.title]
source = "user_provided"
type = "string"
"#;
    std::fs::write(td.path().join("schemas/text.schema"), v1_schema).unwrap();
    stage_starter_artifacts(&repo);
    let mut u = Fields::new();
    u.insert("title".into(), FieldValue::String("v1".into()));
    repo.add("a.md", b"hi\n", Some(u), Some("text")).unwrap();
    let _c1 = repo
        .commit("v1", Some(fixed_author_at("2026-04-22T00:00:00Z")))
        .unwrap();

    // v2: same schema, switch to markdown rendering. The on-disk manifest
    // for a.md doesn't change (no field churn), but the schema does.
    let v2_schema = br#"file_type = "text"
mime_patterns = ["text/*"]

[render]
kind = "markdown"

[fields.title]
source = "user_provided"
type = "string"
"#;
    repo.add("schemas/text.schema", v2_schema, None, None)
        .unwrap();
    let _c2 = repo
        .commit(
            "switch to markdown",
            Some(fixed_author_at("2026-04-22T01:00:00Z")),
        )
        .unwrap();

    // HEAD: render hint follows the v2 schema.
    let now = repo.show("a.md", None).unwrap();
    match now {
        ShowResult::Manifest { render, .. } => {
            assert_eq!(render.kind, RenderKind::Markdown, "HEAD should use v2 schema");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // HEAD~1: render hint must follow the v1 schema, not HEAD.
    let past = repo.show("a.md", Some("HEAD~1")).unwrap();
    match past {
        ShowResult::Manifest { render, .. } => {
            assert_eq!(
                render.kind,
                RenderKind::Text,
                "HEAD~1 should use v1 schema (time-travel)"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

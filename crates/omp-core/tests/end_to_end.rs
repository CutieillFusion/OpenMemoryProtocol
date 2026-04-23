//! End-to-end integration tests against `omp_core::api::Repo`.

use omp_core::api::{AuthorOverride, Fields, Repo, ShowResult};
use omp_core::manifest::FieldValue;
use omp_core::registry::Quotas;
use omp_core::tenant::TenantId;
use tempfile::TempDir;

fn fixed_author() -> AuthorOverride {
    AuthorOverride {
        name: Some("test".into()),
        email: Some("test@local".into()),
        timestamp: Some("2026-04-22T00:00:00Z".into()),
    }
}

#[test]
fn init_drops_starter_pack() {
    let td = TempDir::new().unwrap();
    let _repo = Repo::init(td.path()).unwrap();
    assert!(td.path().join(".omp/HEAD").exists());
    assert!(td.path().join("schemas/text.schema").exists());
    assert!(td.path().join("omp.toml").exists());
    // The v1 starter pack is the three `file.*` probes.
    for basename in ["size", "mime", "sha256"] {
        assert!(td.path().join(format!("probes/file/{basename}.wasm")).exists());
        assert!(td
            .path()
            .join(format!("probes/file/{basename}.probe.toml"))
            .exists());
    }
    // No text/pdf/image/audio probes in the starter pack.
    assert!(!td.path().join("probes/text").exists());
    assert!(!td.path().join("probes/pdf").exists());
    assert!(!td.path().join("schemas/pdf.schema").exists());
}

#[test]
fn add_text_then_commit_then_log() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    // Stage the starter schemas + probes + omp.toml so the commit captures a
    // usable tree.
    stage_starter_artifacts(&repo);

    // Add a user text file.
    let mut user = Fields::new();
    user.insert("title".into(), FieldValue::String("Hello".into()));
    user.insert(
        "tags".into(),
        FieldValue::List(vec![
            FieldValue::String("demo".into()),
            FieldValue::String("intro".into()),
        ]),
    );
    let res = repo
        .add("docs/hello.md", b"# Hello\n\nIntro.\n", Some(user), Some("text"))
        .unwrap();
    match res {
        omp_core::api::AddResult::Manifest { .. } => {}
        other => panic!("expected Manifest, got {other:?}"),
    }

    let commit_hash = repo.commit("initial", Some(fixed_author())).unwrap();
    assert!(!commit_hash.hex().is_empty());

    let log = repo.log_commits(None, 10).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].message, "initial");
    assert_eq!(log[0].author, "test");

    // Show the manifest.
    let shown = repo.show("docs/hello.md", None).unwrap();
    match shown {
        ShowResult::Manifest { manifest, .. } => {
            assert_eq!(manifest.file_type, "text");
            assert_eq!(
                manifest.fields.get("title"),
                Some(&FieldValue::String("Hello".into()))
            );
            assert!(manifest.probe_hashes.contains_key("file.size"));
            assert!(manifest.probe_hashes.contains_key("file.sha256"));
        }
        other => panic!("expected Manifest, got {other:?}"),
    }
}

#[test]
fn time_travel_sees_past_manifest() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);

    // First commit: hello with title="v1".
    let mut u = Fields::new();
    u.insert("title".into(), FieldValue::String("v1".into()));
    repo.add("a.md", b"hi\n", Some(u), Some("text")).unwrap();
    let c1 = repo.commit("v1", Some(fixed_author())).unwrap();

    // Second commit: patch title to "v2".
    let mut u2 = Fields::new();
    u2.insert("title".into(), FieldValue::String("v2".into()));
    repo.patch_fields("a.md", u2).unwrap();
    let c2 = repo
        .commit(
            "v2",
            Some(AuthorOverride {
                timestamp: Some("2026-04-22T01:00:00Z".into()),
                ..fixed_author()
            }),
        )
        .unwrap();
    assert_ne!(c1, c2);

    // --at HEAD~1 returns the v1 title.
    let shown_past = repo.show("a.md", Some("HEAD~1")).unwrap();
    match shown_past {
        ShowResult::Manifest { manifest, .. } => {
            assert_eq!(
                manifest.fields.get("title"),
                Some(&FieldValue::String("v1".into()))
            );
        }
        other => panic!("unexpected: {other:?}"),
    }

    // --at HEAD returns v2.
    let shown_now = repo.show("a.md", None).unwrap();
    match shown_now {
        ShowResult::Manifest { manifest, .. } => {
            assert_eq!(
                manifest.fields.get("title"),
                Some(&FieldValue::String("v2".into()))
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_ingest_dry_run_does_not_stage() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    // Dry-run an ingest. Nothing should appear in status.staged.
    let manifest = repo
        .test_ingest("docs/a.md", b"hi", None, None)
        .unwrap();
    assert_eq!(manifest.file_type, "text");

    let status = repo.status().unwrap();
    assert!(status.staged.is_empty());
}

#[test]
fn branches_and_checkout() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    let mut u = Fields::new();
    u.insert("title".into(), FieldValue::String("root".into()));
    repo.add("a.md", b"1", Some(u), Some("text")).unwrap();
    repo.commit("root", Some(fixed_author())).unwrap();

    repo.branch("experimental", None).unwrap();
    let branches = repo.list_branches().unwrap();
    let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"main"));
    assert!(names.contains(&"experimental"));

    repo.checkout("experimental").unwrap();
    let status = repo.status().unwrap();
    assert_eq!(status.branch.as_deref(), Some("refs/heads/experimental"));
}

#[test]
fn invalid_schema_rejected_on_upload() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    // A schema that references a missing probe.
    let bad = br#"file_type = "custom"
mime_patterns = ["application/x-custom"]

[fields.x]
source = "probe"
probe = "missing.probe"
type = "string"
"#;
    let err = repo.add("schemas/custom.schema", bad, None, None).unwrap_err();
    assert!(matches!(err, omp_core::OmpError::SchemaValidation(_)));
}

#[test]
fn remove_then_commit_drops_path() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    repo.add("a.md", b"hi", None, Some("text")).unwrap();
    repo.commit("init", Some(fixed_author())).unwrap();

    repo.remove("a.md").unwrap();
    repo.commit(
        "rm",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    assert!(repo.show("a.md", None).is_err());
}

#[test]
fn quota_exceeded_is_reported() {
    let td = TempDir::new().unwrap();
    // Very tight byte cap: the starter pack alone blows past it.
    let repo = Repo::init_tenant(
        td.path(),
        TenantId::new("tiny").unwrap(),
        Quotas {
            bytes: Some(1024),
            ..Quotas::unlimited()
        },
    )
    .unwrap();

    // Staging the starter pack pushes past 1 KB of objects.
    let entries = omp_core::walker::walk_repo(
        td.path(),
        &omp_core::walker::WalkOptions::default(),
    )
    .unwrap();
    let mut errors = Vec::new();
    for e in entries {
        let bytes = std::fs::read(&e.fs_path).unwrap();
        if let Err(err) = repo.add(&e.repo_path, &bytes, None, None) {
            errors.push(err);
        }
    }
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, omp_core::OmpError::QuotaExceeded { .. })),
        "expected at least one QuotaExceeded, got {errors:?}"
    );
}

// Helpers ---------------------------------------------------------------------

fn stage_starter_artifacts(repo: &Repo) {
    // Stage the schemas, probes, and omp.toml that `init` dropped into the
    // working tree so the first commit contains a usable snapshot.
    let root = repo.root().to_path_buf();
    let opts = omp_core::walker::WalkOptions::default();
    let entries = omp_core::walker::walk_repo(&root, &opts).unwrap();
    for e in entries {
        let bytes = std::fs::read(&e.fs_path).unwrap();
        repo.add(&e.repo_path, &bytes, None, None).unwrap();
    }
}

//! Schema-update auto-reprobe.
//!
//! See `docs/design/21-schema-reprobe.md`. The headline behaviour:
//! committing a schema change rebuilds every existing manifest of that
//! file_type in the same commit, so newly-added fields populate
//! retroactively. Old commits stay immutable; HEAD reflects the latest
//! schema for every file.

use std::collections::BTreeMap;

use omp_core::api::{AuthorOverride, Fields, Repo, ShowResult};
use omp_core::manifest::FieldValue;
use omp_core::probes::starter::STARTER_PROBES;
use tempfile::TempDir;

fn fixed_author() -> AuthorOverride {
    AuthorOverride {
        name: Some("test".into()),
        email: Some("test@local".into()),
        timestamp: Some("2026-04-22T00:00:00Z".into()),
    }
}

fn stage_blob(repo: &Repo, path: &str, bytes: &[u8]) {
    repo.add(path, bytes, Some(BTreeMap::new()), None)
        .unwrap_or_else(|e| panic!("stage {path}: {e}"));
}

/// Stage a user probe under `<namespace>/<basename>` reusing the bytes
/// from the starter `file.size` probe. The reprobe semantics are
/// identical to that probe — a fresh-byte clone under a new qualified
/// name. This avoids any external toolchain dependency in the test.
fn register_user_probe(repo: &Repo, namespace: &str, basename: &str) -> String {
    let starter = STARTER_PROBES
        .iter()
        .find(|p| p.name == "file.size")
        .expect("file.size in starter pack");

    let wasm_path = format!("probes/{namespace}/{basename}/probe.wasm");
    let toml_path = format!("probes/{namespace}/{basename}/probe.toml");

    let manifest_toml = format!(
        r#"name = "{namespace}.{basename}"
returns = "int"
accepts_kwargs = []
description = "Reprobe-test fixture probe (re-uses file.size bytes)."

[limits]
memory_mb = 32
fuel = 100000000
wall_clock_s = 5
"#
    );

    stage_blob(repo, &wasm_path, starter.wasm);
    stage_blob(repo, &toml_path, manifest_toml.as_bytes());
    format!("{namespace}.{basename}")
}

const TEXT_SCHEMA_V1: &str = r#"file_type = "text"
mime_patterns = ["text/*"]
allow_extra_fields = false

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"
"#;

fn text_schema_v2(probe_name: &str) -> String {
    format!(
        r#"file_type = "text"
mime_patterns = ["text/*"]
allow_extra_fields = false

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"

[fields.byte_size_v2]
source = "probe"
probe = "{probe_name}"
type = "int"
"#
    )
}

#[test]
fn schema_update_repopulates_existing_files() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    // Commit 1: just the v1 schema. Pre-condition for ingest.
    stage_blob(&repo, "schemas/text/schema.toml", TEXT_SCHEMA_V1.as_bytes());
    repo.commit("v1 schema", Some(fixed_author())).unwrap();

    // Commit 2: ingest two text files under v1.
    repo.add("a.txt", b"hello", Some(Fields::new()), None)
        .unwrap();
    repo.add("b.txt", b"world!", Some(Fields::new()), None)
        .unwrap();
    repo.commit("ingest two files", Some(fixed_author()))
        .unwrap();

    // Sanity: a.txt has only byte_size.
    let m = match repo.show("a.txt", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("not a manifest: {x:?}"),
    };
    assert!(m.fields.contains_key("byte_size"));
    assert!(!m.fields.contains_key("byte_size_v2"));

    // Commit 3: register a new user probe + update the schema to
    // reference it. The schema commit must auto-reprobe a.txt and b.txt.
    let probe_name = register_user_probe(&repo, "custom", "size_v2");
    repo.commit("add user probe", Some(fixed_author())).unwrap();
    let v2 = text_schema_v2(&probe_name);
    stage_blob(&repo, "schemas/text/schema.toml", v2.as_bytes());
    let (_hash, summaries) = repo
        .commit_with_summary("update text schema", Some(fixed_author()))
        .unwrap();

    // The summary should report 2 reprobed text files.
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].file_type, "text");
    assert_eq!(summaries[0].count, 2, "summary: {:?}", summaries);
    assert!(summaries[0].skipped.is_empty());

    // Both files now carry the new field.
    for path in ["a.txt", "b.txt"] {
        let m = match repo.show(path, None).unwrap() {
            ShowResult::Manifest { manifest, .. } => manifest,
            x => panic!("{path}: not a manifest: {x:?}"),
        };
        assert!(
            m.fields.contains_key("byte_size_v2"),
            "{path} missing byte_size_v2 — fields: {:?}",
            m.fields.keys().collect::<Vec<_>>()
        );
        // Probe re-uses file.size's bytes, so byte_size_v2 == byte_size.
        assert_eq!(m.fields["byte_size_v2"], m.fields["byte_size"]);
    }
}

#[test]
fn time_travel_returns_old_manifest_unchanged() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    stage_blob(&repo, "schemas/text/schema.toml", TEXT_SCHEMA_V1.as_bytes());
    repo.commit("v1 schema", Some(fixed_author())).unwrap();

    repo.add("a.txt", b"hello", Some(Fields::new()), None)
        .unwrap();
    let pre_commit = repo.commit("ingest", Some(fixed_author())).unwrap();
    let pre_manifest = match repo.show("a.txt", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("{x:?}"),
    };

    // Schema bump.
    let probe_name = register_user_probe(&repo, "custom", "size_v2");
    repo.commit("add user probe", Some(fixed_author())).unwrap();
    stage_blob(
        &repo,
        "schemas/text/schema.toml",
        text_schema_v2(&probe_name).as_bytes(),
    );
    repo.commit("schema v2", Some(fixed_author())).unwrap();

    // HEAD has the new field.
    let head_manifest = match repo.show("a.txt", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("{x:?}"),
    };
    assert!(head_manifest.fields.contains_key("byte_size_v2"));

    // Time-travel to the pre-bump commit returns the original manifest
    // bit-for-bit (same source_hash, same fields, same schema_hash).
    let at_pre = match repo.show("a.txt", Some(&pre_commit.hex())).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("{x:?}"),
    };
    assert_eq!(at_pre.fields, pre_manifest.fields);
    assert_eq!(at_pre.schema_hash, pre_manifest.schema_hash);
    assert!(!at_pre.fields.contains_key("byte_size_v2"));
}

#[test]
fn removed_field_drops_from_new_manifest() {
    // Schema v1 has byte_size + a redundant clone field. v2 removes the
    // clone. After the schema commit, existing manifests should not carry
    // the removed field.
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    // Probe must land in HEAD before the schema validator can accept a
    // schema referencing it. (Same two-commit dance as in dynamic_probes.rs.)
    let probe_name = register_user_probe(&repo, "custom", "size_dup");
    repo.commit("user probe", Some(fixed_author())).unwrap();
    let schema_v1 = format!(
        r#"file_type = "text"
mime_patterns = ["text/*"]
allow_extra_fields = false

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"

[fields.byte_size_dup]
source = "probe"
probe = "{probe_name}"
type = "int"
"#
    );
    stage_blob(&repo, "schemas/text/schema.toml", schema_v1.as_bytes());
    repo.commit("v1 schema with two fields", Some(fixed_author()))
        .unwrap();

    repo.add("a.txt", b"hi", Some(Fields::new()), None).unwrap();
    repo.commit("ingest", Some(fixed_author())).unwrap();
    let pre = match repo.show("a.txt", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("{x:?}"),
    };
    assert!(pre.fields.contains_key("byte_size_dup"));

    // Drop the field by replacing the schema with v1-style (no _dup).
    stage_blob(&repo, "schemas/text/schema.toml", TEXT_SCHEMA_V1.as_bytes());
    repo.commit("v2 schema drops byte_size_dup", Some(fixed_author()))
        .unwrap();
    let post = match repo.show("a.txt", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("{x:?}"),
    };
    assert!(post.fields.contains_key("byte_size"));
    assert!(
        !post.fields.contains_key("byte_size_dup"),
        "removed field still present: {:?}",
        post.fields
    );
}

#[test]
fn user_provided_field_carries_over() {
    // A user-provided field declared in v1 should carry over verbatim to
    // the v2 manifest (same field name, same Source::UserProvided).
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    let schema_v1 = r#"file_type = "text"
mime_patterns = ["text/*"]
allow_extra_fields = false

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"

[fields.author]
source = "user_provided"
type = "string"
"#;
    stage_blob(&repo, "schemas/text/schema.toml", schema_v1.as_bytes());
    repo.commit("v1", Some(fixed_author())).unwrap();

    let mut user_fields: Fields = BTreeMap::new();
    user_fields.insert("author".to_string(), FieldValue::String("alice".into()));
    repo.add("a.txt", b"x", Some(user_fields), None).unwrap();
    repo.commit("ingest", Some(fixed_author())).unwrap();

    // v2 adds a new probe-driven field but keeps `author` as user-provided.
    let probe_name = register_user_probe(&repo, "custom", "size_v2");
    repo.commit("user probe", Some(fixed_author())).unwrap();
    let schema_v2 = format!(
        r#"file_type = "text"
mime_patterns = ["text/*"]
allow_extra_fields = false

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"

[fields.byte_size_v2]
source = "probe"
probe = "{probe_name}"
type = "int"

[fields.author]
source = "user_provided"
type = "string"
"#
    );
    stage_blob(&repo, "schemas/text/schema.toml", schema_v2.as_bytes());
    repo.commit("v2", Some(fixed_author())).unwrap();

    let m = match repo.show("a.txt", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("{x:?}"),
    };
    assert_eq!(m.fields["author"], FieldValue::String("alice".into()));
    assert!(m.fields.contains_key("byte_size_v2"));
}

#[test]
fn reprobe_only_walks_committed_manifests() {
    // The reprobe pass walks HEAD's tree, not the staged index. So a file
    // freshly staged in the same commit as the schema bump is invisible
    // to reprobe — its manifest stays at whatever schema it was ingested
    // against (HEAD's schema at the time `repo.add` was called).
    //
    // This is a known v1 limitation. Users get auto-reprobe for everything
    // already in HEAD; in-commit fresh ingests for that file_type either
    // need a second commit OR users should commit the schema by itself
    // first. The test pins the behaviour so a future change is deliberate.
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    stage_blob(&repo, "schemas/text/schema.toml", TEXT_SCHEMA_V1.as_bytes());
    repo.commit("v1", Some(fixed_author())).unwrap();

    repo.add("a.txt", b"old", Some(Fields::new()), None)
        .unwrap();
    repo.commit("ingest a.txt under v1", Some(fixed_author()))
        .unwrap();

    // Stage v2 schema + a brand new file b.txt in the SAME commit.
    let probe_name = register_user_probe(&repo, "custom", "size_v2");
    repo.commit("user probe", Some(fixed_author())).unwrap();
    stage_blob(
        &repo,
        "schemas/text/schema.toml",
        text_schema_v2(&probe_name).as_bytes(),
    );
    repo.add("b.txt", b"new", Some(Fields::new()), None)
        .unwrap();
    let (_hash, summaries) = repo
        .commit_with_summary("v2 + ingest b", Some(fixed_author()))
        .unwrap();

    // Only a.txt was in HEAD when reprobe ran; b.txt wasn't yet committed.
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].count, 1, "summary: {:?}", summaries);

    let a = match repo.show("a.txt", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("{x:?}"),
    };
    assert!(
        a.fields.contains_key("byte_size_v2"),
        "a.txt should be reprobed"
    );

    let b = match repo.show("b.txt", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => manifest,
        x => panic!("{x:?}"),
    };
    assert!(
        !b.fields.contains_key("byte_size_v2"),
        "b.txt was ingested under HEAD's v1 schema and not reprobed in the same commit"
    );
}

// `OMP_DEFER_REPROBE=1` is documented as an operator escape hatch but isn't
// covered by integration tests here — toggling a global env var inside
// cargo's parallel test runner races with sibling tests. Manual smoke
// (set the env var, observe `summaries.is_empty()`) is sufficient given
// the implementation is a one-line check.

#[test]
fn cosmetic_schema_change_is_a_no_op() {
    // Re-staging the v1 schema bytes verbatim means the schema_hash
    // didn't change. The reprobe hook should detect this and NOT walk
    // any manifests.
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    stage_blob(&repo, "schemas/text/schema.toml", TEXT_SCHEMA_V1.as_bytes());
    repo.commit("v1", Some(fixed_author())).unwrap();

    repo.add("a.txt", b"hi", Some(Fields::new()), None).unwrap();
    repo.commit("ingest", Some(fixed_author())).unwrap();

    // Re-stage byte-identical schema. Different blob hash on disk? No —
    // same bytes deflate to the same blob hash. Even with the staging,
    // the reprobe hook compares parsed-Schema equality, so no rebuild.
    stage_blob(&repo, "schemas/text/schema.toml", TEXT_SCHEMA_V1.as_bytes());
    // The commit may legally fail with "no staged changes" if staging
    // was a no-op; tolerate either path.
    let res = repo.commit_with_summary("re-stage same v1", Some(fixed_author()));
    if let Ok((_h, summaries)) = res {
        assert!(
            summaries.is_empty(),
            "no schema change → no reprobe; got {:?}",
            summaries
        );
    }
}

//! Tree-resident probes are discovered at ingest.
//!
//! Without these tests the `omp-builder` work is meaningless — even a
//! perfectly built `.wasm` blob in the tree is inert if the engine doesn't
//! read it. See `docs/design/20-server-side-probes.md` §"The prerequisite".

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

/// Stage a blob at `path` with `bytes`. Wraps `Repo::add` so the tests
/// stay terse.
fn stage_blob(repo: &Repo, path: &str, bytes: &[u8]) {
    repo.add(path, bytes, Some(BTreeMap::new()), None)
        .unwrap_or_else(|e| panic!("stage {path}: {e}"));
}

/// Register a tree-resident probe at the given qualified name by reusing
/// the bytes of the starter `file.size` probe. Returns the dotted name
/// used to reference the probe from a schema.
///
/// We re-use the starter blob bytes rather than hand-building a fresh
/// `.wasm` so the test has no external toolchain dependency. Behaviour
/// under the new name is identical to `file.size` — exactly what a "user
/// uploaded a copy under a different name" scenario looks like.
fn register_user_probe(repo: &Repo, namespace: &str, basename: &str) -> String {
    let starter = STARTER_PROBES
        .iter()
        .find(|p| p.name == "file.size")
        .expect("file.size in starter pack");

    let wasm_path = format!("probes/{namespace}/{basename}.wasm");
    let toml_path = format!("probes/{namespace}/{basename}.probe.toml");

    let manifest_toml = format!(
        r#"name = "{namespace}.{basename}"
returns = "int"
accepts_kwargs = []
description = "Test probe — re-uses the file.size starter for fixture purposes."

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

/// A schema that adds a `byte_size_v2` field driven by a user-uploaded
/// probe at qualified name `<probe_name>`. Mirrors the shape of
/// `crates/omp-core/starter-schemas/text.schema`.
fn schema_with_user_probe(probe_name: &str) -> String {
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
fn user_uploaded_probe_runs_at_ingest() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    // Commit 1: user probe alone. After this commit HEAD's tree contains
    // probes/custom/bytecount.{wasm,probe.toml} so the schema validator
    // and the engine can find it.
    let probe_name = register_user_probe(&repo, "custom", "bytecount");
    assert_eq!(probe_name, "custom.bytecount");
    repo.commit("add user probe", Some(fixed_author())).unwrap();

    // Commit 2: schema referencing the user probe. Schema staging triggers
    // validation against current_probe_names() — which now sees the user
    // probe in HEAD because of Phase 1.
    let schema_bytes = schema_with_user_probe(&probe_name);
    stage_blob(&repo, "schemas/text.schema", schema_bytes.as_bytes());
    repo.commit("update text schema", Some(fixed_author())).unwrap();

    // Commit 3: ingest a text file. The new schema requires both
    // byte_size (file.size, starter) and byte_size_v2 (custom.bytecount,
    // tree-resident).
    let body = b"hello tree-resident probes";
    repo.add("hello.txt", body, Some(Fields::new()), None).unwrap();
    repo.commit("ingest with user probe", Some(fixed_author())).unwrap();

    let result = repo.show("hello.txt", None).unwrap();
    let manifest = match result {
        ShowResult::Manifest { manifest, .. } => manifest,
        other => panic!("expected Manifest, got {other:?}"),
    };

    let len = body.len() as i64;
    assert_eq!(manifest.fields["byte_size"], FieldValue::Int(len));
    assert_eq!(manifest.fields["byte_size_v2"], FieldValue::Int(len));

    // Manifest's probe_hashes table records BOTH the starter probe and
    // the tree-resident user probe, proving the engine looked up the
    // user one dynamically.
    assert!(
        manifest.probe_hashes.contains_key("file.size"),
        "starter probe hash recorded"
    );
    assert!(
        manifest.probe_hashes.contains_key(&probe_name),
        "user probe hash recorded; got keys: {:?}",
        manifest.probe_hashes.keys().collect::<Vec<_>>()
    );
}

#[test]
fn schema_validation_accepts_user_probe_once_committed() {
    // Without the dynamic loader, `Repo::add` of the schema below would
    // reject because `custom.bytecount` would not be in
    // `current_probe_names()`. The schema validator and the engine must
    // agree on what's loadable.
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    let probe_name = register_user_probe(&repo, "custom", "bytecount");
    repo.commit("user probe", Some(fixed_author())).unwrap();

    let schema_bytes = schema_with_user_probe(&probe_name);
    // This call must not raise SchemaValidation — a regression here means
    // current_probe_names() failed to discover the tree-resident probe.
    stage_blob(&repo, "schemas/text.schema", schema_bytes.as_bytes());
}

#[test]
fn user_probe_uncommitted_is_invisible_to_schema_validation() {
    // Probes load from HEAD's tree, not from staging. A schema referencing
    // a staged-but-uncommitted probe is rejected at validation time. This
    // is by design — `docs/design/20-server-side-probes.md` says the
    // engine reads probes off the current tree at the start of each
    // ingest. A future enhancement could relax this for staged probes;
    // for now the user does it as two commits.
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    let probe_name = register_user_probe(&repo, "custom", "uncommitted");
    // Note: NO commit between staging the probe and the schema.

    let schema_bytes = schema_with_user_probe(&probe_name);
    let result = repo.add(
        "schemas/text.schema",
        schema_bytes.as_bytes(),
        Some(BTreeMap::new()),
        None,
    );
    assert!(
        result.is_err(),
        "expected schema validation to reject the schema; got {:?}",
        result
    );
}

#[test]
fn starter_probes_in_tree_dedup_with_embedded_pack() {
    // Stage and commit the entire starter pack into the tree. The walker
    // will then find probes/file/{size,mime,sha256}.wasm at HEAD with the
    // exact framed_hash already registered by the embedded pack — so
    // current_probes() must dedup silently (no warning, same Borrowed
    // bytes still served).
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();

    for probe in STARTER_PROBES {
        stage_blob(&repo, &probe.tree_path_wasm(), probe.wasm);
        stage_blob(&repo, &probe.tree_path_manifest(), probe.manifest_toml);
    }
    repo.commit("seed starter pack into tree", Some(fixed_author()))
        .unwrap();

    // Plain ingest: the dedup path runs. If broken, ingest still
    // succeeds (Owned vs Borrowed yields identical behaviour) but the
    // test acts as a regression canary that current_probes() doesn't
    // explode when a starter probe also lives in the tree.
    let body = b"checks the dedup path";
    repo.add("note.txt", body, Some(Fields::new()), None).unwrap();
    repo.commit("ingest", Some(fixed_author())).unwrap();

    let result = repo.show("note.txt", None).unwrap();
    let manifest = match result {
        ShowResult::Manifest { manifest, .. } => manifest,
        other => panic!("expected Manifest, got {other:?}"),
    };
    assert_eq!(
        manifest.fields["byte_size"],
        FieldValue::Int(body.len() as i64)
    );
}

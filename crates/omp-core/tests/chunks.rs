//! End-to-end tests for the chunked-file pipeline from
//! `docs/design/12-large-files.md`.

use std::fs;

use omp_core::api::{AddResult, AuthorOverride, Fields, Repo};
use omp_core::chunks::ChunksBody;
use omp_core::manifest::{FieldValue, Manifest};
use omp_core::object::ObjectType;
use omp_core::store::disk::DiskStore;
use omp_core::store::ObjectStore;
use tempfile::TempDir;

fn fixed_author() -> AuthorOverride {
    AuthorOverride {
        name: Some("test".into()),
        email: Some("test@local".into()),
        timestamp: Some("2026-04-22T00:00:00Z".into()),
    }
}

fn stage_starter_artifacts(repo: &Repo) {
    let root = repo.root().to_path_buf();
    let opts = omp_core::walker::WalkOptions::default();
    let entries = omp_core::walker::walk_repo(&root, &opts).unwrap();
    for e in entries {
        let bytes = fs::read(&e.fs_path).unwrap();
        repo.add(&e.repo_path, &bytes, None, None).unwrap();
    }
}

/// Rewrite the repo's `omp.toml` with a given chunk size so tests don't
/// have to allocate 16 MiB buffers to cross the default threshold.
fn set_chunk_size(td: &TempDir, chunk_size_bytes: u64) {
    let body = format!(
        "[ingest]\ndefault_schema_policy = \"minimal\"\n\n[storage]\nchunk_size_bytes = {chunk_size_bytes}\n",
    );
    fs::write(td.path().join("omp.toml"), body).unwrap();
}

#[test]
fn small_file_uses_single_blob_path() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);

    // Default chunk_size_bytes = 16 MiB; 100-byte file falls well under.
    let res = repo
        .add("docs/small.md", b"tiny file content\n", None, Some("text"))
        .unwrap();
    let source_hash = match res {
        AddResult::Manifest { source_hash, .. } => source_hash,
        other => panic!("expected Manifest, got {other:?}"),
    };

    // Source hash should resolve to a `blob` — v1 behavior unchanged.
    let store = DiskStore::open(td.path()).unwrap();
    let (ty, _content) = store.get(&source_hash).unwrap().unwrap();
    assert_eq!(ty, "blob");
}

#[test]
fn large_file_produces_chunks_object() {
    let td = TempDir::new().unwrap();
    let _repo = Repo::init(td.path()).unwrap();

    // Reinit with a tiny chunk size so the test doesn't allocate megabytes.
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter_artifacts(&repo);

    // 200 bytes / 64-byte chunks ⇒ 4 chunks (64, 64, 64, 8).
    let big: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    let res = repo
        .add("data/big.bin", &big, None, Some("_minimal"))
        .unwrap();
    let source_hash = match res {
        AddResult::Manifest { source_hash, .. } => source_hash,
        other => panic!("expected Manifest, got {other:?}"),
    };

    let store = DiskStore::open(td.path()).unwrap();
    let (ty, body) = store.get(&source_hash).unwrap().unwrap();
    assert_eq!(ty, "chunks", "source_hash must resolve to a chunks object");

    let parsed = ChunksBody::parse(&body).unwrap();
    assert_eq!(parsed.entries.len(), 4, "expected 4 chunks, got {}", parsed.entries.len());
    // The chunk lengths should sum to the full file.
    assert_eq!(parsed.total_length(), 200);
    // Per-chunk sizes: three 64-byte + one 8-byte tail.
    assert_eq!(parsed.entries[0].length, 64);
    assert_eq!(parsed.entries[1].length, 64);
    assert_eq!(parsed.entries[2].length, 64);
    assert_eq!(parsed.entries[3].length, 8);

    // Each referenced chunk resolves to a blob with the right content.
    let expected_chunks: Vec<&[u8]> = vec![&big[..64], &big[64..128], &big[128..192], &big[192..]];
    for (entry, expected) in parsed.entries.iter().zip(expected_chunks.iter()) {
        let (ty, content) = store.get(&entry.hash).unwrap().unwrap();
        assert_eq!(ty, "blob");
        assert_eq!(content.as_slice(), *expected);
    }
}

#[test]
fn streaming_builtin_sha256_in_manifest_for_large_file() {
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter_artifacts(&repo);

    // Define a schema whose only field is backed by `file.sha256` so we can
    // assert the streaming built-in value lands in the manifest.
    let schema = br#"file_type = "bin"
mime_patterns = ["application/octet-stream"]

[fields.sha]
source = "probe"
probe = "file.sha256"
type = "string"
"#;
    repo.add("schemas/bin.schema", schema, None, None).unwrap();
    repo.commit("add bin schema", Some(fixed_author())).unwrap();

    // 150 bytes ⇒ 3 chunks; content_length = 150 > all probe caps on the
    // default limits (memory_mb=64 MiB, so 150 < 64 MiB — but the chunked
    // path disables all non-streaming probes regardless).
    let big: Vec<u8> = (0..150u32).map(|i| (i & 0xff) as u8).collect();
    repo.add("data/big.bin", &big, None, Some("bin")).unwrap();
    repo.commit(
        "add big",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    // SHA-256 of the plaintext, hex-lowercase.
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(&big);
    let expected_hex = digest
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    // Show the manifest and check the field resolved to the streaming value.
    use omp_core::api::ShowResult;
    match repo.show("data/big.bin", None).unwrap() {
        ShowResult::Manifest { manifest, .. } => {
            let field = manifest.fields.get("sha").unwrap();
            let got = match field {
                FieldValue::String(s) => s.clone(),
                other => panic!("expected string, got {other:?}"),
            };
            assert_eq!(got, expected_hex);
            // Chunked path doesn't record probe_hashes — the WASM probe
            // did not fire.
            assert!(
                !manifest.probe_hashes.contains_key("file.sha256"),
                "probe_hashes must not include a probe that didn't run"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn put_stream_roundtrip_matches_put_hash() {
    // Sanity check on the new `ObjectStore::put_stream` method.
    let td = TempDir::new().unwrap();
    let s = DiskStore::init(td.path()).unwrap();
    let content = vec![0x5au8; 2 * 1024 * 1024]; // 2 MiB
    let h_put = s.put("blob", &content).unwrap();
    let td2 = TempDir::new().unwrap();
    let s2 = DiskStore::init(td2.path()).unwrap();
    let mut cur = std::io::Cursor::new(content.clone());
    let h_stream = s2
        .put_stream("blob", &mut cur, content.len() as u64)
        .unwrap();
    assert_eq!(h_put, h_stream);
    let (ty, back) = s2.get(&h_stream).unwrap().unwrap();
    assert_eq!(ty, "blob");
    assert_eq!(back, content);
}

#[test]
fn chunks_object_framing_pinned() {
    // Direct assertion against the hash the design doc's framing guarantees.
    use omp_core::{hash_of, Hash};
    let body = b""; // empty chunks object
    let h = hash_of(ObjectType::Chunks, body);
    assert_eq!(h, Hash::of(b"chunks 0\0"));

    // Non-empty pinning.
    let body = b"abc\n";
    let h2 = hash_of(ObjectType::Chunks, body);
    assert_eq!(h2, Hash::of(b"chunks 4\0abc\n"));
}

#[test]
fn chunked_ingest_then_commit_preserves_source_hash() {
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 128);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter_artifacts(&repo);

    let big: Vec<u8> = (0..500u32).map(|i| (i & 0xff) as u8).collect();
    let add_result = repo
        .add("data/big.bin", &big, None, Some("_minimal"))
        .unwrap();
    let expected_source = match add_result {
        AddResult::Manifest { source_hash, .. } => source_hash,
        other => panic!("expected Manifest, got {other:?}"),
    };

    repo.commit("commit big", Some(fixed_author())).unwrap();

    // After commit, show must still report the same chunks-object source_hash.
    use omp_core::api::ShowResult;
    let shown = repo.show("data/big.bin", None).unwrap();
    match shown {
        ShowResult::Manifest { manifest, .. } => {
            assert_eq!(manifest.source_hash, expected_source);
            let (ty, _) = DiskStore::open(td.path())
                .unwrap()
                .get(&manifest.source_hash)
                .unwrap()
                .unwrap();
            assert_eq!(ty, "chunks");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn manifest_serialization_still_canonical_under_chunks() {
    // Regression: the manifest wire format doesn't change for chunked files.
    // The only observable difference is that `source_hash` happens to point
    // at a `chunks` object rather than a `blob`.
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter_artifacts(&repo);

    let big: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    let res = repo
        .add("data/big.bin", &big, Some(Fields::new()), Some("_minimal"))
        .unwrap();
    let manifest_hash = match res {
        AddResult::Manifest { manifest_hash, .. } => manifest_hash,
        other => panic!("expected Manifest, got {other:?}"),
    };

    let store = DiskStore::open(td.path()).unwrap();
    let (ty, body) = store.get(&manifest_hash).unwrap().unwrap();
    assert_eq!(ty, "manifest");
    // Parse the manifest bytes and verify re-serializing is byte-identical.
    let parsed = Manifest::parse(&body).unwrap();
    let reserialized = parsed.serialize().unwrap();
    assert_eq!(reserialized, body);
}

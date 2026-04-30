//! Composition tests: chunked ingest + end-to-end encryption.
//! See `docs/design/12-large-files.md §Interaction with large files` and
//! `docs/design/13-end-to-end-encryption.md §Interaction with large files`.

use std::fs;

use omp_core::api::{AddResult, AuthorOverride, Repo};
use omp_core::chunks::ChunksBody;
use omp_core::keys::TenantKeys;
use omp_core::store::disk::DiskStore;
use omp_core::store::ObjectStore;
use omp_core::tenant::TenantId;
use tempfile::TempDir;

fn fixed_author() -> AuthorOverride {
    AuthorOverride {
        name: Some("test".into()),
        email: Some("test@local".into()),
        timestamp: Some("2026-04-22T00:00:00Z".into()),
    }
}

fn stage_starter(repo: &Repo) {
    let root = repo.root().to_path_buf();
    let opts = omp_core::walker::WalkOptions::default();
    let entries = omp_core::walker::walk_repo(&root, &opts).unwrap();
    for e in entries {
        let bytes = fs::read(&e.fs_path).unwrap();
        repo.add(&e.repo_path, &bytes, None, None).unwrap();
    }
}

/// Tiny chunk size + reject policy, so encrypted-chunked tests trigger
/// chunking on small inputs.
fn set_chunk_size(td: &TempDir, chunk_size_bytes: u64) {
    let body = format!(
        "[ingest]\ndefault_schema_policy = \"minimal\"\n\n[storage]\nchunk_size_bytes = {chunk_size_bytes}\n",
    );
    fs::write(td.path().join("omp.toml"), body).unwrap();
}

fn make_keys(tenant: &TenantId) -> TenantKeys {
    TenantKeys::unlock(b"correct horse battery staple", tenant).unwrap()
}

#[test]
fn encrypted_chunked_roundtrip() {
    let td = TempDir::new().unwrap();
    let _repo = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let tenant = TenantId::local();
    let keys = make_keys(&tenant);

    // 200 bytes / 64-byte chunks ⇒ 4 chunks (64, 64, 64, 8) of plaintext.
    // Each ciphertext chunk is plaintext + 1-byte alg + 12-byte nonce + 16-byte tag.
    let plaintext: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();

    let res = repo
        .add_encrypted("big.bin", &plaintext, None, Some("text"), &keys)
        .unwrap();
    let source_hash = match res {
        AddResult::Manifest { source_hash, .. } => source_hash,
        other => panic!("expected Manifest, got {other:?}"),
    };
    repo.commit(
        "add",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    // source_hash points at a chunks object, whose body is plaintext.
    let store = DiskStore::open(td.path()).unwrap();
    let (ty, body) = store.get(&source_hash).unwrap().unwrap();
    assert_eq!(
        ty, "chunks",
        "encrypted chunked ingest still produces a chunks object"
    );
    let parsed = ChunksBody::parse(&body).unwrap();
    assert_eq!(
        parsed.entries.len(),
        4,
        "four chunks for a 200-byte file at 64/chunk"
    );

    // Each referenced chunk's content length is (plaintext + 29) — the 29
    // bytes are the aead framing: 1 (alg) + 12 (nonce) + 16 (tag).
    let plaintext_sizes = [64usize, 64, 64, 8];
    for (entry, plain) in parsed.entries.iter().zip(plaintext_sizes.iter()) {
        assert_eq!(
            entry.length as usize,
            plain + 1 + 12 + 16,
            "chunk ciphertext length = plaintext + aead framing"
        );
    }

    // Client reads back the plaintext.
    let (manifest, recovered) = repo.show_encrypted("big.bin", None, &keys).unwrap();
    assert_eq!(recovered, plaintext);
    assert_eq!(manifest.source_hash, source_hash);
}

#[test]
fn chunks_body_is_plaintext_under_encryption() {
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let keys = make_keys(&TenantId::local());
    let big: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    let res = repo
        .add_encrypted("big.bin", &big, None, Some("text"), &keys)
        .unwrap();
    let source_hash = match res {
        AddResult::Manifest { source_hash, .. } => source_hash,
        other => panic!("{other:?}"),
    };

    let store = DiskStore::open(td.path()).unwrap();
    let (ty, body) = store.get(&source_hash).unwrap().unwrap();
    assert_eq!(ty, "chunks");
    // Plaintext TOML-style lines — no key required to parse.
    let _parsed = ChunksBody::parse(&body).unwrap();
}

#[test]
fn tampering_a_chunk_fails_open() {
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let keys = make_keys(&TenantId::local());
    let big: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    repo.add_encrypted("big.bin", &big, None, Some("text"), &keys)
        .unwrap();
    repo.commit(
        "add",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    // Find one of the chunk objects on disk and flip a byte in it.
    let objects_dir = td.path().join(".omp/objects");
    let mut flipped = false;
    'outer: for bucket in fs::read_dir(&objects_dir).unwrap() {
        let bucket = bucket.unwrap();
        if !bucket.file_type().unwrap().is_dir() || bucket.file_name() == "tmp" {
            continue;
        }
        for entry in fs::read_dir(bucket.path()).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let bytes = fs::read(&path).unwrap();
            // Decompress to see if it's a blob object whose plaintext length
            // is 64 + 29 = 93 bytes (one of our chunk ciphertexts).
            if let Ok(framed) = omp_core::object::decompress(&bytes) {
                if let Ok((ty, content)) = omp_core::object::parse_framed(&framed) {
                    if ty == omp_core::ObjectType::Blob && content.len() == 93 {
                        // Flip a byte in the on-disk compressed file — the
                        // decompress step will still work, but the AEAD
                        // tag on the embedded chunk ciphertext will no
                        // longer match.
                        //
                        // Easier: rewrite the object with tampered content.
                        // We'll corrupt one byte of the ciphertext and
                        // re-compress.
                        let mut new_content = content.to_vec();
                        let mid = new_content.len() / 2;
                        new_content[mid] ^= 0x01;
                        let new_framed =
                            omp_core::object::frame(omp_core::ObjectType::Blob, &new_content);
                        let new_compressed =
                            omp_core::object::compress_framed(&new_framed).unwrap();
                        fs::write(&path, &new_compressed).unwrap();
                        flipped = true;
                        break 'outer;
                    }
                }
            }
            let _ = bytes;
        }
    }
    assert!(flipped, "did not find any chunk ciphertext to tamper");

    // Reading the file must fail with Unauthorized (AEAD auth-tag mismatch).
    let err = repo.show_encrypted("big.bin", None, &keys).unwrap_err();
    assert!(
        matches!(err, omp_core::OmpError::Unauthorized(_)),
        "expected Unauthorized on tamper, got {err:?}"
    );
}

#[test]
fn reordering_chunks_fails_open() {
    // Attacker with write access to the object store swaps the on-disk
    // contents of two chunk blobs. Without position binding in the AAD each
    // swapped ciphertext still opens cleanly under the content key, so the
    // client would receive a silently reordered plaintext. The AAD binds
    // `(chunk_index, total_chunks)`, so the swap must cause AEAD failure.
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let keys = make_keys(&TenantId::local());
    let plaintext: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    repo.add_encrypted("big.bin", &plaintext, None, Some("text"), &keys)
        .unwrap();
    repo.commit(
        "add",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    // Find two full-size chunk blobs (plaintext 64 → ciphertext 93) and swap
    // their on-disk file contents.
    let objects_dir = td.path().join(".omp/objects");
    let mut full_chunk_paths: Vec<std::path::PathBuf> = Vec::new();
    for bucket in fs::read_dir(&objects_dir).unwrap() {
        let bucket = bucket.unwrap();
        if !bucket.file_type().unwrap().is_dir() || bucket.file_name() == "tmp" {
            continue;
        }
        for entry in fs::read_dir(bucket.path()).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let bytes = fs::read(&path).unwrap();
            if let Ok(framed) = omp_core::object::decompress(&bytes) {
                if let Ok((ty, content)) = omp_core::object::parse_framed(&framed) {
                    if ty == omp_core::ObjectType::Blob && content.len() == 93 {
                        full_chunk_paths.push(path);
                    }
                }
            }
        }
    }
    assert!(
        full_chunk_paths.len() >= 2,
        "need at least two full-size chunk blobs to swap, found {}",
        full_chunk_paths.len()
    );
    let a_bytes = fs::read(&full_chunk_paths[0]).unwrap();
    let b_bytes = fs::read(&full_chunk_paths[1]).unwrap();
    fs::write(&full_chunk_paths[0], &b_bytes).unwrap();
    fs::write(&full_chunk_paths[1], &a_bytes).unwrap();

    let err = repo.show_encrypted("big.bin", None, &keys).unwrap_err();
    assert!(
        matches!(err, omp_core::OmpError::Unauthorized(_)),
        "expected Unauthorized on chunk reorder, got {err:?}"
    );
}

#[test]
fn truncating_chunks_body_fails_open() {
    // Attacker drops the trailing entry of the chunks-body. Position
    // binding alone wouldn't catch this (remaining entries are still at
    // their original positions) — binding `total_chunks` into the AAD is
    // what makes the shortened list fail AEAD.
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();
    stage_starter(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let keys = make_keys(&TenantId::local());
    // 200 bytes / 64-byte chunks → 4 chunks.
    let plaintext: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    let res = repo
        .add_encrypted("big.bin", &plaintext, None, Some("text"), &keys)
        .unwrap();
    let source_hash = match res {
        AddResult::Manifest { source_hash, .. } => source_hash,
        other => panic!("expected Manifest, got {other:?}"),
    };
    repo.commit(
        "add",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    // Rewrite the chunks-body object at its original on-disk path with the
    // last entry removed. `Store::get` returns bytes without re-hashing, so
    // the manifest still resolves the (now-mismatched) object.
    let hex = source_hash.hex();
    let path = td
        .path()
        .join(".omp/objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    let bytes = fs::read(&path).unwrap();
    let framed = omp_core::object::decompress(&bytes).unwrap();
    let (ty, content) = omp_core::object::parse_framed(&framed).unwrap();
    assert_eq!(ty, omp_core::ObjectType::Chunks);
    let mut parsed = ChunksBody::parse(content).unwrap();
    assert_eq!(parsed.entries.len(), 4);
    parsed.entries.pop();
    let new_content = parsed.serialize();
    let new_framed = omp_core::object::frame(omp_core::ObjectType::Chunks, &new_content);
    let new_compressed = omp_core::object::compress_framed(&new_framed).unwrap();
    fs::write(&path, &new_compressed).unwrap();

    let err = repo.show_encrypted("big.bin", None, &keys).unwrap_err();
    assert!(
        matches!(err, omp_core::OmpError::Unauthorized(_)),
        "expected Unauthorized on chunks-body truncation, got {err:?}"
    );
}

#[test]
fn streaming_sha256_is_over_plaintext_under_encryption() {
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    set_chunk_size(&td, 64);
    let repo = Repo::open(td.path()).unwrap();

    // Custom schema so we can assert the `file.sha256` field resolves via
    // the streaming built-in (which is computed over the plaintext).
    stage_starter(&repo);
    let bin_schema = br#"file_type = "bin"
mime_patterns = ["application/octet-stream"]

[fields.sha]
source = "probe"
probe = "file.sha256"
type = "string"
"#;
    repo.add("schemas/bin/schema.toml", bin_schema, None, None)
        .unwrap();
    repo.commit("init", Some(fixed_author())).unwrap();

    let keys = make_keys(&TenantId::local());
    let plaintext: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    repo.add_encrypted("big.bin", &plaintext, None, Some("bin"), &keys)
        .unwrap();
    repo.commit(
        "add",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    use sha2::{Digest, Sha256};
    let expected_hex = Sha256::digest(&plaintext)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let (manifest, _plain) = repo.show_encrypted("big.bin", None, &keys).unwrap();
    let sha = manifest.fields.get("sha").unwrap();
    use omp_core::manifest::FieldValue;
    match sha {
        FieldValue::String(s) => assert_eq!(s, &expected_hex),
        other => panic!("expected string, got {other:?}"),
    }
}

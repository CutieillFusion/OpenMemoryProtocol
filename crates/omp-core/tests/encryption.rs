//! End-to-end tests for the encrypted ingest path from
//! `docs/design/13-end-to-end-encryption.md`.

use std::fs;

use omp_core::api::{AddResult, AuthorOverride, Fields, Repo};
use omp_core::keys::TenantKeys;
use omp_core::manifest::FieldValue;
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

fn stage_starter_artifacts(repo: &Repo) {
    let root = repo.root().to_path_buf();
    let opts = omp_core::walker::WalkOptions::default();
    let entries = omp_core::walker::walk_repo(&root, &opts).unwrap();
    for e in entries {
        let bytes = fs::read(&e.fs_path).unwrap();
        repo.add(&e.repo_path, &bytes, None, None).unwrap();
    }
}

fn make_keys(tenant: &TenantId) -> TenantKeys {
    // Argon2id at 64 MiB / t=3 is slow but correct; tests pay it once.
    TenantKeys::unlock(b"correct horse battery staple", tenant).unwrap()
}

#[test]
fn encrypted_small_file_roundtrip() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let tenant = TenantId::local();
    let keys = make_keys(&tenant);

    let plaintext = b"sealed contents of the user's file\n".to_vec();
    let res = repo
        .add_encrypted("secrets/note.txt", &plaintext, None, Some("text"), &keys)
        .unwrap();
    let (manifest_hash, source_hash) = match res {
        AddResult::Manifest {
            manifest_hash,
            source_hash,
            ..
        } => (manifest_hash, source_hash),
        other => panic!("expected Manifest, got {other:?}"),
    };
    repo.commit(
        "add encrypted note",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    // On disk: the object store holds ciphertext. Open the blob directly
    // and confirm it doesn't contain the plaintext.
    let store = DiskStore::open(td.path()).unwrap();
    let (ty, ciphertext) = store.get(&source_hash).unwrap().unwrap();
    assert_eq!(ty, "blob");
    assert!(
        ciphertext.windows(plaintext.len()).all(|w| w != plaintext),
        "plaintext bytes visible in ciphertext blob"
    );

    // The envelope is a valid manifest-type object but its TOML includes
    // no plaintext from the file and no plaintext from the Manifest body.
    let (mty, env_bytes) = store.get(&manifest_hash).unwrap().unwrap();
    assert_eq!(mty, "manifest");
    let env_str = std::str::from_utf8(&env_bytes).unwrap();
    assert!(!env_str.contains("sealed contents of the user"));
    // file_type shouldn't leak either — it's inside the sealed body.
    assert!(!env_str.contains("text"), "file_type leaked into envelope: {env_str}");

    // Reading back with the right keys recovers the plaintext + Manifest.
    let (manifest, recovered) = repo.show_encrypted("secrets/note.txt", None, &keys).unwrap();
    assert_eq!(recovered, plaintext);
    assert_eq!(manifest.file_type, "text");
    assert_eq!(manifest.source_hash, source_hash);
}

#[test]
fn encrypted_manifest_has_wrapped_content_key() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let keys = make_keys(&TenantId::local());
    let plaintext = b"hi";
    let res = repo
        .add_encrypted("a.txt", plaintext, None, Some("text"), &keys)
        .unwrap();
    let manifest_hash = match res {
        AddResult::Manifest { manifest_hash, .. } => manifest_hash,
        other => panic!("expected Manifest, got {other:?}"),
    };

    // Parse the stored envelope and confirm it carries the expected shape.
    let store = DiskStore::open(td.path()).unwrap();
    let (_ty, bytes) = store.get(&manifest_hash).unwrap().unwrap();
    let env = omp_core::encrypted_manifest::EncryptedManifestEnvelope::parse(&bytes).unwrap();
    assert_eq!(env.alg, "chacha20poly1305");
    assert!(!env.wrapped_content_key.is_empty());
    assert!(!env.sealed_body.is_empty());
}

#[test]
fn wrong_passphrase_cannot_decrypt() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let tenant = TenantId::local();
    let right_keys = make_keys(&tenant);
    repo.add_encrypted(
        "x.txt",
        b"secret contents",
        None,
        Some("text"),
        &right_keys,
    )
    .unwrap();
    repo.commit(
        "add",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    let wrong_keys = TenantKeys::unlock(b"wrong passphrase right here", &tenant).unwrap();
    let err = repo.show_encrypted("x.txt", None, &wrong_keys).unwrap_err();
    assert!(
        matches!(err, omp_core::OmpError::Unauthorized(_)),
        "expected Unauthorized, got {err:?}"
    );
}

#[test]
fn share_wraps_to_recipient_and_recipient_can_decrypt() {
    use omp_crypto::identity::generate_identity;

    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let alice_tenant = TenantId::new("alice").unwrap();
    let bob_tenant = TenantId::new("bob").unwrap();

    let alice_keys = make_keys(&alice_tenant);
    let (bob_priv, bob_pub) = generate_identity();

    // Alice ingests a file under her keys.
    let plaintext = b"alice's confidential plan\n".to_vec();
    repo.add_encrypted(
        "plans/q3.txt",
        &plaintext,
        None,
        Some("text"),
        &alice_keys,
    )
    .unwrap();
    repo.commit(
        "alice adds plan",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    // Alice creates a share addressed to Bob's X25519 public key.
    let share_hash = repo
        .create_share(
            "plans/q3.txt",
            None,
            &alice_keys,
            &[(bob_tenant.clone(), bob_pub)],
        )
        .unwrap();

    // Bob applies the share to recover the content key + for_hash.
    let (for_hash, recovered_content_key) = repo
        .apply_share(&share_hash, &bob_tenant, &bob_priv)
        .unwrap();

    // With content_key, Bob decrypts the referenced object directly.
    use omp_crypto::{aead, chunk_nonce};
    let store = DiskStore::open(td.path()).unwrap();
    let (ty, ciphertext) = store.get(&for_hash).unwrap().unwrap();
    assert_eq!(ty, "blob", "single-blob source");
    let _nonce = chunk_nonce::nonce_for_chunk(&recovered_content_key, 0).unwrap();
    // AEAD::open extracts the nonce from the framed blob; we just need the
    // content key to match.
    let bob_plain = aead::open(&recovered_content_key, b"omp-blob", &ciphertext).unwrap();
    assert_eq!(bob_plain, plaintext);
}

#[test]
fn share_refuses_non_recipient() {
    use omp_crypto::identity::generate_identity;

    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let alice_tenant = TenantId::new("alice").unwrap();
    let bob_tenant = TenantId::new("bob").unwrap();
    let carol_tenant = TenantId::new("carol").unwrap();
    let alice_keys = make_keys(&alice_tenant);
    let (_bob_priv, bob_pub) = generate_identity();
    let (carol_priv, _carol_pub) = generate_identity();

    repo.add_encrypted(
        "x.txt",
        b"private data",
        None,
        Some("text"),
        &alice_keys,
    )
    .unwrap();
    repo.commit(
        "init",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    let share_hash = repo
        .create_share(
            "x.txt",
            None,
            &alice_keys,
            // Only Bob is on the recipient list.
            &[(bob_tenant.clone(), bob_pub)],
        )
        .unwrap();

    // Carol tries to apply the share — she's not on the list.
    let err = repo
        .apply_share(&share_hash, &carol_tenant, &carol_priv)
        .unwrap_err();
    assert!(
        matches!(err, omp_core::OmpError::Unauthorized(_)),
        "expected Unauthorized, got {err:?}"
    );
}

#[test]
fn field_resolution_runs_client_side_on_plaintext() {
    // The engine runs on the client against plaintext; user-provided
    // fields and constant fields resolve normally. We just prove the
    // resolved manifest carries a user-provided field that we passed in.
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    stage_starter_artifacts(&repo);
    repo.commit("init", Some(fixed_author())).unwrap();

    let keys = make_keys(&TenantId::local());
    let mut fields = Fields::new();
    fields.insert("title".into(), FieldValue::String("User Title".into()));

    repo.add_encrypted(
        "doc.md",
        b"# hello\n\nbody\n",
        Some(fields),
        Some("text"),
        &keys,
    )
    .unwrap();
    repo.commit(
        "add",
        Some(AuthorOverride {
            timestamp: Some("2026-04-22T01:00:00Z".into()),
            ..fixed_author()
        }),
    )
    .unwrap();

    let (manifest, _plain) = repo.show_encrypted("doc.md", None, &keys).unwrap();
    assert_eq!(
        manifest.fields.get("title"),
        Some(&FieldValue::String("User Title".into()))
    );
}

//! End-to-end tests for the remaining doc 13 items: tree-entry-name
//! encryption (`path_key`), commit-message encryption (`commit_key`), and
//! share revocation by rewriting the underlying source.

use std::fs;

use omp_core::api::{AuthorOverride, Fields, Repo};
use omp_core::keys::TenantKeys;
use omp_core::store::disk::DiskStore;
use omp_core::store::ObjectStore;
use omp_core::tenant::TenantId;
use tempfile::TempDir;

fn fixed_author() -> AuthorOverride {
    AuthorOverride {
        name: Some("test".into()),
        email: Some("test@local".into()),
        timestamp: Some("2026-04-23T00:00:00Z".into()),
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

fn make_keys(tenant: &TenantId) -> TenantKeys {
    TenantKeys::unlock(b"correct horse battery staple", tenant).unwrap()
}

#[test]
fn encrypted_commit_hides_tree_names_from_server_view() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    let keys = make_keys(&TenantId::local());

    // Stage starter and commit via the encrypted path so every reachable
    // tree is encrypted from the root.
    stage_starter(&repo);
    repo.commit_encrypted("init", Some(fixed_author()), &keys)
        .unwrap();

    let plaintext = b"some content\n";
    repo.add_encrypted(
        "quarterly/secret-earnings.md",
        plaintext,
        None,
        Some("text"),
        &keys,
    )
    .unwrap();
    let commit_hash = repo
        .commit_encrypted(
            "add secret",
            Some(AuthorOverride {
                timestamp: Some("2026-04-23T01:00:00Z".into()),
                ..fixed_author()
            }),
            &keys,
        )
        .unwrap();

    // Find the tree object from the commit headers — that part is
    // plaintext even for encrypted commits.
    let store = DiskStore::open(td.path()).unwrap();
    let (_ty, commit_body) = store.get(&commit_hash).unwrap().unwrap();
    let commit_str = std::str::from_utf8(&commit_body).unwrap();
    let tree_line = commit_str.lines().find(|l| l.starts_with("tree ")).unwrap();
    let tree_hash: omp_core::Hash = tree_line.strip_prefix("tree ").unwrap().parse().unwrap();

    // Walk every tree reachable from that root and confirm no plaintext
    // name appears in any tree body.
    let mut to_visit = vec![tree_hash];
    let mut seen = std::collections::HashSet::new();
    while let Some(h) = to_visit.pop() {
        if !seen.insert(h) {
            continue;
        }
        let (ty, body) = store.get(&h).unwrap().unwrap();
        if ty != "tree" {
            continue;
        }
        let s = std::str::from_utf8(&body).unwrap();
        assert!(
            !s.contains("secret-earnings.md"),
            "plaintext filename leaked in tree: {s}"
        );
        assert!(
            !s.contains("quarterly"),
            "plaintext directory name leaked in tree: {s}"
        );
        // The tree should carry the `!encrypted-path v1` marker.
        assert!(
            s.starts_with("!encrypted-path v1"),
            "encrypted commit produced plaintext tree: {s:?}"
        );
        // Recurse into any sub-tree hashes we can parse from mode+hash
        // lines (names are ciphertext but modes are still plaintext).
        for line in s.lines().skip(1) {
            if let Some(rest) = line.strip_prefix("tree ") {
                if let Some((hash_part, _)) = rest.split_once('\t') {
                    if let Ok(h) = hash_part.parse::<omp_core::Hash>() {
                        to_visit.push(h);
                    }
                }
            }
        }
    }

    // Decrypting with the keys recovers the plaintext content.
    let (_m, recovered) = repo
        .show_encrypted("quarterly/secret-earnings.md", None, &keys)
        .unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn encrypted_commit_message_is_ciphertext() {
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    let keys = make_keys(&TenantId::local());
    stage_starter(&repo);
    repo.commit_encrypted("init", Some(fixed_author()), &keys)
        .unwrap();

    repo.add_encrypted("a.txt", b"x", None, Some("text"), &keys)
        .unwrap();
    let commit_hash = repo
        .commit_encrypted(
            "UNIQUE-PLAINTEXT-MARKER-4242",
            Some(AuthorOverride {
                timestamp: Some("2026-04-23T01:00:00Z".into()),
                ..fixed_author()
            }),
            &keys,
        )
        .unwrap();

    let store = DiskStore::open(td.path()).unwrap();
    let (_ty, body) = store.get(&commit_hash).unwrap().unwrap();
    let s = std::str::from_utf8(&body).unwrap();

    // Headers stay readable.
    assert!(s.contains("tree "));
    assert!(s.contains("author test"));
    // Message marker present, plaintext not.
    assert!(s.contains("!encrypted-message v1 "));
    assert!(
        !s.contains("UNIQUE-PLAINTEXT-MARKER-4242"),
        "commit message leaked into ciphertext: {s}"
    );

    // Parsing with the commit_key recovers the message.
    use omp_core::commit::Commit;
    let parsed = Commit::parse_with_commit_key(&body, Some(&keys.commit_key)).unwrap();
    assert_eq!(parsed.message, "UNIQUE-PLAINTEXT-MARKER-4242");
}

#[test]
fn share_revoke_rewrites_source_and_issues_new_share() {
    use omp_crypto::identity::generate_identity;

    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    let alice_tenant = TenantId::new("alice").unwrap();
    let bob_tenant = TenantId::new("bob").unwrap();
    let carol_tenant = TenantId::new("carol").unwrap();
    let alice_keys = make_keys(&alice_tenant);
    let (bob_priv, bob_pub) = generate_identity();
    let (carol_priv, carol_pub) = generate_identity();

    stage_starter(&repo);
    repo.commit_encrypted("init", Some(fixed_author()), &alice_keys)
        .unwrap();

    let plaintext = b"shared contents";
    repo.add_encrypted("note.md", plaintext, None, Some("text"), &alice_keys)
        .unwrap();
    repo.commit_encrypted(
        "add",
        Some(AuthorOverride {
            timestamp: Some("2026-04-23T01:00:00Z".into()),
            ..fixed_author()
        }),
        &alice_keys,
    )
    .unwrap();

    // Initial grant: Bob + Carol.
    let original_share = repo
        .create_share(
            "note.md",
            None,
            &alice_keys,
            &[
                (bob_tenant.clone(), bob_pub),
                (carol_tenant.clone(), carol_pub),
            ],
        )
        .unwrap();

    // Revoke: keep only Bob.
    let new_share = repo
        .revoke_share("note.md", &alice_keys, &[(bob_tenant.clone(), bob_pub)])
        .unwrap();
    assert_ne!(original_share, new_share);

    // Commit so the new manifest is reachable through HEAD.
    repo.commit_encrypted(
        "revoke carol",
        Some(AuthorOverride {
            timestamp: Some("2026-04-23T02:00:00Z".into()),
            ..fixed_author()
        }),
        &alice_keys,
    )
    .unwrap();

    // Bob can still decrypt: his public key is in the new share, and the
    // share points at the re-encrypted source.
    let (bob_for_hash, bob_content_key) = repo
        .apply_share(&new_share, &bob_tenant, &bob_priv)
        .unwrap();
    use omp_crypto::aead;
    let store = DiskStore::open(td.path()).unwrap();
    let (ty, bob_ct) = store.get(&bob_for_hash).unwrap().unwrap();
    assert_eq!(ty, "blob");
    let bob_pt = aead::open(&bob_content_key, b"omp-blob", &bob_ct).unwrap();
    assert_eq!(bob_pt, plaintext);

    // Carol is no longer a recipient — apply_share fails.
    let err = repo
        .apply_share(&new_share, &carol_tenant, &carol_priv)
        .unwrap_err();
    assert!(matches!(err, omp_core::OmpError::Unauthorized(_)));

    // Alice's current view decrypts to the same plaintext under the new
    // content key (which was rotated during revoke).
    let (_m, recovered) = repo.show_encrypted("note.md", None, &alice_keys).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn identity_persistence_roundtrip_end_to_end() {
    // Establish a repo with an encrypted identity, drop the keys, unlock
    // a fresh session with the same passphrase + on-disk identity, and
    // confirm the original public key reappears.
    let td = TempDir::new().unwrap();
    Repo::init(td.path()).unwrap();

    let tenant = TenantId::local();
    let mut original = TenantKeys::unlock(b"secret-pass", &tenant).unwrap();
    let pub_orig = original.generate_identity();

    // Persist the wrapped private half.
    let sealed = original.seal_identity_private().unwrap();
    let id_path = td.path().join(".omp/encrypted-identity");
    fs::create_dir_all(id_path.parent().unwrap()).unwrap();
    fs::write(&id_path, sealed).unwrap();
    drop(original);

    // Fresh session: unlock, load sealed identity, verify.
    let sealed = fs::read(&id_path).unwrap();
    let mut reloaded = TenantKeys::unlock(b"secret-pass", &tenant).unwrap();
    reloaded.unseal_and_attach_identity(&sealed).unwrap();
    assert_eq!(reloaded.identity().unwrap().pub_key, pub_orig);

    // Wrong passphrase in a new session fails.
    let mut wrong = TenantKeys::unlock(b"wrong-pass-", &tenant).unwrap();
    assert!(wrong.unseal_and_attach_identity(&sealed).is_err());
}

#[test]
fn gc_walker_handles_encrypted_commits_and_manifests() {
    let td = TempDir::new().unwrap();
    let _ = Repo::init(td.path()).unwrap();
    std::fs::write(
        td.path().join("omp.toml"),
        "[ingest]\ndefault_schema_policy = \"minimal\"\n\n[storage]\nchunk_size_bytes = 128\n",
    )
    .unwrap();
    let repo = Repo::open(td.path()).unwrap();
    let keys = make_keys(&TenantId::local());
    stage_starter(&repo);
    repo.commit_encrypted("init", Some(fixed_author()), &keys)
        .unwrap();

    let big: Vec<u8> = (0..500u32).map(|i| (i & 0xff) as u8).collect();
    repo.add_encrypted("data/big.bin", &big, None, Some("text"), &keys)
        .unwrap();
    let commit_hash = repo
        .commit_encrypted(
            "add",
            Some(AuthorOverride {
                timestamp: Some("2026-04-23T01:00:00Z".into()),
                ..fixed_author()
            }),
            &keys,
        )
        .unwrap();

    // Walk with keys so the walker can unseal the manifest envelope and
    // trace into the chunks object.
    let live = omp_core::gc::walk(repo.store(), &[commit_hash], Some(&keys), &[]).unwrap();
    assert!(live.commits.contains(&commit_hash));
    // chunks object should be live (500 bytes / 128 = 4 chunks).
    assert_eq!(
        live.chunks_objects.len(),
        1,
        "expected exactly one chunks object in the live set"
    );
    // chunk blobs plus starter-pack probe blobs.
    assert!(
        live.blobs.len() >= 4,
        "chunks should add ≥4 blobs to live set"
    );
}

#[test]
fn ls_still_works_on_encrypted_repo() {
    // The `ls` API reads trees. For encrypted repos it needs path_key —
    // the existing public `ls` doesn't thread keys, so we exercise the
    // lower-level `paths::get_at_with_key` directly.
    let td = TempDir::new().unwrap();
    let repo = Repo::init(td.path()).unwrap();
    let keys = make_keys(&TenantId::local());
    stage_starter(&repo);
    repo.commit_encrypted("init", Some(fixed_author()), &keys)
        .unwrap();

    let mut fields = Fields::new();
    fields.insert(
        "title".into(),
        omp_core::manifest::FieldValue::String("t".into()),
    );
    repo.add_encrypted("a.md", b"x", Some(fields), Some("text"), &keys)
        .unwrap();
    let commit_hash = repo
        .commit_encrypted(
            "add",
            Some(AuthorOverride {
                timestamp: Some("2026-04-23T01:00:00Z".into()),
                ..fixed_author()
            }),
            &keys,
        )
        .unwrap();

    let store = DiskStore::open(td.path()).unwrap();
    // Resolve tree hash from the commit headers.
    let (_ty, commit_body) = store.get(&commit_hash).unwrap().unwrap();
    let s = std::str::from_utf8(&commit_body).unwrap();
    let tree_line = s.lines().find(|l| l.starts_with("tree ")).unwrap();
    let tree_hash: omp_core::Hash = tree_line.strip_prefix("tree ").unwrap().parse().unwrap();

    // With the path_key, we can find the entry for "a.md".
    let found =
        omp_core::paths::get_at_with_key(repo.store(), "a.md", &tree_hash, Some(&keys.path_key))
            .unwrap();
    assert!(
        found.is_some(),
        "get_at_with_key should resolve the manifest"
    );

    // Without path_key, tree parsing would fail on the encryption marker.
    let err = omp_core::paths::get_at(repo.store(), "a.md", &tree_hash).unwrap_err();
    assert!(
        matches!(err, omp_core::OmpError::Unauthorized(_)),
        "plaintext parser must refuse encrypted trees: got {err:?}"
    );
}

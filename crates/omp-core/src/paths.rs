//! Path resolution through nested trees. See `03-hierarchical-trees.md`.
//!
//! All tree mutations (`put_at`, `delete_at`) return a new root-tree hash.
//! Callers chain these during staging until the final root enters a commit.

use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::object::ObjectType;
use crate::store::ObjectStore;
use crate::tree::{Entry, Mode, Tree};

/// Normalize a slash-separated path: trim leading/trailing `/`, reject empty
/// components, reject absolute references, and surface the list of components.
pub fn split(path: &str) -> Result<Vec<&str>> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<&str> = trimmed.split('/').collect();
    for p in &parts {
        if p.is_empty() || *p == "." || *p == ".." {
            return Err(OmpError::InvalidPath(format!("bad path: {path:?}")));
        }
    }
    Ok(parts)
}

fn load_tree_with_key(
    store: &dyn ObjectStore,
    h: &Hash,
    path_key: Option<&[u8; 32]>,
) -> Result<Tree> {
    let (ty, content) = store
        .get(h)?
        .ok_or_else(|| OmpError::NotFound(format!("tree object {}", h.hex())))?;
    if ty != "tree" {
        return Err(OmpError::Corrupt(format!(
            "expected tree, got {ty} at {}",
            h.hex()
        )));
    }
    Tree::parse_with_path_key(&content, path_key)
}

fn store_tree_with_key(
    store: &dyn ObjectStore,
    t: &Tree,
    path_key: Option<&[u8; 32]>,
) -> Result<Hash> {
    let bytes = t.serialize_with_path_key(path_key)?;
    store.put(ObjectType::Tree.as_str(), &bytes)
}

/// Walk `path` starting from `root` and return the target entry.
///
/// - Empty path returns `(Mode::Tree, root)`.
/// - A missing segment yields `Ok(None)`.
/// - Attempting to walk *through* a non-tree is `Ok(None)` (the caller can't
///   disambiguate "missing" vs "not a directory" here, matching Git's behavior).
pub fn get_at(store: &dyn ObjectStore, path: &str, root: &Hash) -> Result<Option<(Mode, Hash)>> {
    get_at_with_key(store, path, root, None)
}

pub fn get_at_with_key(
    store: &dyn ObjectStore,
    path: &str,
    root: &Hash,
    path_key: Option<&[u8; 32]>,
) -> Result<Option<(Mode, Hash)>> {
    let parts = split(path)?;
    if parts.is_empty() {
        return Ok(Some((Mode::Tree, *root)));
    }

    let mut current_tree_hash = *root;
    let mut current = load_tree_with_key(store, &current_tree_hash, path_key)?;

    for (i, part) in parts.iter().enumerate() {
        let entry = match current.get(part) {
            Some(e) => e.clone(),
            None => return Ok(None),
        };
        if i == parts.len() - 1 {
            return Ok(Some((entry.mode, entry.hash)));
        }
        if entry.mode != Mode::Tree {
            return Ok(None);
        }
        current_tree_hash = entry.hash;
        current = load_tree_with_key(store, &current_tree_hash, path_key)?;
    }
    unreachable!("loop always returns")
}

/// Insert or replace an entry at `path`, returning the new root-tree hash.
///
/// Creates intermediate trees on the way down; does not require existing
/// parents. If a non-tree entry already exists at an intermediate segment,
/// returns `OmpError::Conflict`.
pub fn put_at(
    store: &dyn ObjectStore,
    root: Option<&Hash>,
    path: &str,
    entry: Entry,
) -> Result<Hash> {
    put_at_with_key(store, root, path, entry, None)
}

/// Key-aware variant. When `path_key` is `Some`, every tree touched on
/// the way down is serialized with encrypted entry names (doc 13).
pub fn put_at_with_key(
    store: &dyn ObjectStore,
    root: Option<&Hash>,
    path: &str,
    entry: Entry,
    path_key: Option<&[u8; 32]>,
) -> Result<Hash> {
    let parts = split(path)?;
    if parts.is_empty() {
        return Err(OmpError::InvalidPath("cannot put at empty path".into()));
    }
    put_inner(store, root, &parts, entry, path_key)
}

fn put_inner(
    store: &dyn ObjectStore,
    current_hash: Option<&Hash>,
    parts: &[&str],
    entry: Entry,
    path_key: Option<&[u8; 32]>,
) -> Result<Hash> {
    let mut tree = match current_hash {
        Some(h) => load_tree_with_key(store, h, path_key)?,
        None => Tree::new(),
    };
    let head = parts[0];
    if parts.len() == 1 {
        tree.insert(head, entry)?;
        return store_tree_with_key(store, &tree, path_key);
    }
    let child_hash = match tree.get(head) {
        Some(e) if e.mode == Mode::Tree => Some(e.hash),
        Some(_) => {
            return Err(OmpError::Conflict(format!(
                "segment {head:?} is not a directory"
            )));
        }
        None => None,
    };
    let new_child = put_inner(store, child_hash.as_ref(), &parts[1..], entry, path_key)?;
    tree.insert(
        head,
        Entry {
            mode: Mode::Tree,
            hash: new_child,
        },
    )?;
    store_tree_with_key(store, &tree, path_key)
}

/// Remove the entry at `path`. Returns the new root-tree hash, or `Ok(None)`
/// if the tree collapses to empty. Empty intermediate directories are pruned.
pub fn delete_at(store: &dyn ObjectStore, root: &Hash, path: &str) -> Result<Option<Hash>> {
    delete_at_with_key(store, root, path, None)
}

pub fn delete_at_with_key(
    store: &dyn ObjectStore,
    root: &Hash,
    path: &str,
    path_key: Option<&[u8; 32]>,
) -> Result<Option<Hash>> {
    let parts = split(path)?;
    if parts.is_empty() {
        return Err(OmpError::InvalidPath("cannot delete root tree".into()));
    }
    delete_inner(store, root, &parts, path_key)
}

fn delete_inner(
    store: &dyn ObjectStore,
    current_hash: &Hash,
    parts: &[&str],
    path_key: Option<&[u8; 32]>,
) -> Result<Option<Hash>> {
    let mut tree = load_tree_with_key(store, current_hash, path_key)?;
    let head = parts[0];
    if parts.len() == 1 {
        tree.remove(head);
    } else {
        let Some(entry) = tree.get(head).cloned() else {
            return Ok(Some(*current_hash));
        };
        if entry.mode != Mode::Tree {
            return Ok(Some(*current_hash));
        }
        match delete_inner(store, &entry.hash, &parts[1..], path_key)? {
            Some(new_child) => {
                tree.insert(
                    head,
                    Entry {
                        mode: Mode::Tree,
                        hash: new_child,
                    },
                )?;
            }
            None => {
                tree.remove(head);
            }
        }
    }
    if tree.is_empty() {
        Ok(None)
    } else {
        Ok(Some(store_tree_with_key(store, &tree, path_key)?))
    }
}

/// Depth-first walk yielding every leaf entry (blob or manifest). Each yielded
/// item is `(path, mode, hash)` with `path` slash-separated. Ordering is
/// deterministic: lexicographic by entry name at every level.
pub fn walk(store: &dyn ObjectStore, root: &Hash) -> Result<Vec<(String, Mode, Hash)>> {
    walk_with_key(store, root, None)
}

pub fn walk_with_key(
    store: &dyn ObjectStore,
    root: &Hash,
    path_key: Option<&[u8; 32]>,
) -> Result<Vec<(String, Mode, Hash)>> {
    let mut out = Vec::new();
    walk_inner(store, root, String::new(), &mut out, path_key)?;
    Ok(out)
}

fn walk_inner(
    store: &dyn ObjectStore,
    tree_hash: &Hash,
    prefix: String,
    out: &mut Vec<(String, Mode, Hash)>,
    path_key: Option<&[u8; 32]>,
) -> Result<()> {
    let tree = load_tree_with_key(store, tree_hash, path_key)?;
    for (name, entry) in tree.entries() {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        match entry.mode {
            Mode::Tree => walk_inner(store, &entry.hash, path, out, path_key)?,
            _ => out.push((path, entry.mode, entry.hash)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::disk::DiskStore;
    use tempfile::TempDir;

    fn make_store() -> (TempDir, DiskStore) {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        (td, s)
    }

    fn blob(store: &dyn ObjectStore, data: &[u8]) -> Hash {
        store.put("blob", data).unwrap()
    }

    #[test]
    fn put_then_get_single_level() {
        let (_td, s) = make_store();
        let b = blob(&s, b"hello");
        let root = put_at(
            &s,
            None,
            "README.md",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap();
        let got = get_at(&s, "README.md", &root).unwrap().unwrap();
        assert_eq!(got, (Mode::Manifest, b));
    }

    #[test]
    fn put_then_get_nested() {
        let (_td, s) = make_store();
        let b = blob(&s, b"hi");
        let root = put_at(
            &s,
            None,
            "a/b/c.md",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap();
        let got = get_at(&s, "a/b/c.md", &root).unwrap().unwrap();
        assert_eq!(got.1, b);
        // Intermediate segments are trees.
        let intermediate = get_at(&s, "a/b", &root).unwrap().unwrap();
        assert_eq!(intermediate.0, Mode::Tree);
    }

    #[test]
    fn missing_segment_is_none() {
        let (_td, s) = make_store();
        let b = blob(&s, b"hi");
        let root = put_at(
            &s,
            None,
            "a/b/c.md",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap();
        assert!(get_at(&s, "a/z", &root).unwrap().is_none());
        assert!(get_at(&s, "nonexistent", &root).unwrap().is_none());
    }

    #[test]
    fn empty_path_returns_root() {
        let (_td, s) = make_store();
        let b = blob(&s, b"hi");
        let root = put_at(
            &s,
            None,
            "a",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap();
        assert_eq!(get_at(&s, "", &root).unwrap(), Some((Mode::Tree, root)));
    }

    #[test]
    fn delete_prunes_empty_parents() {
        let (_td, s) = make_store();
        let b = blob(&s, b"hi");
        let root = put_at(
            &s,
            None,
            "a/b/c.md",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap();
        let after = delete_at(&s, &root, "a/b/c.md").unwrap();
        assert!(after.is_none(), "tree should collapse to empty");
    }

    #[test]
    fn delete_preserves_siblings() {
        let (_td, s) = make_store();
        let b1 = blob(&s, b"1");
        let b2 = blob(&s, b"2");
        let r = put_at(
            &s,
            None,
            "a/b/c.md",
            Entry {
                mode: Mode::Manifest,
                hash: b1,
            },
        )
        .unwrap();
        let r = put_at(
            &s,
            Some(&r),
            "a/b/d.md",
            Entry {
                mode: Mode::Manifest,
                hash: b2,
            },
        )
        .unwrap();
        let r = delete_at(&s, &r, "a/b/c.md").unwrap().unwrap();
        assert!(get_at(&s, "a/b/c.md", &r).unwrap().is_none());
        assert!(get_at(&s, "a/b/d.md", &r).unwrap().is_some());
    }

    #[test]
    fn rename_preserves_subtree_hash() {
        // Put a/x and a/y, then move the subtree from "a" to "b". The subtree
        // hash should be preserved because only the parent changes.
        let (_td, s) = make_store();
        let b1 = blob(&s, b"1");
        let b2 = blob(&s, b"2");
        let r = put_at(
            &s,
            None,
            "a/x",
            Entry {
                mode: Mode::Manifest,
                hash: b1,
            },
        )
        .unwrap();
        let r = put_at(
            &s,
            Some(&r),
            "a/y",
            Entry {
                mode: Mode::Manifest,
                hash: b2,
            },
        )
        .unwrap();
        let (mode, subtree_hash) = get_at(&s, "a", &r).unwrap().unwrap();
        assert_eq!(mode, Mode::Tree);
        let r = put_at(
            &s,
            Some(&r),
            "b",
            Entry {
                mode: Mode::Tree,
                hash: subtree_hash,
            },
        )
        .unwrap();
        let r = delete_at(&s, &r, "a").unwrap().unwrap();
        let (_, new_subtree_hash) = get_at(&s, "b", &r).unwrap().unwrap();
        assert_eq!(subtree_hash, new_subtree_hash);
    }

    #[test]
    fn walk_visits_all_leaves_in_order() {
        let (_td, s) = make_store();
        let b = blob(&s, b"x");
        let r = put_at(
            &s,
            None,
            "z/b",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap();
        let r = put_at(
            &s,
            Some(&r),
            "z/a",
            Entry {
                mode: Mode::Blob,
                hash: b,
            },
        )
        .unwrap();
        let r = put_at(
            &s,
            Some(&r),
            "a",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap();
        let all = walk(&s, &r).unwrap();
        let paths: Vec<&str> = all.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["a", "z/a", "z/b"]);
    }

    #[test]
    fn rejects_bad_paths() {
        let (_td, s) = make_store();
        let b = blob(&s, b"x");
        let err = put_at(
            &s,
            None,
            "a/../b",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap_err();
        assert!(matches!(err, OmpError::InvalidPath(_)));
    }

    #[test]
    fn put_through_non_tree_is_conflict() {
        let (_td, s) = make_store();
        let b = blob(&s, b"x");
        let r = put_at(
            &s,
            None,
            "a",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap();
        let err = put_at(
            &s,
            Some(&r),
            "a/b",
            Entry {
                mode: Mode::Manifest,
                hash: b,
            },
        )
        .unwrap_err();
        assert!(matches!(err, OmpError::Conflict(_)));
    }

    #[test]
    fn overwrite_replaces_entry() {
        let (_td, s) = make_store();
        let b1 = blob(&s, b"1");
        let b2 = blob(&s, b"2");
        let r = put_at(
            &s,
            None,
            "x",
            Entry {
                mode: Mode::Manifest,
                hash: b1,
            },
        )
        .unwrap();
        let r = put_at(
            &s,
            Some(&r),
            "x",
            Entry {
                mode: Mode::Manifest,
                hash: b2,
            },
        )
        .unwrap();
        assert_eq!(get_at(&s, "x", &r).unwrap().unwrap(), (Mode::Manifest, b2));
    }
}

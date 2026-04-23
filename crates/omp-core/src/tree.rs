//! Plain-text tree objects.
//!
//! One line per entry: `<mode> <hash>\t<name>\n`. Entries sorted lexically by
//! `<name>`. Modes: `blob`, `manifest`, `tree`. See `02-object-model.md`.
//!
//! When a tenant is in encrypted mode, each entry's `<name>` is AEAD-sealed
//! under the tenant's `path_key` (doc 13 §What is encrypted). The tree body
//! then begins with the marker line `!encrypted-path v1\n` and each entry
//! is `<mode> <hash>\t<hex_sealed_name>\n`. Mode + hash stay plaintext so
//! the server can walk the reference graph for GC.

use std::collections::BTreeMap;

use crate::error::{OmpError, Result};
use crate::hash::Hash;

/// First line of an encrypted-name tree. Callers decide whether a tree is
/// encrypted by sniffing this prefix; `parse` dispatches on it.
const ENCRYPTED_TREE_MARKER: &str = "!encrypted-path v1";
/// AEAD associated-data label binding a sealed name to this usage.
const TREE_NAME_AAD: &[u8] = b"omp-tree-name";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    Blob,
    Manifest,
    Tree,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Blob => "blob",
            Mode::Manifest => "manifest",
            Mode::Tree => "tree",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "blob" => Ok(Mode::Blob),
            "manifest" => Ok(Mode::Manifest),
            "tree" => Ok(Mode::Tree),
            other => Err(OmpError::Corrupt(format!("unknown tree mode: {other}"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub mode: Mode,
    pub hash: Hash,
}

/// An in-memory tree object: an ordered map of name -> entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    entries: BTreeMap<String, Entry>,
}

impl Tree {
    pub fn new() -> Self {
        Tree {
            entries: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.get(name)
    }

    pub fn insert(&mut self, name: impl Into<String>, entry: Entry) -> Result<()> {
        let name = name.into();
        validate_name(&name)?;
        self.entries.insert(name, entry);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Option<Entry> {
        self.entries.remove(name)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Serialize to the canonical on-disk form (plaintext names).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, entry) in &self.entries {
            out.extend_from_slice(entry.mode.as_str().as_bytes());
            out.push(b' ');
            out.extend_from_slice(entry.hash.hex().as_bytes());
            out.push(b'\t');
            out.extend_from_slice(name.as_bytes());
            out.push(b'\n');
        }
        out
    }

    /// Serialize with optional name encryption. `None` produces the v1
    /// plaintext format; `Some(path_key)` produces the encrypted-name form
    /// defined in doc 13 §What is encrypted.
    ///
    /// The per-name nonce is deterministic (HKDF(path_key, b"tree-name" ||
    /// name_bytes)) so re-serializing the same in-memory Tree yields
    /// byte-identical output — required because the framed-object hash is
    /// over these bytes.
    pub fn serialize_with_path_key(&self, path_key: Option<&[u8; 32]>) -> Result<Vec<u8>> {
        let Some(key) = path_key else {
            return Ok(self.serialize());
        };
        use crate::share::hex_encode;

        let mut out = Vec::new();
        out.extend_from_slice(ENCRYPTED_TREE_MARKER.as_bytes());
        out.push(b'\n');
        for (name, entry) in &self.entries {
            let nonce = crate::keys::derive_nonce(key, b"tree-name", name.as_bytes())?;
            let sealed = omp_crypto::aead::seal(
                key,
                &nonce,
                TREE_NAME_AAD,
                name.as_bytes(),
            )
            .map_err(|e| OmpError::internal(format!("seal tree name: {e}")))?;
            out.extend_from_slice(entry.mode.as_str().as_bytes());
            out.push(b' ');
            out.extend_from_slice(entry.hash.hex().as_bytes());
            out.push(b'\t');
            out.extend_from_slice(hex_encode(&sealed).as_bytes());
            out.push(b'\n');
        }
        Ok(out)
    }

    /// Parse a tree body. Auto-detects the encrypted-name marker; when
    /// present, `path_key` is required to unseal.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_with_path_key(bytes, None)
    }

    pub fn parse_with_path_key(bytes: &[u8], path_key: Option<&[u8; 32]>) -> Result<Self> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::Corrupt("tree is not UTF-8".into()))?;
        let mut lines = s.lines();
        // Peek for the encrypted-tree marker.
        let (encrypted, body_start_line) = if let Some(first) = s.lines().next() {
            if first == ENCRYPTED_TREE_MARKER {
                // Consume the marker and start after it.
                lines.next();
                (true, 2usize)
            } else {
                (false, 1)
            }
        } else {
            (false, 1)
        };
        if encrypted && path_key.is_none() {
            return Err(OmpError::Unauthorized(
                "tree has encrypted names but no path_key supplied".into(),
            ));
        }

        let mut out = Tree::new();
        for (offset, line) in lines.enumerate() {
            let line_no = body_start_line + offset;
            if line.is_empty() {
                continue;
            }
            let (meta, name_field) = line.split_once('\t').ok_or_else(|| {
                OmpError::Corrupt(format!("tree line {line_no}: missing tab"))
            })?;
            let (mode_s, hash_s) = meta.split_once(' ').ok_or_else(|| {
                OmpError::Corrupt(format!("tree line {line_no}: missing space"))
            })?;
            let mode = Mode::parse(mode_s)?;
            let hash: Hash = hash_s.parse().map_err(|e| {
                OmpError::Corrupt(format!("tree line {line_no}: bad hash: {e}"))
            })?;
            let name = if encrypted {
                use crate::share::hex_decode;
                let sealed = hex_decode(name_field)?;
                let key = path_key.unwrap();
                let opened = omp_crypto::aead::open(key, TREE_NAME_AAD, &sealed).map_err(
                    |_| OmpError::Unauthorized(format!(
                        "tree line {line_no}: sealed name did not open (wrong path_key or tampered)"
                    )),
                )?;
                String::from_utf8(opened).map_err(|_| {
                    OmpError::Corrupt(format!(
                        "tree line {line_no}: decrypted name is not UTF-8"
                    ))
                })?
            } else {
                name_field.to_string()
            };
            validate_name(&name)?;
            out.entries.insert(name, Entry { mode, hash });
        }
        Ok(out)
    }
}


fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(OmpError::InvalidPath("empty tree entry name".into()));
    }
    if name.contains('/') || name.contains('\0') || name == "." || name == ".." {
        return Err(OmpError::InvalidPath(format!(
            "invalid tree entry name: {name:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::object::{hash_of, ObjectType};

    fn h(s: &str) -> Hash {
        Hash::of(s.as_bytes())
    }

    #[test]
    fn roundtrip_empty() {
        let t = Tree::new();
        let bytes = t.serialize();
        assert_eq!(bytes.len(), 0);
        let parsed = Tree::parse(&bytes).unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn roundtrip_mixed_modes() {
        let mut t = Tree::new();
        t.insert(
            "omp.toml",
            Entry {
                mode: Mode::Blob,
                hash: h("omp.toml"),
            },
        )
        .unwrap();
        t.insert(
            "earnings-q3.pdf",
            Entry {
                mode: Mode::Manifest,
                hash: h("q3"),
            },
        )
        .unwrap();
        t.insert(
            "schemas",
            Entry {
                mode: Mode::Tree,
                hash: h("schemas"),
            },
        )
        .unwrap();
        let bytes = t.serialize();
        let parsed = Tree::parse(&bytes).unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn entries_are_sorted_lexicographically() {
        let mut t = Tree::new();
        t.insert("z", Entry { mode: Mode::Blob, hash: h("z") }).unwrap();
        t.insert("a", Entry { mode: Mode::Blob, hash: h("a") }).unwrap();
        t.insert("m", Entry { mode: Mode::Blob, hash: h("m") }).unwrap();
        let bytes = t.serialize();
        let s = std::str::from_utf8(&bytes).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines[0].ends_with("\ta"));
        assert!(lines[1].ends_with("\tm"));
        assert!(lines[2].ends_with("\tz"));
    }

    #[test]
    fn rejects_slash_in_name() {
        let mut t = Tree::new();
        let err = t
            .insert(
                "a/b",
                Entry {
                    mode: Mode::Blob,
                    hash: h("x"),
                },
            )
            .unwrap_err();
        assert!(matches!(err, OmpError::InvalidPath(_)));
    }

    #[test]
    fn rejects_dot_entries() {
        let mut t = Tree::new();
        assert!(t
            .insert(".", Entry { mode: Mode::Blob, hash: h("a") })
            .is_err());
        assert!(t
            .insert("..", Entry { mode: Mode::Blob, hash: h("a") })
            .is_err());
    }

    #[test]
    fn encrypted_tree_hides_names() {
        let mut t = Tree::new();
        t.insert(
            "secrets.pdf",
            Entry {
                mode: Mode::Manifest,
                hash: h("m"),
            },
        )
        .unwrap();
        t.insert(
            "schemas",
            Entry {
                mode: Mode::Tree,
                hash: h("s"),
            },
        )
        .unwrap();
        let key = [0x11u8; 32];
        let bytes = t.serialize_with_path_key(Some(&key)).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with(ENCRYPTED_TREE_MARKER));
        // The plaintext name should not appear anywhere in the body.
        assert!(!s.contains("secrets.pdf"));
        assert!(!s.contains("schemas"));
        // Roundtrip recovers the plaintext names.
        let back = Tree::parse_with_path_key(&bytes, Some(&key)).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn encrypted_tree_is_byte_stable() {
        // Same in-memory Tree must serialize to identical bytes on every
        // call so the framed-object hash is stable.
        let mut t = Tree::new();
        for name in ["a", "b", "ccc"] {
            t.insert(
                name,
                Entry {
                    mode: Mode::Blob,
                    hash: h(name),
                },
            )
            .unwrap();
        }
        let key = [0x77u8; 32];
        let s1 = t.serialize_with_path_key(Some(&key)).unwrap();
        let s2 = t.serialize_with_path_key(Some(&key)).unwrap();
        assert_eq!(s1, s2, "encrypted tree serialization must be deterministic");
    }

    #[test]
    fn encrypted_tree_rejects_missing_key_on_parse() {
        let mut t = Tree::new();
        t.insert(
            "x",
            Entry {
                mode: Mode::Blob,
                hash: h("x"),
            },
        )
        .unwrap();
        let key = [0x01u8; 32];
        let bytes = t.serialize_with_path_key(Some(&key)).unwrap();
        let err = Tree::parse(&bytes).unwrap_err();
        assert!(matches!(err, OmpError::Unauthorized(_)));
    }

    #[test]
    fn encrypted_tree_rejects_wrong_key() {
        let mut t = Tree::new();
        t.insert(
            "x",
            Entry {
                mode: Mode::Blob,
                hash: h("x"),
            },
        )
        .unwrap();
        let bytes = t.serialize_with_path_key(Some(&[0x01u8; 32])).unwrap();
        let err = Tree::parse_with_path_key(&bytes, Some(&[0xffu8; 32])).unwrap_err();
        assert!(matches!(err, OmpError::Unauthorized(_)));
    }

    #[test]
    fn plaintext_tree_parses_without_key() {
        let mut t = Tree::new();
        t.insert(
            "a.md",
            Entry {
                mode: Mode::Manifest,
                hash: h("a"),
            },
        )
        .unwrap();
        let bytes = t.serialize_with_path_key(None).unwrap();
        // Same as the v1 serialize().
        assert_eq!(bytes, t.serialize());
        let back = Tree::parse_with_path_key(&bytes, None).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn hash_stability() {
        // Freeze the tree wire format against a hand-computed hash.
        let mut t = Tree::new();
        t.insert(
            "a",
            Entry {
                mode: Mode::Blob,
                hash: Hash::of(b"a"),
            },
        )
        .unwrap();
        let bytes = t.serialize();
        // Expected: "blob <hex>\ta\n" exactly.
        let expected_line = format!("blob {}\ta\n", Hash::of(b"a").hex());
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), expected_line);
        // The framed-object hash equals sha256("tree <size>\0<bytes>").
        let framed_hash = hash_of(ObjectType::Tree, &bytes);
        let manual = Hash::of(
            &[
                format!("tree {}\0", bytes.len()).as_bytes(),
                &bytes,
            ]
            .concat(),
        );
        assert_eq!(framed_hash, manual);
    }
}

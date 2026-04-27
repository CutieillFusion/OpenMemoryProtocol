//! `chunks` object body — an ordered list of `(chunk_hash, length)` pairs
//! pointing at the blobs that hold a large file's content.
//!
//! Wire format (plain text, UTF-8, LF-terminated, one entry per line):
//!
//! ```text
//! <chunk_hash_hex> <length_bytes>
//! ```
//!
//! Order is file order (not sorted). Framing + compression are identical to
//! every other object type — see `docs/design/12-large-files.md §The chunks
//! object type`.

use std::fmt::Write as _;

use crate::error::{OmpError, Result};
use crate::hash::Hash;

/// Parsed body of a `chunks` object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunksBody {
    pub entries: Vec<ChunkEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkEntry {
    pub hash: Hash,
    pub length: u64,
}

impl ChunksBody {
    pub fn new(entries: Vec<ChunkEntry>) -> Self {
        ChunksBody { entries }
    }

    /// Serialize to the canonical plain-text wire format.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = String::with_capacity(self.entries.len() * 80);
        for e in &self.entries {
            let _ = writeln!(out, "{} {}", e.hash.hex(), e.length);
        }
        out.into_bytes()
    }

    /// Parse the wire format. Tolerates a trailing LF but no other variation.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::Corrupt("chunks body is not UTF-8".into()))?;
        let mut entries = Vec::new();
        for (i, line) in s.split('\n').enumerate() {
            if line.is_empty() {
                // Final LF produces a trailing empty element; tolerate only there.
                continue;
            }
            let (hash_part, len_part) = line.split_once(' ').ok_or_else(|| {
                OmpError::Corrupt(format!(
                    "chunks body line {} missing space separator",
                    i + 1
                ))
            })?;
            let hash: Hash = hash_part
                .parse()
                .map_err(|e| OmpError::Corrupt(format!("chunks body line {}: {e}", i + 1)))?;
            let length: u64 = len_part.parse().map_err(|_| {
                OmpError::Corrupt(format!(
                    "chunks body line {}: bad length {len_part:?}",
                    i + 1
                ))
            })?;
            entries.push(ChunkEntry { hash, length });
        }
        Ok(ChunksBody { entries })
    }

    /// Total plaintext (or ciphertext, under encryption) length across every
    /// chunk. Used by readers to size buffers and by GC.
    pub fn total_length(&self) -> u64 {
        self.entries.iter().map(|e| e.length).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_roundtrip() {
        let body = ChunksBody::new(vec![]);
        let bytes = body.serialize();
        assert_eq!(bytes, b"");
        assert_eq!(ChunksBody::parse(&bytes).unwrap(), body);
    }

    #[test]
    fn three_entries_roundtrip() {
        let body = ChunksBody::new(vec![
            ChunkEntry {
                hash: Hash::of(b"a"),
                length: 16_777_216,
            },
            ChunkEntry {
                hash: Hash::of(b"b"),
                length: 16_777_216,
            },
            ChunkEntry {
                hash: Hash::of(b"c"),
                length: 4_215_603,
            },
        ]);
        let bytes = body.serialize();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Three LF-terminated lines.
        assert_eq!(s.matches('\n').count(), 3);
        assert_eq!(ChunksBody::parse(&bytes).unwrap(), body);
    }

    #[test]
    fn order_is_preserved_not_sorted() {
        let h1 = Hash::of(b"z");
        let h2 = Hash::of(b"a");
        let body = ChunksBody::new(vec![
            ChunkEntry {
                hash: h1,
                length: 1,
            },
            ChunkEntry {
                hash: h2,
                length: 2,
            },
        ]);
        let bytes = body.serialize();
        let s = std::str::from_utf8(&bytes).unwrap();
        // First line must still reference h1 — order is the content.
        assert!(s.starts_with(&h1.hex()));
    }

    #[test]
    fn corrupt_line_without_space_rejected() {
        assert!(ChunksBody::parse(b"abc\n").is_err());
    }

    #[test]
    fn corrupt_length_rejected() {
        let mut line = Hash::of(b"x").hex();
        line.push_str(" not-a-number\n");
        assert!(ChunksBody::parse(line.as_bytes()).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_entries_roundtrip(pairs in proptest::collection::vec(
            (any::<[u8; 8]>(), any::<u64>()),
            0..32usize,
        )) {
            let entries: Vec<ChunkEntry> = pairs.into_iter().map(|(seed, length)| ChunkEntry {
                hash: Hash::of(&seed),
                length,
            }).collect();
            let body = ChunksBody::new(entries);
            let bytes = body.serialize();
            let back = ChunksBody::parse(&bytes).unwrap();
            prop_assert_eq!(back, body);
        }
    }
}

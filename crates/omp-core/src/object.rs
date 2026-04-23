use std::io::{Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::error::{OmpError, Result};
use crate::hash::Hash;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectType {
    Blob,
    Tree,
    Manifest,
    Commit,
    /// Chunked-Merkle object: an ordered list of `<chunk_hash> <length>` lines
    /// pointing at the blobs that hold a large file's content. See
    /// `docs/design/12-large-files.md`.
    Chunks,
    /// Cryptographic grant wrapping a content key to one or more recipient
    /// X25519 public keys. Plaintext canonical TOML envelope; the wrapped
    /// keys are themselves ciphertext. See
    /// `docs/design/13-end-to-end-encryption.md §Sharing`.
    Share,
}

impl ObjectType {
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectType::Blob => "blob",
            ObjectType::Tree => "tree",
            ObjectType::Manifest => "manifest",
            ObjectType::Commit => "commit",
            ObjectType::Chunks => "chunks",
            ObjectType::Share => "share",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "blob" => Ok(ObjectType::Blob),
            "tree" => Ok(ObjectType::Tree),
            "manifest" => Ok(ObjectType::Manifest),
            "commit" => Ok(ObjectType::Commit),
            "chunks" => Ok(ObjectType::Chunks),
            "share" => Ok(ObjectType::Share),
            other => Err(OmpError::Corrupt(format!("unknown object type: {other}"))),
        }
    }
}

/// Build the canonical framed bytes: `"<type> <size>\0<content>"`.
pub fn frame(type_: ObjectType, content: &[u8]) -> Vec<u8> {
    let header = format!("{} {}\0", type_.as_str(), content.len());
    let mut out = Vec::with_capacity(header.len() + content.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(content);
    out
}

/// Hash over the framed bytes, pre-compression.
pub fn hash_of(type_: ObjectType, content: &[u8]) -> Hash {
    Hash::of(&frame(type_, content))
}

/// Zlib-compress the framed bytes for on-disk storage.
pub fn compress_framed(framed: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(framed)
        .map_err(|e| OmpError::internal(format!("zlib encode: {e}")))?;
    encoder
        .finish()
        .map_err(|e| OmpError::internal(format!("zlib finish: {e}")))
}

/// Decompress on-disk bytes back to the framed form.
pub fn decompress(raw: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(raw);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| OmpError::Corrupt(format!("zlib decode: {e}")))?;
    Ok(out)
}

/// Split a framed object into `(type, content)`.
pub fn parse_framed(framed: &[u8]) -> Result<(ObjectType, &[u8])> {
    let nul = framed
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| OmpError::Corrupt("framed object missing NUL".into()))?;
    let header = std::str::from_utf8(&framed[..nul])
        .map_err(|_| OmpError::Corrupt("framed object header is not UTF-8".into()))?;
    let (type_str, size_str) = header
        .split_once(' ')
        .ok_or_else(|| OmpError::Corrupt("framed object header lacks space".into()))?;
    let declared: usize = size_str
        .parse()
        .map_err(|_| OmpError::Corrupt(format!("framed object bad size: {size_str:?}")))?;
    let type_ = ObjectType::parse(type_str)?;
    let content = &framed[nul + 1..];
    if content.len() != declared {
        return Err(OmpError::Corrupt(format!(
            "framed object size mismatch: declared {declared}, got {}",
            content.len()
        )));
    }
    Ok((type_, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_frame() {
        let framed = frame(ObjectType::Blob, b"hello");
        let (t, content) = parse_framed(&framed).unwrap();
        assert_eq!(t, ObjectType::Blob);
        assert_eq!(content, b"hello");
    }

    #[test]
    fn blob_hash_is_over_framed_bytes() {
        let h = hash_of(ObjectType::Blob, b"hello");
        let direct = Hash::of(b"blob 5\0hello");
        assert_eq!(h, direct);
    }

    #[test]
    fn compression_roundtrip() {
        let framed = frame(ObjectType::Commit, b"tree abcd\nfoo");
        let compressed = compress_framed(&framed).unwrap();
        let back = decompress(&compressed).unwrap();
        assert_eq!(back, framed);
    }

    #[test]
    fn empty_blob_hash_matches_literal_frame() {
        // Pin the framing by comparing against sha256 over the exact literal bytes.
        // If this fires, the wire format silently changed.
        let h = hash_of(ObjectType::Blob, b"");
        assert_eq!(h, Hash::of(b"blob 0\0"));
    }

    #[test]
    fn empty_chunks_hash_matches_literal_frame() {
        // Same pinning guard for the chunks wire format from
        // `docs/design/12-large-files.md`. Framing is identical to every
        // other object type.
        let h = hash_of(ObjectType::Chunks, b"");
        assert_eq!(h, Hash::of(b"chunks 0\0"));
    }

    #[test]
    fn chunks_type_roundtrips_parse() {
        assert_eq!(ObjectType::parse("chunks").unwrap(), ObjectType::Chunks);
        assert_eq!(ObjectType::Chunks.as_str(), "chunks");
    }

    #[test]
    fn empty_share_hash_matches_literal_frame() {
        // Pinning guard for the share wire format from
        // `docs/design/13-end-to-end-encryption.md §Sharing`.
        let h = hash_of(ObjectType::Share, b"");
        assert_eq!(h, Hash::of(b"share 0\0"));
    }

    #[test]
    fn share_type_roundtrips_parse() {
        assert_eq!(ObjectType::parse("share").unwrap(), ObjectType::Share);
        assert_eq!(ObjectType::Share.as_str(), "share");
    }
}

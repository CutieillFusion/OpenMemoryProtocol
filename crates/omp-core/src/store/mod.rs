//! The single storage contract. Everything OMP does reduces to these nine
//! core methods plus one additive streaming variant.
//! See `docs/design/08-deployability.md` and `docs/design/12-large-files.md`.

pub mod disk;

use std::io::Read;

use crate::error::{OmpError, Result};
use crate::hash::Hash;

/// The nine-method object-store trait plus one additive streaming writer.
///
/// Implementations **must** be `Send + Sync` so the trait object can be shared
/// across tokio tasks without per-request locking (see `08-deployability.md`).
pub trait ObjectStore: Send + Sync {
    /// Store an object under the framed `<type> <size>\0<content>` encoding.
    /// Returns the SHA-256 of the framed bytes (pre-compression).
    fn put(&self, type_: &str, content: &[u8]) -> Result<Hash>;

    /// Stream-in an object from a reader. Computes the framed hash while
    /// writing compressed bytes; avoids loading the full object into RAM.
    /// `known_size` is required because the framing header `<type> <size>\0`
    /// must be hashed before content — callers without a known size should
    /// buffer to a temp file and `fstat` it first.
    ///
    /// Default impl reads the full stream into memory and delegates to `put`.
    /// Backends that can truly stream (disk, S3) override this. See
    /// `docs/design/12-large-files.md §Streaming ingest`.
    fn put_stream(&self, type_: &str, reader: &mut dyn Read, known_size: u64) -> Result<Hash> {
        let cap = usize::try_from(known_size).map_err(|_| {
            OmpError::internal(format!("put_stream: known_size {known_size} exceeds usize"))
        })?;
        let mut buf = Vec::with_capacity(cap);
        reader
            .read_to_end(&mut buf)
            .map_err(|e| OmpError::internal(format!("put_stream read: {e}")))?;
        if buf.len() as u64 != known_size {
            return Err(OmpError::internal(format!(
                "put_stream: declared size {known_size} != actual {}",
                buf.len()
            )));
        }
        self.put(type_, &buf)
    }

    /// Look up an object by hash. Returns `(type, content)` with content
    /// already stripped of the framing header.
    fn get(&self, hash: &Hash) -> Result<Option<(String, Vec<u8>)>>;

    /// Does this hash exist in the store?
    fn has(&self, hash: &Hash) -> Result<bool>;

    /// Iterate every `(ref_name, commit_hash)` pair. Ref names are
    /// slash-separated (e.g., `refs/heads/main`).
    fn iter_refs(&self) -> Result<Box<dyn Iterator<Item = (String, Hash)> + '_>>;

    fn read_ref(&self, name: &str) -> Result<Option<Hash>>;
    fn write_ref(&self, name: &str, commit: &Hash) -> Result<()>;
    fn delete_ref(&self, name: &str) -> Result<()>;

    /// Read `HEAD`. Returns `"ref: refs/..."` for attached HEAD, or a raw hash
    /// string for detached HEAD.
    fn read_head(&self) -> Result<String>;
    fn write_head(&self, value: &str) -> Result<()>;
}

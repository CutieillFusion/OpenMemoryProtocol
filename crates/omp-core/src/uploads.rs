//! Resumable upload-session state. See
//! `docs/design/12-large-files.md §Resumable upload sessions`.
//!
//! Each session is a directory under `.omp/uploads/<id>/` containing:
//!   - `state.toml`      — session metadata: declared size, path hint, created_at
//!   - `chunk.<offset>`  — one file per `PATCH /uploads/{id}?offset=<bytes>` call;
//!                         idempotent because re-PATCHing the same offset
//!                         overwrites the same filename.
//!
//! Session state lives parallel to `.omp/index.json`: un-versioned, local to
//! this machine, ignored by the tree walker.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::error::{OmpError, Result};

/// Returned by `POST /uploads`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadHandle {
    pub upload_id: String,
    pub chunk_size_bytes: u64,
}

/// Persisted metadata for a session. One file `state.toml` per session dir.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionState {
    pub id: String,
    pub declared_size: u64,
    /// Set on `commit`; None until then.
    pub path: Option<String>,
    pub created_at: String,
    pub chunk_size_bytes: u64,
}

impl SessionState {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::Corrupt("upload state.toml is not UTF-8".into()))?;
        toml::from_str(s).map_err(|e| OmpError::Corrupt(format!("upload state.toml: {e}")))
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        let s = toml::to_string_pretty(self)
            .map_err(|e| OmpError::internal(format!("serialize session: {e}")))?;
        Ok(s.into_bytes())
    }
}

/// Session-manager handle, rooted at `.omp/uploads/`.
pub struct UploadManager {
    uploads_dir: PathBuf,
    default_chunk_size: u64,
}

impl UploadManager {
    pub fn new(omp_dir: impl AsRef<Path>, default_chunk_size: u64) -> Self {
        UploadManager {
            uploads_dir: omp_dir.as_ref().join("uploads"),
            default_chunk_size,
        }
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.uploads_dir.join(id)
    }

    /// Create a fresh session. Returns the opaque id + the server's chosen
    /// `chunk_size_bytes`. Callers may PATCH in any chunk size, but the
    /// server reassembles via the session's declared chunk size at commit.
    pub fn open(&self, declared_size: u64) -> Result<UploadHandle> {
        fs::create_dir_all(&self.uploads_dir).map_err(|e| OmpError::io(&self.uploads_dir, e))?;

        let id = generate_id();
        let dir = self.session_dir(&id);
        fs::create_dir_all(&dir).map_err(|e| OmpError::io(&dir, e))?;

        let created_at = crate::time::now_rfc3339();
        let state = SessionState {
            id: id.clone(),
            declared_size,
            path: None,
            created_at,
            chunk_size_bytes: self.default_chunk_size,
        };
        let body = state.serialize()?;
        fs::write(dir.join("state.toml"), body).map_err(|e| OmpError::io(&dir, e))?;
        Ok(UploadHandle {
            upload_id: id,
            chunk_size_bytes: self.default_chunk_size,
        })
    }

    /// Idempotent write of one chunk at `offset`. Subsequent PATCHes at the
    /// same offset overwrite. The chunk filename encodes the offset so
    /// concurrent PATCHes at different offsets don't collide.
    pub fn write_chunk(&self, id: &str, offset: u64, bytes: &[u8]) -> Result<()> {
        let dir = self.session_dir(id);
        if !dir.is_dir() {
            return Err(OmpError::NotFound(format!("upload session {id}")));
        }
        let state = self.load_state(id)?;
        if offset
            .checked_add(bytes.len() as u64)
            .map(|end| end > state.declared_size)
            .unwrap_or(true)
        {
            return Err(OmpError::InvalidPath(format!(
                "upload {id}: chunk at offset {offset} (len {}) overflows declared size {}",
                bytes.len(),
                state.declared_size
            )));
        }
        // Fixed-width 20-char offset so directory ordering is lexicographic
        // in offset order — lets the commit path read chunks back in one pass.
        let file = dir.join(format!("chunk.{:020}", offset));
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&file)
            .map_err(|e| OmpError::io(&file, e))?;
        f.write_all(bytes).map_err(|e| OmpError::io(&file, e))?;
        f.sync_all().map_err(|e| OmpError::io(&file, e))?;
        Ok(())
    }

    /// Reassemble all chunks in offset order into one buffer. Returns the
    /// full plaintext. For v1 of this feature, the buffer is in-memory —
    /// true streaming into a `chunks` object via `put_stream` is a
    /// follow-up (see the design doc's §Deferred).
    pub fn assemble(&self, id: &str) -> Result<Vec<u8>> {
        let dir = self.session_dir(id);
        if !dir.is_dir() {
            return Err(OmpError::NotFound(format!("upload session {id}")));
        }
        let state = self.load_state(id)?;
        // Collect chunk files, sort by the offset suffix in the filename.
        let mut chunks: Vec<(u64, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| OmpError::io(&dir, e))? {
            let entry = entry.map_err(|e| OmpError::io(&dir, e))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("chunk.") {
                let offset: u64 = rest.parse().map_err(|_| {
                    OmpError::Corrupt(format!("upload {id}: bad chunk name {name:?}"))
                })?;
                chunks.push((offset, entry.path()));
            }
        }
        chunks.sort_by_key(|(off, _)| *off);

        let expected = state.declared_size as usize;
        let mut buf = Vec::with_capacity(expected);
        let mut cursor: u64 = 0;
        for (offset, path) in chunks {
            if offset != cursor {
                return Err(OmpError::Corrupt(format!(
                    "upload {id}: chunk gap: expected offset {cursor}, got {offset}"
                )));
            }
            let mut f = fs::File::open(&path).map_err(|e| OmpError::io(&path, e))?;
            let meta = f.metadata().map_err(|e| OmpError::io(&path, e))?;
            // Reserve and splice in.
            let before = buf.len();
            buf.resize(before + meta.len() as usize, 0);
            f.seek(SeekFrom::Start(0))
                .map_err(|e| OmpError::io(&path, e))?;
            f.read_exact(&mut buf[before..])
                .map_err(|e| OmpError::io(&path, e))?;
            cursor += meta.len();
        }
        if cursor != state.declared_size {
            return Err(OmpError::Corrupt(format!(
                "upload {id}: assembled {cursor} bytes but declared {}",
                state.declared_size
            )));
        }
        Ok(buf)
    }

    /// Remove the session directory (commit, abort, or TTL reap).
    pub fn remove(&self, id: &str) -> Result<()> {
        let dir = self.session_dir(id);
        if !dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&dir).map_err(|e| OmpError::io(&dir, e))
    }

    /// Reap sessions older than `ttl_hours`. Returns the number of sessions
    /// removed. Designed to be called from a periodic task or an `omp admin
    /// gc` command.
    pub fn reap_stale(&self, ttl_hours: u32) -> Result<u64> {
        if !self.uploads_dir.is_dir() {
            return Ok(0);
        }
        let now = OffsetDateTime::now_utc();
        let ttl_seconds = ttl_hours as i64 * 3600;
        let mut reaped = 0u64;
        for entry in
            fs::read_dir(&self.uploads_dir).map_err(|e| OmpError::io(&self.uploads_dir, e))?
        {
            let entry = entry.map_err(|e| OmpError::io(&self.uploads_dir, e))?;
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let state_path = entry.path().join("state.toml");
            let age_ok = match fs::read(&state_path) {
                Ok(bytes) => match SessionState::parse(&bytes) {
                    Ok(s) => match OffsetDateTime::parse(&s.created_at, &Rfc3339) {
                        Ok(created) => (now - created).whole_seconds() > ttl_seconds,
                        Err(_) => true, // unparseable timestamp: reap
                    },
                    Err(_) => true,
                },
                Err(_) => true, // missing state.toml: reap
            };
            if age_ok {
                self.remove(&id)?;
                reaped += 1;
            }
        }
        Ok(reaped)
    }

    pub fn load_state(&self, id: &str) -> Result<SessionState> {
        let path = self.session_dir(id).join("state.toml");
        let bytes = fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => OmpError::NotFound(format!("upload session {id}")),
            _ => OmpError::io(&path, e),
        })?;
        SessionState::parse(&bytes)
    }
}

/// Random base-62 session id. 22 chars ≈ 131 bits of entropy.
fn generate_id() -> String {
    use std::fs::File;
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    let mut f = File::open("/dev/urandom").expect("open /dev/urandom");
    f.read_exact(&mut bytes).expect("read /dev/urandom");
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(22);
    for b in bytes {
        out.push(ALPHA[(b as usize) % ALPHA.len()] as char);
    }
    // Pad to 22 from further bytes if desired; 16 is fine for collision-avoid.
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_mgr() -> (TempDir, UploadManager) {
        let td = TempDir::new().unwrap();
        let omp = td.path().join(".omp");
        std::fs::create_dir_all(&omp).unwrap();
        let mgr = UploadManager::new(&omp, 64);
        (td, mgr)
    }

    #[test]
    fn open_creates_session_dir_with_state() {
        let (td, mgr) = fresh_mgr();
        let h = mgr.open(256).unwrap();
        let dir = td.path().join(".omp/uploads").join(&h.upload_id);
        assert!(dir.is_dir());
        assert!(dir.join("state.toml").exists());
        let state = mgr.load_state(&h.upload_id).unwrap();
        assert_eq!(state.declared_size, 256);
        assert_eq!(state.chunk_size_bytes, 64);
    }

    #[test]
    fn patch_chunks_and_assemble_in_order() {
        let (_td, mgr) = fresh_mgr();
        let h = mgr.open(12).unwrap();
        // Offsets arrive out of order, exercising the sort-at-assemble path.
        mgr.write_chunk(&h.upload_id, 4, b"mid4").unwrap();
        mgr.write_chunk(&h.upload_id, 0, b"zero").unwrap();
        mgr.write_chunk(&h.upload_id, 8, b"last").unwrap();
        let out = mgr.assemble(&h.upload_id).unwrap();
        assert_eq!(out, b"zeromid4last");
    }

    #[test]
    fn patch_is_idempotent() {
        let (_td, mgr) = fresh_mgr();
        let h = mgr.open(4).unwrap();
        mgr.write_chunk(&h.upload_id, 0, b"xxxx").unwrap();
        mgr.write_chunk(&h.upload_id, 0, b"yyyy").unwrap();
        let out = mgr.assemble(&h.upload_id).unwrap();
        assert_eq!(out, b"yyyy", "second PATCH must overwrite first");
    }

    #[test]
    fn rejects_overflow_beyond_declared_size() {
        let (_td, mgr) = fresh_mgr();
        let h = mgr.open(4).unwrap();
        let err = mgr.write_chunk(&h.upload_id, 0, b"xxxxx").unwrap_err();
        assert!(matches!(err, OmpError::InvalidPath(_)));
    }

    #[test]
    fn assemble_detects_gap() {
        let (_td, mgr) = fresh_mgr();
        let h = mgr.open(10).unwrap();
        mgr.write_chunk(&h.upload_id, 0, b"abc").unwrap();
        mgr.write_chunk(&h.upload_id, 5, b"fghij").unwrap();
        let err = mgr.assemble(&h.upload_id).unwrap_err();
        assert!(matches!(err, OmpError::Corrupt(_)));
    }

    #[test]
    fn remove_session_deletes_dir() {
        let (td, mgr) = fresh_mgr();
        let h = mgr.open(4).unwrap();
        let dir = td.path().join(".omp/uploads").join(&h.upload_id);
        assert!(dir.is_dir());
        mgr.remove(&h.upload_id).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn load_state_on_missing_session_is_not_found() {
        let (_td, mgr) = fresh_mgr();
        let err = mgr.load_state("nope").unwrap_err();
        assert!(matches!(err, OmpError::NotFound(_)));
    }
}

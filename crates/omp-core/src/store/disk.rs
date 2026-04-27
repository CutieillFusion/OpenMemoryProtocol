//! Disk-backed `ObjectStore`.
//!
//! Layout (mirrors Git's `.git/`, per `01-on-disk-layout.md`):
//! ```text
//! <repo>/.omp/
//!   HEAD
//!   refs/heads/<name>
//!   objects/<first-2-hex>/<remaining-62-hex>
//!   refs.lock        (held exclusively during commit)
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::write::ZlibEncoder;
use flate2::Compression;
use fs2::FileExt;
use sha2::{Digest, Sha256};

use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::object::{self, ObjectType};
use crate::store::ObjectStore;

pub struct DiskStore {
    root: PathBuf,
}

impl DiskStore {
    /// Open an existing `.omp/` directory. Returns an error if not present.
    pub fn open(repo: impl AsRef<Path>) -> Result<Self> {
        let root = repo.as_ref().join(".omp");
        if !root.is_dir() {
            return Err(OmpError::NotFound(format!(
                ".omp/ not found under {}",
                repo.as_ref().display()
            )));
        }
        Ok(DiskStore { root })
    }

    /// Create a fresh `.omp/` directory skeleton: `objects/`, `refs/heads/`,
    /// and `HEAD` pointing at `refs/heads/main`. Idempotent.
    pub fn init(repo: impl AsRef<Path>) -> Result<Self> {
        let root = repo.as_ref().join(".omp");
        create_dir(&root)?;
        create_dir(&root.join("objects"))?;
        create_dir(&root.join("refs"))?;
        create_dir(&root.join("refs/heads"))?;

        let head_path = root.join("HEAD");
        if !head_path.exists() {
            write_atomic(&head_path, b"ref: refs/heads/main\n")?;
        }
        Ok(DiskStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, h: &Hash) -> PathBuf {
        let hex = h.hex();
        self.root.join("objects").join(&hex[..2]).join(&hex[2..])
    }

    fn ref_path(&self, name: &str) -> Result<PathBuf> {
        if name.contains("..") || name.starts_with('/') || name.is_empty() {
            return Err(OmpError::InvalidPath(format!("ref name: {name:?}")));
        }
        Ok(self.root.join(name))
    }

    /// Acquire an exclusive cross-process lock on `.omp/refs.lock`. The
    /// returned guard releases on drop.
    pub fn lock_refs(&self) -> Result<RefsLock> {
        let path = self.root.join("refs.lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| OmpError::io(&path, e))?;
        file.lock_exclusive().map_err(|e| OmpError::io(&path, e))?;
        Ok(RefsLock { file: Some(file) })
    }
}

pub struct RefsLock {
    file: Option<File>,
}

impl Drop for RefsLock {
    fn drop(&mut self) {
        if let Some(f) = self.file.take() {
            let _ = f.unlock();
        }
    }
}

impl ObjectStore for DiskStore {
    fn put(&self, type_: &str, content: &[u8]) -> Result<Hash> {
        let t = ObjectType::parse(type_)?;
        let framed = object::frame(t, content);
        let hash = Hash::of(&framed);
        let path = self.object_path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        let parent = path
            .parent()
            .ok_or_else(|| OmpError::internal("object path has no parent"))?;
        create_dir(parent)?;
        let compressed = object::compress_framed(&framed)?;
        write_atomic(&path, &compressed)?;
        Ok(hash)
    }

    fn put_stream(&self, type_: &str, reader: &mut dyn Read, known_size: u64) -> Result<Hash> {
        // See docs/design/12-large-files.md §Disk backend implementation sketch.
        //
        // 1. Hash-and-zlib-compress `"<type> <size>\0"` + streamed content
        //    into a temp file under `.omp/objects/tmp/`.
        // 2. On EOF, finalize hash → rename the temp file into the final
        //    object path. If the target already exists (dedup), drop the tmp.
        // Peak memory: 64 KiB read buffer + ~256 KiB zlib state.
        let t = ObjectType::parse(type_)?;
        let header = format!("{} {}\0", t.as_str(), known_size);

        let tmp_dir = self.root.join("objects").join("tmp");
        create_dir(&tmp_dir)?;
        let tmp_path = tmp_dir.join(format!("stream.{}.{}", std::process::id(), tmp_counter()));

        let mut hasher = Sha256::new();
        hasher.update(header.as_bytes());

        // Compression target: the temp file. We stream compressed bytes in.
        let compressed_out = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| OmpError::io(&tmp_path, e))?;
        let mut encoder = ZlibEncoder::new(compressed_out, Compression::default());
        encoder
            .write_all(header.as_bytes())
            .map_err(|e| OmpError::io(&tmp_path, e))?;

        let mut buf = [0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = reader.read(&mut buf).map_err(|e| {
                // Best-effort cleanup on read error.
                let _ = fs::remove_file(&tmp_path);
                OmpError::internal(format!("put_stream read: {e}"))
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| OmpError::io(&tmp_path, e))?;
            total += n as u64;
        }
        if total != known_size {
            let _ = fs::remove_file(&tmp_path);
            return Err(OmpError::internal(format!(
                "put_stream: declared size {known_size} != actual {total}"
            )));
        }

        let final_out = encoder.finish().map_err(|e| OmpError::io(&tmp_path, e))?;
        final_out
            .sync_all()
            .map_err(|e| OmpError::io(&tmp_path, e))?;
        drop(final_out);

        let digest = hasher.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&digest);
        let hash = Hash(arr);

        let final_path = self.object_path(&hash);
        if final_path.exists() {
            // Dedup: drop the temp; an equivalent object is already on disk.
            let _ = fs::remove_file(&tmp_path);
            return Ok(hash);
        }
        if let Some(parent) = final_path.parent() {
            create_dir(parent)?;
        }
        if let Err(first) = fs::rename(&tmp_path, &final_path) {
            // Mirror `write_atomic`'s fallback for filesystems that don't
            // support rename-over-existing.
            if fs::remove_file(&final_path).is_ok() && fs::rename(&tmp_path, &final_path).is_ok() {
                return Ok(hash);
            }
            let _ = fs::remove_file(&tmp_path);
            return Err(OmpError::io(&final_path, first));
        }
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Option<(String, Vec<u8>)>> {
        let path = self.object_path(hash);
        if !path.exists() {
            return Ok(None);
        }
        let mut raw = Vec::new();
        File::open(&path)
            .and_then(|mut f| f.read_to_end(&mut raw).map(|_| ()))
            .map_err(|e| OmpError::io(&path, e))?;
        let framed = object::decompress(&raw)?;
        let (t, content) = object::parse_framed(&framed)?;
        Ok(Some((t.as_str().to_string(), content.to_vec())))
    }

    fn has(&self, hash: &Hash) -> Result<bool> {
        Ok(self.object_path(hash).exists())
    }

    fn iter_refs(&self) -> Result<Box<dyn Iterator<Item = (String, Hash)> + '_>> {
        let refs_dir = self.root.join("refs");
        let mut out = Vec::new();
        collect_refs(&refs_dir, &refs_dir, &mut out)?;
        Ok(Box::new(out.into_iter()))
    }

    fn read_ref(&self, name: &str) -> Result<Option<Hash>> {
        let path = self.ref_path(name)?;
        match fs::read_to_string(&path) {
            Ok(s) => {
                let trimmed = s.trim();
                trimmed
                    .parse()
                    .map(Some)
                    .map_err(|e| OmpError::Corrupt(format!("ref {name}: {e}")))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(OmpError::io(&path, e)),
        }
    }

    fn write_ref(&self, name: &str, commit: &Hash) -> Result<()> {
        let path = self.ref_path(name)?;
        if let Some(parent) = path.parent() {
            create_dir(parent)?;
        }
        let mut bytes = commit.hex();
        bytes.push('\n');
        write_atomic(&path, bytes.as_bytes())
    }

    fn delete_ref(&self, name: &str) -> Result<()> {
        let path = self.ref_path(name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(OmpError::io(&path, e)),
        }
    }

    fn read_head(&self) -> Result<String> {
        let path = self.root.join("HEAD");
        let s = fs::read_to_string(&path).map_err(|e| OmpError::io(&path, e))?;
        Ok(s.trim().to_string())
    }

    fn write_head(&self, value: &str) -> Result<()> {
        let path = self.root.join("HEAD");
        let mut s = value.to_string();
        if !s.ends_with('\n') {
            s.push('\n');
        }
        write_atomic(&path, s.as_bytes())
    }
}

fn collect_refs(base: &Path, current: &Path, out: &mut Vec<(String, Hash)>) -> Result<()> {
    let entries = match fs::read_dir(current) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(OmpError::io(current, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| OmpError::io(current, e))?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|e| OmpError::io(&path, e))?;
        if metadata.is_dir() {
            collect_refs(base, &path, out)?;
        } else if metadata.is_file() {
            let name = path
                .strip_prefix(base.parent().unwrap_or(base))
                .ok()
                .and_then(|p| p.to_str())
                .map(|s| s.replace('\\', "/"))
                .ok_or_else(|| OmpError::internal("ref path not utf-8"))?;
            // `name` is `refs/heads/...` — keep it as-is.
            let raw = fs::read_to_string(&path).map_err(|e| OmpError::io(&path, e))?;
            let hash = raw
                .trim()
                .parse()
                .map_err(|e| OmpError::Corrupt(format!("ref {name}: {e}")))?;
            out.push((name, hash));
        }
    }
    Ok(())
}

fn create_dir(p: &Path) -> Result<()> {
    fs::create_dir_all(p).map_err(|e| OmpError::io(p, e))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| OmpError::internal("atomic write: no parent"))?;
    let tmp = parent.join(format!(".tmp.{}.{}", std::process::id(), tmp_counter()));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| OmpError::io(&tmp, e))?;
        f.write_all(bytes).map_err(|e| OmpError::io(&tmp, e))?;
        f.sync_all().map_err(|e| OmpError::io(&tmp, e))?;
    }
    if let Err(first) = fs::rename(&tmp, path) {
        // Some filesystems (notably older Windows FSes) refuse rename-over-existing.
        // Fall back to remove-then-rename; if the fallback also fails, surface the
        // original error rather than the secondary one.
        if fs::remove_file(path).is_ok() && fs::rename(&tmp, path).is_ok() {
            return Ok(());
        }
        let _ = fs::remove_file(&tmp);
        return Err(OmpError::io(path, first));
    }
    Ok(())
}

fn tmp_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    CTR.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_creates_skeleton() {
        let td = TempDir::new().unwrap();
        let _ = DiskStore::init(td.path()).unwrap();
        assert!(td.path().join(".omp/HEAD").exists());
        assert!(td.path().join(".omp/objects").is_dir());
        assert!(td.path().join(".omp/refs/heads").is_dir());
    }

    #[test]
    fn object_roundtrip() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        let h = s.put("blob", b"hello").unwrap();
        assert!(s.has(&h).unwrap());
        let (t, content) = s.get(&h).unwrap().unwrap();
        assert_eq!(t, "blob");
        assert_eq!(content, b"hello");
    }

    #[test]
    fn ref_roundtrip() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        let h = s.put("commit", b"tree abcd\n\nmsg").unwrap();
        s.write_ref("refs/heads/main", &h).unwrap();
        assert_eq!(s.read_ref("refs/heads/main").unwrap(), Some(h));
        let all: Vec<_> = s.iter_refs().unwrap().collect();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "refs/heads/main");
    }

    #[test]
    fn missing_object_is_none() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        let h = Hash::of(b"not there");
        assert!(!s.has(&h).unwrap());
        assert!(s.get(&h).unwrap().is_none());
    }

    #[test]
    fn head_default_is_main() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        assert_eq!(s.read_head().unwrap(), "ref: refs/heads/main");
    }

    #[test]
    fn put_is_idempotent() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        let h1 = s.put("blob", b"x").unwrap();
        let h2 = s.put("blob", b"x").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn put_stream_matches_put_hash() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        let content = vec![42u8; 1024 * 1024]; // 1 MiB
        let h_put = s.put("blob", &content).unwrap();
        // Fresh store so the second write is not deduped trivially.
        let td2 = TempDir::new().unwrap();
        let s2 = DiskStore::init(td2.path()).unwrap();
        let mut cur = std::io::Cursor::new(content.clone());
        let h_stream = s2
            .put_stream("blob", &mut cur, content.len() as u64)
            .unwrap();
        assert_eq!(h_put, h_stream);
        let (t, back) = s2.get(&h_stream).unwrap().unwrap();
        assert_eq!(t, "blob");
        assert_eq!(back, content);
    }

    #[test]
    fn put_stream_dedups() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        let content = b"streamed content".to_vec();
        let mut c1 = std::io::Cursor::new(content.clone());
        let h1 = s.put_stream("blob", &mut c1, content.len() as u64).unwrap();
        let mut c2 = std::io::Cursor::new(content.clone());
        let h2 = s.put_stream("blob", &mut c2, content.len() as u64).unwrap();
        assert_eq!(h1, h2);
        // Only one object file on disk (plus maybe tmp from the dedup path
        // that should have been cleaned up).
        let prefix = td.path().join(".omp/objects").join(&h1.hex()[..2]);
        let count = std::fs::read_dir(&prefix).unwrap().count();
        assert_eq!(count, 1, "expected 1 object file after dedup");
    }

    #[test]
    fn put_stream_rejects_size_mismatch() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        let mut cur = std::io::Cursor::new(b"abc".to_vec());
        let err = s.put_stream("blob", &mut cur, 99).unwrap_err();
        assert!(matches!(err, OmpError::Internal(_)));
    }

    #[test]
    fn put_stream_chunks_type() {
        let td = TempDir::new().unwrap();
        let s = DiskStore::init(td.path()).unwrap();
        let body = b"abc\n".to_vec();
        let mut cur = std::io::Cursor::new(body.clone());
        let h = s.put_stream("chunks", &mut cur, body.len() as u64).unwrap();
        let (t, back) = s.get(&h).unwrap().unwrap();
        assert_eq!(t, "chunks");
        assert_eq!(back, body);
    }
}

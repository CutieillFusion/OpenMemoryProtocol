//! Filesystem-backed blob store. Sha256-content-addressed, sharded by the
//! first two hex chars to avoid one giant directory. The doc names the
//! shared `omp-store` gRPC service for production; for the course-scope
//! demo a local filesystem is enough and keeps the deploy story trivial.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("creating blob root {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn path_for(&self, hash_hex: &str) -> PathBuf {
        let (prefix, rest) = if hash_hex.len() >= 2 {
            (&hash_hex[..2], &hash_hex[2..])
        } else {
            ("00", hash_hex)
        };
        self.root.join(prefix).join(rest)
    }

    pub fn put(&self, hash_hex: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(hash_hex);
        if path.exists() {
            return Ok(()); // content-addressed; idempotent
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path_for(hash_hex);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(bytes))
    }
}

//! JSON-on-disk catalog. The schema mirrors `docs/design/23-probe-marketplace.md`'s
//! SQL table; persisting as JSON is a deliberate course-scope simplification
//! (the doc names Postgres for production). Atomic writes via tmp+rename.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub publisher_sub: String,
    pub namespace: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub wasm_hash: String,
    pub manifest_hash: String,
    #[serde(default)]
    pub readme_hash: Option<String>,
    /// Optional source bundle (raw `lib.rs` or a tarball depending on
    /// publisher). Surfaced on the detail page so a consumer can read the
    /// probe's source before installing.
    #[serde(default)]
    pub source_hash: Option<String>,
    pub published_at: u64,
    #[serde(default)]
    pub yanked_at: Option<u64>,
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    entries: HashMap<String, CatalogEntry>,
}

#[derive(Debug)]
pub struct Catalog {
    path: PathBuf,
    state: OnDisk,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        let state = if path.exists() {
            let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            if bytes.is_empty() {
                OnDisk::default()
            } else {
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?
            }
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            OnDisk::default()
        };
        Ok(Self {
            path: path.to_path_buf(),
            state,
        })
    }

    pub fn all(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.state.entries.values()
    }

    pub fn get(&self, id: &str) -> Option<&CatalogEntry> {
        self.state.entries.get(id)
    }

    pub fn upsert(&mut self, entry: CatalogEntry) -> Result<()> {
        self.state.entries.insert(entry.id.clone(), entry);
        self.flush()
    }

    fn flush(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "renaming {} -> {}",
                tmp.display(),
                self.path.display()
            )
        })?;
        Ok(())
    }
}

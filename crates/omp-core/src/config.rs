//! `omp.toml` (versioned) and `.omp/local.toml` (machine-local) loaders.
//! See `07-config.md`.

use std::path::Path;

use serde::Deserialize;

use crate::error::{OmpError, Result};

/// Versioned repo config parsed from `omp.toml` in the tree root.
#[derive(Clone, Debug)]
pub struct RepoConfig {
    pub ingest: IngestConfig,
    pub workdir: WorkdirConfig,
    pub probes: ProbeLimits,
    pub storage: StorageConfig,
}

#[derive(Clone, Debug)]
pub struct IngestConfig {
    pub default_schema_policy: DefaultSchemaPolicy,
    pub allow_blob_fallback: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultSchemaPolicy {
    Reject,
    Minimal,
}

#[derive(Clone, Debug)]
pub struct WorkdirConfig {
    pub ignore: Vec<String>,
    pub follow_symlinks: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ProbeLimits {
    pub memory_mb: u32,
    pub fuel: u64,
    pub wall_clock_s: u32,
}

/// Storage-layer knobs from `[storage]` in `omp.toml`. See
/// `docs/design/12-large-files.md`.
#[derive(Clone, Copy, Debug)]
pub struct StorageConfig {
    /// Threshold below which ingest uses the v1 single-blob path. At or above
    /// this size, the ingest pipeline splits the file into `chunks` of exactly
    /// this size (last chunk may be shorter).
    pub chunk_size_bytes: u64,
    /// Upload sessions older than this TTL are reaped by `omp admin gc`.
    pub upload_session_ttl_hours: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            chunk_size_bytes: 16 * 1024 * 1024,
            upload_session_ttl_hours: 24,
        }
    }
}

impl Default for RepoConfig {
    fn default() -> Self {
        RepoConfig {
            ingest: IngestConfig {
                default_schema_policy: DefaultSchemaPolicy::Reject,
                allow_blob_fallback: false,
            },
            workdir: WorkdirConfig {
                ignore: vec![
                    ".git/".into(),
                    "node_modules/".into(),
                    "*.log".into(),
                    "*.tmp".into(),
                    "__pycache__/".into(),
                ],
                follow_symlinks: false,
            },
            probes: ProbeLimits {
                memory_mb: 64,
                fuel: 1_000_000_000,
                wall_clock_s: 10,
            },
            storage: StorageConfig::default(),
        }
    }
}

impl RepoConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default)]
            ingest: Option<IngestRaw>,
            #[serde(default)]
            workdir: Option<WorkdirRaw>,
            #[serde(default)]
            probes: Option<ProbesRaw>,
            #[serde(default)]
            storage: Option<StorageRaw>,
        }
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct IngestRaw {
            #[serde(default)]
            default_schema_policy: Option<String>,
            #[serde(default)]
            allow_blob_fallback: Option<bool>,
        }
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct WorkdirRaw {
            #[serde(default)]
            ignore: Option<Vec<String>>,
            #[serde(default)]
            follow_symlinks: Option<bool>,
        }
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct ProbesRaw {
            #[serde(default)]
            memory_mb: Option<u32>,
            #[serde(default)]
            fuel: Option<u64>,
            #[serde(default)]
            wall_clock_s: Option<u32>,
        }
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct StorageRaw {
            #[serde(default)]
            chunk_size_bytes: Option<u64>,
            #[serde(default)]
            upload_session_ttl_hours: Option<u32>,
        }

        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::SchemaValidation("omp.toml not UTF-8".into()))?;
        let raw: Raw = toml::from_str(s)
            .map_err(|e| OmpError::SchemaValidation(format!("omp.toml: {e}")))?;

        let mut cfg = Self::default();
        if let Some(i) = raw.ingest {
            if let Some(p) = i.default_schema_policy {
                cfg.ingest.default_schema_policy = match p.as_str() {
                    "reject" => DefaultSchemaPolicy::Reject,
                    "minimal" => DefaultSchemaPolicy::Minimal,
                    other => {
                        return Err(OmpError::SchemaValidation(format!(
                            "omp.toml: default_schema_policy must be 'reject' or 'minimal', got {other:?}"
                        )));
                    }
                };
            }
            if let Some(b) = i.allow_blob_fallback {
                cfg.ingest.allow_blob_fallback = b;
            }
        }
        if let Some(w) = raw.workdir {
            if let Some(ig) = w.ignore {
                cfg.workdir.ignore = ig;
            }
            if let Some(f) = w.follow_symlinks {
                cfg.workdir.follow_symlinks = f;
            }
        }
        if let Some(p) = raw.probes {
            if let Some(m) = p.memory_mb {
                cfg.probes.memory_mb = m;
            }
            if let Some(f) = p.fuel {
                cfg.probes.fuel = f;
            }
            if let Some(w) = p.wall_clock_s {
                cfg.probes.wall_clock_s = w;
            }
        }
        if let Some(s) = raw.storage {
            if let Some(c) = s.chunk_size_bytes {
                if c == 0 {
                    return Err(OmpError::SchemaValidation(
                        "omp.toml: storage.chunk_size_bytes must be > 0".into(),
                    ));
                }
                cfg.storage.chunk_size_bytes = c;
            }
            if let Some(t) = s.upload_session_ttl_hours {
                cfg.storage.upload_session_ttl_hours = t;
            }
        }
        Ok(cfg)
    }
}

/// Machine-local config from `.omp/local.toml`, overlaid with env overrides.
#[derive(Clone, Debug)]
pub struct LocalConfig {
    pub server_bind: String,
    pub author_name: String,
    pub author_email: String,
    pub cache_dir: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        LocalConfig {
            server_bind: "127.0.0.1:8000".into(),
            author_name: "omp".into(),
            author_email: "omp@local".into(),
            cache_dir: ".omp/cache/".into(),
        }
    }
}

impl LocalConfig {
    pub fn load(omp_dir: &Path) -> Result<Self> {
        let path = omp_dir.join("local.toml");
        let mut cfg = Self::default();
        if path.exists() {
            let s = std::fs::read_to_string(&path).map_err(|e| OmpError::io(&path, e))?;
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct Raw {
                #[serde(default)]
                server: Option<ServerRaw>,
                #[serde(default)]
                author: Option<AuthorRaw>,
                #[serde(default)]
                cache: Option<CacheRaw>,
            }
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct ServerRaw {
                #[serde(default)]
                bind: Option<String>,
            }
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct AuthorRaw {
                #[serde(default)]
                name: Option<String>,
                #[serde(default)]
                email: Option<String>,
            }
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct CacheRaw {
                #[serde(default)]
                dir: Option<String>,
            }
            let raw: Raw = toml::from_str(&s)
                .map_err(|e| OmpError::SchemaValidation(format!("local.toml: {e}")))?;
            if let Some(srv) = raw.server {
                if let Some(b) = srv.bind {
                    cfg.server_bind = b;
                }
            }
            if let Some(a) = raw.author {
                if let Some(n) = a.name {
                    cfg.author_name = n;
                }
                if let Some(e) = a.email {
                    cfg.author_email = e;
                }
            }
            if let Some(c) = raw.cache {
                if let Some(d) = c.dir {
                    cfg.cache_dir = d;
                }
            }
        }

        // Environment overrides take final precedence.
        if let Ok(v) = std::env::var("OMP_SERVER_BIND") {
            cfg.server_bind = v;
        }
        if let Ok(v) = std::env::var("OMP_AUTHOR_NAME") {
            cfg.author_name = v;
        }
        if let Ok(v) = std::env::var("OMP_AUTHOR_EMAIL") {
            cfg.author_email = v;
        }
        Ok(cfg)
    }

    /// Write a fresh local.toml skeleton on `omp init`.
    pub fn write_skeleton(omp_dir: &Path) -> Result<()> {
        let path = omp_dir.join("local.toml");
        if path.exists() {
            return Ok(());
        }
        let body = r#"[server]
bind = "127.0.0.1:8000"

[author]
name = "omp"
email = "omp@local"
"#;
        std::fs::write(&path, body).map_err(|e| OmpError::io(&path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let c = RepoConfig::parse(b"").unwrap();
        assert_eq!(c.ingest.default_schema_policy, DefaultSchemaPolicy::Reject);
        assert!(!c.ingest.allow_blob_fallback);
        assert_eq!(c.probes.memory_mb, 64);
        assert_eq!(c.storage.chunk_size_bytes, 16 * 1024 * 1024);
        assert_eq!(c.storage.upload_session_ttl_hours, 24);
    }

    #[test]
    fn storage_section_overrides_defaults() {
        let input = r#"
[storage]
chunk_size_bytes = 4194304
upload_session_ttl_hours = 6
"#;
        let c = RepoConfig::parse(input.as_bytes()).unwrap();
        assert_eq!(c.storage.chunk_size_bytes, 4 * 1024 * 1024);
        assert_eq!(c.storage.upload_session_ttl_hours, 6);
    }

    #[test]
    fn storage_rejects_zero_chunk_size() {
        let input = r#"[storage]
chunk_size_bytes = 0
"#;
        assert!(RepoConfig::parse(input.as_bytes()).is_err());
    }

    #[test]
    fn parses_full_config() {
        let input = r#"
[ingest]
default_schema_policy = "minimal"
allow_blob_fallback = true

[workdir]
ignore = ["*.bak"]
follow_symlinks = true

[probes]
memory_mb = 128
fuel = 500000000
wall_clock_s = 5
"#;
        let c = RepoConfig::parse(input.as_bytes()).unwrap();
        assert_eq!(
            c.ingest.default_schema_policy,
            DefaultSchemaPolicy::Minimal
        );
        assert!(c.ingest.allow_blob_fallback);
        assert_eq!(c.workdir.ignore, vec!["*.bak"]);
        assert!(c.workdir.follow_symlinks);
        assert_eq!(c.probes.memory_mb, 128);
    }

    #[test]
    fn rejects_unknown_policy() {
        let input = r#"[ingest]
default_schema_policy = "strict"
"#;
        assert!(RepoConfig::parse(input.as_bytes()).is_err());
    }
}

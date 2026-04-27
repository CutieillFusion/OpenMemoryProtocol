//! `omp-builder` — server-side WASM probe compilation service.
//!
//! Per [`docs/design/20-server-side-probes.md`](../../docs/design/20-server-side-probes.md):
//! tenants POST Rust source through the gateway, this service compiles it in
//! a sandboxed cargo subprocess, and returns the resulting `.wasm` plus the
//! source bytes for the tenant to stage and commit into their tree.
//!
//! The interface is deliberately narrow:
//!
//! - `POST /probes/build`         → 202 with a `job_id`.
//! - `GET  /probes/build/{id}`    → state + artifacts (when `state == "ok"`).
//! - `GET  /probes/build/{id}/log` → SSE stream of cargo stdout+stderr.
//! - `DELETE /probes/build/{id}`  → cancel + cleanup.
//! - `GET  /healthz`               → cheap liveness probe.
//!
//! All builder state is in-memory; a pod restart drops in-flight jobs.
//! Persistent queueing is deferred (see the design doc §What's deferred).

pub mod builder;
pub mod jobs;
pub mod router;

use std::path::PathBuf;
use std::sync::Arc;

pub use jobs::{Job, JobId, JobState, JobsTable};

/// Runtime configuration for the builder service. Values default to
/// sensible-for-the-demo numbers; production deployments override via CLI
/// flags or config files.
#[derive(Clone, Debug)]
pub struct BuilderConfig {
    /// Filesystem root for per-job scratch directories. One subdirectory
    /// per `JobId`; cleaned up on terminal state + TTL.
    pub scratch_root: PathBuf,
    /// Absolute path to the `probe-common` crate the skeleton uses as a
    /// path dependency. Production deployments install it at
    /// `/usr/local/share/omp/probe-common/`; dev mode finds it at
    /// `<workspace_root>/probes-src/probe-common/`.
    pub probe_common_path: PathBuf,
    /// Wall-clock cap on the cargo subprocess. Builds that exceed this are
    /// SIGKILL'd and transition to `failed` with a `timeout` reason.
    pub wall_clock_secs: u64,
    /// Pod-wide concurrent build limit.
    pub max_concurrent_builds: usize,
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            scratch_root: std::env::temp_dir().join("omp-builder"),
            probe_common_path: PathBuf::from("../../probes-src/probe-common"),
            wall_clock_secs: 60,
            max_concurrent_builds: 4,
        }
    }
}

/// Shared service state. Cloned into each request handler.
#[derive(Clone)]
pub struct BuilderState {
    pub config: Arc<BuilderConfig>,
    pub jobs: Arc<JobsTable>,
    pub build_semaphore: Arc<tokio::sync::Semaphore>,
}

impl BuilderState {
    pub fn new(config: BuilderConfig) -> Self {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_builds));
        Self {
            config: Arc::new(config),
            jobs: Arc::new(JobsTable::new()),
            build_semaphore: semaphore,
        }
    }
}

pub use router::router;

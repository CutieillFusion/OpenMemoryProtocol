// `axum::Response` is the natural Err for HTTP handler helpers; boxing
// it would require unwrapping at every call site for no real benefit.
#![allow(clippy::result_large_err)]

//! Library surface of the OMP HTTP server.
//!
//! `AppState` holds the tenant registry, the tenants-base directory, and a
//! per-tenant `Repo` cache. The auth middleware is in `auth.rs`; routes in
//! `routes.rs` pull the already-resolved `Repo` out of a request extension.

pub mod auth;
pub mod health;
pub mod metrics;
pub mod routes;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use omp_core::api::Repo;
use omp_core::registry::TenantRegistry;
use omp_core::tenant::TenantId;
use omp_events::{EventBus, InMemoryBus};

/// Runtime mode for the server.
pub enum Mode {
    /// Single-tenant (`--no-auth`). One repo at `tenants_base`, always used
    /// as tenant `_local` with `Quotas::unlimited()`. No token required.
    NoAuth { repo: Arc<Repo> },
    /// Multi-tenant. Middleware resolves a `TenantId` from the
    /// `Authorization: Bearer` header via the registry, then looks up a
    /// per-tenant repo rooted at `<tenants_base>/<tenant_id>/`.
    MultiTenant {
        tenants_base: PathBuf,
        registry: Arc<Mutex<TenantRegistry>>,
        registry_path: PathBuf,
        /// Lazy cache: once a tenant's repo is opened/initialized, reuse it.
        repos: Mutex<HashMap<TenantId, Arc<Repo>>>,
    },
}

pub struct AppState {
    pub mode: Mode,
    /// Event bus for `commit.created`, `manifest.staged`, etc. See
    /// `docs/design/16-event-streaming.md`. The default is an in-process
    /// broadcast bus; production deployments swap in a Kafka-backed impl.
    pub events: Arc<dyn EventBus>,
}

impl AppState {
    /// Construct a single-tenant state for a local repo.
    pub fn single(repo: Arc<Repo>) -> Self {
        AppState {
            mode: Mode::NoAuth { repo },
            events: Arc::new(InMemoryBus::default()),
        }
    }

    /// Construct a multi-tenant state rooted at `tenants_base` with the
    /// registry file at `registry_path`.
    pub fn multi(tenants_base: PathBuf, registry_path: PathBuf) -> anyhow::Result<Self> {
        let registry = TenantRegistry::load(&registry_path)?;
        Ok(AppState {
            mode: Mode::MultiTenant {
                tenants_base,
                registry: Arc::new(Mutex::new(registry)),
                registry_path,
                repos: Mutex::new(HashMap::new()),
            },
            events: Arc::new(InMemoryBus::default()),
        })
    }

    /// Replace the default event bus. Useful for tests and for wiring a
    /// Kafka-backed bus at startup.
    pub fn with_events(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.events = bus;
        self
    }
}

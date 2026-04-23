//! Library surface of the OMP HTTP server.
//!
//! `AppState` holds the tenant registry, the tenants-base directory, and a
//! per-tenant `Repo` cache. The auth middleware is in `auth.rs`; routes in
//! `routes.rs` pull the already-resolved `Repo` out of a request extension.

pub mod auth;
pub mod routes;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use omp_core::api::Repo;
use omp_core::registry::TenantRegistry;
use omp_core::tenant::TenantId;

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
}

impl AppState {
    /// Construct a single-tenant state for a local repo.
    pub fn single(repo: Arc<Repo>) -> Self {
        AppState {
            mode: Mode::NoAuth { repo },
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
        })
    }
}

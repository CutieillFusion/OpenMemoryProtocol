//! Bearer-token auth middleware + tenant resolution.
//!
//! For each request that needs a `Repo` handle:
//!   1. In `NoAuth` mode, return the single shared repo.
//!   2. In `MultiTenant` mode, read `Authorization: Bearer <token>`, look it
//!      up in the registry, and get-or-open a `Repo` scoped to that tenant.
//!
//! Cross-tenant access is prevented by construction: each handler only ever
//! receives a `TenantRepo` for the caller's tenant, and `Repo` has no
//! operation that retargets a different tenant.

use std::path::Path;
use std::sync::Arc;

use axum::http::HeaderMap;

use omp_core::api::Repo;
use omp_core::registry::{EncryptionMode, Quotas};
use omp_core::tenant::TenantId;
use omp_core::OmpError;

use crate::{AppState, Mode};

/// A resolved request context: the caller's tenant plus the `Repo` scoped to
/// that tenant. Passed into handlers via `State` + header parsing.
pub struct TenantRepo {
    pub repo: Arc<Repo>,
    /// Mirrors the tenant's registry entry so handlers can refuse
    /// operations that collide with the tenant's encryption mode. See
    /// `docs/design/13-end-to-end-encryption.md §Migration and coexistence`.
    pub encryption_mode: EncryptionMode,
}

/// Resolve the caller's tenant from headers + state. Returns the per-tenant
/// `Repo`, creating and caching it on first use.
pub async fn resolve(state: &AppState, headers: &HeaderMap) -> omp_core::Result<TenantRepo> {
    match &state.mode {
        Mode::NoAuth { repo } => Ok(TenantRepo {
            repo: repo.clone(),
            // Single-tenant local mode is always plaintext — there's no
            // registry entry to consult, and the client talking to
            // `--no-auth` is the same process that would be holding any
            // encryption keys.
            encryption_mode: EncryptionMode::Plaintext,
        }),
        Mode::MultiTenant {
            tenants_base,
            registry,
            repos,
            ..
        } => {
            let token = extract_bearer(headers).ok_or_else(|| {
                OmpError::Unauthorized("missing or malformed Authorization header".into())
            })?;
            let (tenant_id, quotas, mode) = {
                let reg = registry.lock().await;
                let entry = reg
                    .by_token(&token)
                    .ok_or_else(|| OmpError::Unauthorized("invalid token".into()))?;
                (
                    entry.id.clone(),
                    entry.quotas.clone(),
                    entry.encryption_mode.clone(),
                )
            };
            let repo = get_or_open(repos, tenants_base, tenant_id, quotas).await?;
            Ok(TenantRepo {
                repo,
                encryption_mode: mode,
            })
        }
    }
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = v.to_str().ok()?;
    let tail = s.strip_prefix("Bearer ")?;
    Some(tail.trim().to_string())
}

async fn get_or_open(
    repos: &tokio::sync::Mutex<std::collections::HashMap<TenantId, Arc<Repo>>>,
    tenants_base: &Path,
    tenant_id: TenantId,
    quotas: Quotas,
) -> omp_core::Result<Arc<Repo>> {
    {
        let guard = repos.lock().await;
        if let Some(r) = guard.get(&tenant_id) {
            return Ok(r.clone());
        }
    }
    // Open (or init) the tenant's repo on first touch. Each tenant has a
    // private root at `<tenants_base>/<tenant_id>/`.
    let tenant_root = tenants_base.join(tenant_id.as_str());
    let repo = if tenant_root.join(".omp").exists() {
        Repo::open_tenant(&tenant_root, tenant_id.clone(), quotas)?
    } else {
        std::fs::create_dir_all(&tenant_root).map_err(|e| OmpError::io(&tenant_root, e))?;
        Repo::init_tenant(&tenant_root, tenant_id.clone(), quotas)?
    };
    let arc = Arc::new(repo);
    let mut guard = repos.lock().await;
    // Re-check: another task may have opened it in the meantime.
    if let Some(existing) = guard.get(&tenant_id) {
        return Ok(existing.clone());
    }
    guard.insert(tenant_id, arc.clone());
    Ok(arc)
}

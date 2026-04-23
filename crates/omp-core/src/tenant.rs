//! Tenant identity and per-tenant context.
//!
//! The [`TenantId`] is the unit of isolation. Every write path in `api.rs`
//! is scoped to one tenant; there is no function that crosses tenants.
//! In single-tenant (local CLI, or `omp-server --no-auth`) deployments,
//! the default tenant `_local` is used throughout.
//!
//! See `docs/design/11-multi-tenancy.md`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{OmpError, Result};

/// Tenant name. `[a-z0-9_-]{1,64}`. Opaque to OMP: no SSO, no org tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TenantId(String);

impl TenantId {
    pub const DEFAULT: &'static str = "_local";

    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        if s.is_empty() || s.len() > 64 {
            return Err(OmpError::InvalidPath(format!(
                "tenant id must be 1..=64 chars, got {}",
                s.len()
            )));
        }
        for (i, c) in s.chars().enumerate() {
            let ok = c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || c == '_'
                || c == '-'
                || (i > 0 && c == '_');
            if !ok {
                return Err(OmpError::InvalidPath(format!(
                    "tenant id has invalid character {c:?} at position {i}"
                )));
            }
        }
        Ok(TenantId(s))
    }

    pub fn local() -> Self {
        TenantId(Self::DEFAULT.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TenantId {
    type Err = OmpError;
    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_forms() {
        TenantId::new("alice").unwrap();
        TenantId::new("bob-2").unwrap();
        TenantId::new("team_a").unwrap();
        TenantId::new("_local").unwrap();
    }

    #[test]
    fn rejects_bad_forms() {
        assert!(TenantId::new("").is_err());
        assert!(TenantId::new("Alice").is_err()); // upper
        assert!(TenantId::new("a/b").is_err());
        assert!(TenantId::new("a".repeat(65)).is_err());
    }

    #[test]
    fn default_is_local() {
        assert_eq!(TenantId::local().as_str(), "_local");
    }
}

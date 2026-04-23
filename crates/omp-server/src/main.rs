//! OMP HTTP server.
//!
//! Two modes:
//! - `--no-auth` + `--repo <path>`: single-tenant local server (default when
//!   no tenants-base is set; matches the v1 shape).
//! - Multi-tenant: `--tenants-base <dir> --registry <path>`. Each tenant gets
//!   a private subdir under `tenants-base`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;

use omp_core::api::Repo;
use omp_core::config::LocalConfig;
use omp_server::{routes, AppState};

#[derive(Parser)]
#[command(name = "omp-server", about = "OMP HTTP server")]
struct Args {
    /// Bind address; falls back to `.omp/local.toml` or OMP_SERVER_BIND.
    #[arg(long)]
    bind: Option<String>,

    /// Single-tenant mode: serve one repo at this path. Mutually exclusive
    /// with --tenants-base.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Multi-tenant mode: tenants live under this directory as subdirs.
    /// Requires --registry.
    #[arg(long = "tenants-base")]
    tenants_base: Option<PathBuf>,

    /// Path to `tenants.toml`. Defaults to `<tenants-base>/admin/tenants.toml`.
    #[arg(long)]
    registry: Option<PathBuf>,

    /// In multi-tenant mode, bypass auth (uses tenant `_local` always).
    /// Only sensible for local development.
    #[arg(long = "no-auth")]
    no_auth: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    let (state, bind) = if let Some(tenants_base) = args.tenants_base.clone() {
        if args.repo.is_some() {
            bail!("--repo and --tenants-base are mutually exclusive");
        }
        let registry_path = args
            .registry
            .clone()
            .unwrap_or_else(|| omp_core::registry::default_registry_path(&tenants_base));

        let state = if args.no_auth {
            // Treat the tenants-base as a single-tenant repo rooted at
            // `<base>/_local/` — still multi-tenant on disk, but no token check.
            let root = tenants_base.join(omp_core::tenant::TenantId::DEFAULT);
            std::fs::create_dir_all(&root).context("creating _local root")?;
            let repo = if root.join(".omp").exists() {
                Repo::open(&root)?
            } else {
                Repo::init(&root)?
            };
            AppState::single(Arc::new(repo))
        } else {
            AppState::multi(tenants_base, registry_path)?
        };

        let bind = resolve_bind(&args.bind, None)?;
        (state, bind)
    } else {
        let repo_root = args
            .repo
            .clone()
            .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
        let repo = Repo::open(&repo_root).context("opening repo")?;
        let local = LocalConfig::load(repo.store().root()).context("loading local.toml")?;
        let bind = resolve_bind(&args.bind, Some(&local.server_bind))?;
        (AppState::single(Arc::new(repo)), bind)
    };

    let app = routes::router(Arc::new(state));

    tracing::info!(%bind, "OMP server listening");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn resolve_bind(flag: &Option<String>, from_local: Option<&str>) -> Result<SocketAddr> {
    let raw = flag
        .clone()
        .or_else(|| std::env::var("OMP_SERVER_BIND").ok())
        .or_else(|| from_local.map(|s| s.to_string()))
        .unwrap_or_else(|| "127.0.0.1:8000".to_string());
    raw.parse().context("parsing bind addr")
}

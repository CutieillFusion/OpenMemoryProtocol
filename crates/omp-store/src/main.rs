//! `omp-store` binary — runs the gRPC store service in front of a disk
//! backend rooted at `--repo`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use omp_core::store::disk::DiskStore;
use omp_store::StoreService;

#[derive(Parser)]
#[command(name = "omp-store", about = "OMP gRPC store service")]
struct Args {
    /// Bind address.
    #[arg(long, default_value = "127.0.0.1:9001")]
    bind: SocketAddr,

    /// Path to the repo root (the directory that contains `.omp/`). The store
    /// service reads/writes objects under `<repo>/.omp/`.
    #[arg(long)]
    repo: PathBuf,
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
    // Use init: idempotent — creates `.omp/` if missing, otherwise opens.
    // The K8s StatefulSet mounts a fresh PVC the first time and we want to
    // come up cleanly; subsequent restarts re-open the same store.
    let store = DiskStore::init(&args.repo)
        .with_context(|| format!("initializing DiskStore at {}", args.repo.display()))?;
    let svc = StoreService::new(Arc::new(store));

    tracing::info!(bind = %args.bind, repo = %args.repo.display(), "omp-store listening");
    omp_store::router(svc)
        .serve(args.bind)
        .await
        .context("serving gRPC store")?;
    Ok(())
}

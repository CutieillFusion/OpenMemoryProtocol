//! `omp-marketplace` binary — see `docs/design/23-probe-marketplace.md`.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use ed25519_dalek::VerifyingKey;
use omp_marketplace::{router, BuildSettings, MarketplaceState};

#[derive(Parser)]
#[command(name = "omp-marketplace", about = "OMP probe marketplace service")]
struct Args {
    /// Bind address.
    #[arg(long, default_value = "127.0.0.1:9200")]
    bind: SocketAddr,

    /// Root directory for the catalog file (`catalog.json`) and blob store
    /// (`blobs/<hash>`). Created if missing.
    #[arg(long, default_value = "/tmp/omp-marketplace")]
    data_root: PathBuf,

    /// Path to the gateway's Ed25519 verifying key (32 raw bytes). When
    /// set, all authed endpoints (publish/yank) require a valid
    /// `X-OMP-Tenant-Context` signed by this key. When unset (dev only),
    /// authed endpoints accept any context as a "demo mode" — log a loud
    /// warning. Production must set this.
    #[arg(long)]
    verifying_key: Option<PathBuf>,

    /// Path to the `probe-common` crate the build skeleton uses as a path
    /// dependency. Same default as `omp-builder`.
    #[arg(long, default_value = "../../probes-src/probe-common")]
    probe_common: PathBuf,

    /// Scratch directory for in-process publish builds. Recreated under here
    /// per publish, removed on completion.
    #[arg(long, default_value = "/tmp/omp-marketplace-build")]
    build_scratch: PathBuf,

    /// Wall-clock cap on each publish build (seconds). Builds that exceed
    /// this are killed and the publish returns 422.
    #[arg(long, default_value_t = 90)]
    build_wall_clock_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    std::fs::create_dir_all(&args.data_root)
        .with_context(|| format!("creating data_root {}", args.data_root.display()))?;

    let verifier = if let Some(p) = args.verifying_key.as_ref() {
        let bytes = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
        if bytes.len() != 32 {
            anyhow::bail!("verifying key must be exactly 32 bytes (got {})", bytes.len());
        }
        let arr: [u8; 32] = bytes.try_into().expect("32-byte key");
        let key = VerifyingKey::from_bytes(&arr).context("parsing verifying key")?;
        Some(key)
    } else {
        tracing::warn!(
            "no --verifying-key provided; running in dev/demo mode where authed endpoints \
             accept any TenantContext. NEVER USE IN PRODUCTION."
        );
        None
    };

    let build = BuildSettings {
        probe_common_path: args.probe_common,
        scratch_root: args.build_scratch,
        wall_clock_secs: args.build_wall_clock_secs,
    };
    let state = MarketplaceState::open(args.data_root, verifier, build)?;
    let app = router(state);

    tracing::info!(bind = %args.bind, "omp-marketplace listening");
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

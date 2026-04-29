//! `omp-gateway` binary — HTTP edge service that fronts shard backends.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use omp_gateway::{router, GatewayConfig, GatewayState};
use omp_tenant_ctx::GatewaySigner;

#[derive(Parser)]
#[command(name = "omp-gateway", about = "OMP HTTP gateway")]
struct Args {
    /// Bind address.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Path to gateway TOML config (shards, tokens).
    #[arg(long)]
    config: PathBuf,

    /// Path to a 32-byte Ed25519 signing key seed (raw bytes, not PEM).
    /// If absent, a fresh key is generated each start (fine for dev).
    #[arg(long)]
    signing_key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let mut config =
        GatewayConfig::from_toml_path(&args.config).context("loading gateway config")?;

    // Allow the WorkOS client secret to come from the environment instead of
    // the TOML file (per `docs/design/22-workos-auth.md` §Build & deployment).
    if let Some(ref mut wcfg) = config.workos {
        if wcfg.client_secret.is_empty() {
            if let Ok(s) = std::env::var("WORKOS_CLIENT_SECRET") {
                wcfg.client_secret = s;
            }
        }
        if wcfg.client_secret.is_empty() {
            tracing::warn!(
                "workos config present but client_secret is empty (set WORKOS_CLIENT_SECRET)"
            );
        }
    }

    let signer = if let Some(p) = args.signing_key {
        let seed = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        if seed.len() != 32 {
            anyhow::bail!("signing key must be exactly 32 bytes (got {})", seed.len());
        }
        let arr: [u8; 32] = seed.try_into().expect("32-byte seed");
        GatewaySigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&arr))
    } else {
        tracing::warn!("no --signing-key provided; generating an ephemeral one");
        GatewaySigner::generate()
    };

    let oidc = if let Some(wcfg) = config.workos.clone() {
        match omp_gateway::auth::OidcRuntime::discover(wcfg).await {
            Ok(rt) => {
                tracing::info!(issuer = %rt.config.issuer_url, "oidc discovery succeeded");
                Some(rt)
            }
            Err(e) => {
                tracing::error!(error = %e, "oidc discovery failed; auth routes will be disabled");
                None
            }
        }
    } else {
        None
    };

    let state = GatewayState::new_with_oidc(config, signer, oidc);
    let app = router(state);

    tracing::info!(bind = %args.bind, "omp-gateway listening");
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

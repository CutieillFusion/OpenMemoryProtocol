//! `omp-builder` binary entry point.

use std::path::PathBuf;

use clap::Parser;
use omp_builder::{router, BuilderConfig, BuilderState};
use tokio::net::TcpListener;

#[derive(Parser, Debug)]
#[command(name = "omp-builder", version)]
struct Args {
    /// Address to bind, e.g. `0.0.0.0:9100`.
    #[arg(long, default_value = "127.0.0.1:9100")]
    bind: String,

    /// Per-job scratch directory root. Builds go into
    /// `<scratch_root>/<job_id>/`.
    #[arg(long, default_value_t = std::env::temp_dir().join("omp-builder").display().to_string())]
    scratch_root: String,

    /// Filesystem path to the `probe-common` crate. The builder injects
    /// this as a path dep in the per-build skeleton's Cargo.toml.
    #[arg(long, default_value = "probes-src/probe-common")]
    probe_common_path: String,

    /// Wall-clock cap on cargo per build, in seconds.
    #[arg(long, default_value_t = 60)]
    wall_clock_secs: u64,

    /// Pod-wide concurrent build limit.
    #[arg(long, default_value_t = 4)]
    max_concurrent_builds: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,omp_builder=debug")),
        )
        .init();

    let args = Args::parse();
    let config = BuilderConfig {
        scratch_root: PathBuf::from(args.scratch_root),
        probe_common_path: PathBuf::from(args.probe_common_path),
        wall_clock_secs: args.wall_clock_secs,
        max_concurrent_builds: args.max_concurrent_builds,
    };
    tokio::fs::create_dir_all(&config.scratch_root).await?;

    let state = BuilderState::new(config);
    let app = router(state);
    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!(bind = %args.bind, "omp-builder listening");
    axum::serve(listener, app).await?;
    Ok(())
}

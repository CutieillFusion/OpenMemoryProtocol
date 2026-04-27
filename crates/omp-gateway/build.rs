//! Build script for `omp-gateway`.
//!
//! When the `embed-ui` feature is enabled (default), `rust_embed` reads the
//! `frontend/build/` directory at *compile* time. If that directory is
//! missing, this script tries to build it by running `npm ci && npm run
//! build` in `frontend/`. Three escape hatches when that fails:
//!
//!   1. Run the npm build manually before `cargo build`.
//!   2. Set `OMP_SKIP_UI_BUILD=1` and stage `frontend/build/` yourself
//!      (this is what the Dockerfile does — Node runs in a separate stage).
//!   3. `cargo build --no-default-features` to drop the UI entirely.
//!
//! See `docs/design/19-web-frontend.md` §Build & deployment.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Only relevant when the embed-ui feature is on.
    if env::var("CARGO_FEATURE_EMBED_UI").is_err() {
        return;
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let frontend_dir = manifest_dir
        .join("..")
        .join("..")
        .join("frontend")
        .canonicalize()
        .unwrap_or_else(|_| manifest_dir.join("../../frontend"));
    let build_dir = frontend_dir.join("build");
    let fallback = build_dir.join("200.html");

    // Tell cargo to re-run when source files change. NOT `frontend/build`
    // (that's our output) — and NOT `frontend/node_modules` (irrelevant).
    println!(
        "cargo:rerun-if-changed={}",
        frontend_dir.join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        frontend_dir.join("static").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        frontend_dir.join("svelte.config.js").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        frontend_dir.join("vite.config.ts").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        frontend_dir.join("package.json").display()
    );
    println!("cargo:rerun-if-env-changed=OMP_SKIP_UI_BUILD");

    if env::var("OMP_SKIP_UI_BUILD").is_ok() {
        ensure_build_dir_exists(&build_dir, &fallback);
        return;
    }

    if fallback.exists() {
        // Already built. Nothing to do.
        return;
    }

    // Try to build with npm.
    eprintln!(
        "omp-gateway build.rs: frontend/build/ is missing — running `npm ci && npm run build` in {}",
        frontend_dir.display()
    );

    let npm_ci = Command::new("npm")
        .arg("ci")
        .current_dir(&frontend_dir)
        .status();
    if let Err(e) = npm_ci {
        fail_with_help(&frontend_dir, &format!("`npm ci` failed to launch: {e}"));
    } else if let Ok(s) = npm_ci {
        if !s.success() {
            fail_with_help(&frontend_dir, "`npm ci` exited non-zero");
        }
    }

    let npm_build = Command::new("npm")
        .args(["run", "build"])
        .current_dir(&frontend_dir)
        .status();
    if let Err(e) = npm_build {
        fail_with_help(
            &frontend_dir,
            &format!("`npm run build` failed to launch: {e}"),
        );
    } else if let Ok(s) = npm_build {
        if !s.success() {
            fail_with_help(&frontend_dir, "`npm run build` exited non-zero");
        }
    }

    if !fallback.exists() {
        fail_with_help(
            &frontend_dir,
            "frontend/build/200.html still missing after `npm run build`",
        );
    }
}

fn ensure_build_dir_exists(_build_dir: &Path, fallback: &Path) {
    if !fallback.exists() {
        eprintln!(
            "omp-gateway build.rs: OMP_SKIP_UI_BUILD set but {} is missing.",
            fallback.display()
        );
        eprintln!(
            "Pre-stage the directory or unset OMP_SKIP_UI_BUILD to let the build script run npm."
        );
        eprintln!("Or build with `--no-default-features` to drop the embedded UI.");
        std::process::exit(1);
    }
}

fn fail_with_help(frontend_dir: &Path, why: &str) -> ! {
    eprintln!("omp-gateway build.rs: {why}");
    eprintln!();
    eprintln!("The `embed-ui` feature requires `frontend/build/` to exist at compile time.");
    eprintln!("You have three options:");
    eprintln!(
        "  1. Run `npm ci && npm run build` in {} yourself.",
        frontend_dir.display()
    );
    eprintln!(
        "  2. Pre-stage the directory and set OMP_SKIP_UI_BUILD=1 (what the Dockerfile does)."
    );
    eprintln!("  3. Build with `cargo build --no-default-features` to drop the embedded UI.");
    std::process::exit(1);
}

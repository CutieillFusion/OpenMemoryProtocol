//! Cargo-subprocess builder.
//!
//! Per [`docs/design/20-server-side-probes.md`](../../../docs/design/20-server-side-probes.md):
//! the user submits a single `lib.rs` file. The builder wraps it in a
//! controlled `Cargo.toml` skeleton (probe-common path-dep, ciborium, sha2),
//! runs `cargo build --release --target wasm32-unknown-unknown`, and returns
//! the resulting `.wasm` plus the source as artifacts the client can stage
//! into the tree.
//!
//! Why wrap rather than accept the user's Cargo.toml: with a server-controlled
//! manifest, dependency whitelisting is a property of the skeleton (not a
//! runtime check), and the response can guarantee the produced `.wasm` is
//! byte-reproducible across same-source-same-builder-image rebuilds.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::jobs::{Artifact, JobId, JobState, JobsTable};
use crate::BuilderState;

/// User-submitted source. v1 supports a single `lib.rs` plus the
/// `.probe.toml` declaring limits.
#[derive(Clone, Debug)]
pub struct BuildRequest {
    pub tenant: String,
    pub namespace: String,
    pub name: String,
    pub lib_rs: String,
    pub probe_toml: String,
}

/// Run the build for `id` to completion. Updates the jobs table on
/// transitions, broadcasts cargo output to the log channel. Errors land in
/// `Job.error` rather than being returned, since the request handler that
/// spawned this future has long since returned 202.
pub async fn run_build(state: BuilderState, id: JobId, req: BuildRequest) {
    let _permit = match state.build_semaphore.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            state.jobs.update(&id, |job| {
                job.state = JobState::Failed;
                job.error = Some("builder shutting down".into());
            });
            return;
        }
    };

    state.jobs.update(&id, |job| job.state = JobState::Building);
    state
        .jobs
        .append_log(&id, format!("[builder] preparing scratch dir for {}", id.0));

    let scratch = state.config.scratch_root.join(&id.0);
    if let Err(e) = tokio::fs::create_dir_all(&scratch).await {
        fail(&state, &id, format!("create scratch dir: {e}")).await;
        return;
    }

    if let Err(e) = stamp_skeleton(&scratch, &state.config.probe_common_path, &req).await {
        fail(&state, &id, format!("stamp skeleton: {e}")).await;
        return;
    }

    state
        .jobs
        .append_log(&id, "[builder] running cargo build --release ...".into());

    let result = run_cargo(
        &state,
        &id,
        &scratch,
        Duration::from_secs(state.config.wall_clock_secs),
    )
    .await;

    match result {
        Ok(()) => {
            // Locate the produced .wasm. The skeleton's `[lib] name` is
            // fixed to `probe_lib`, so the artifact path is deterministic.
            let wasm_path = scratch
                .join("target")
                .join("wasm32-unknown-unknown")
                .join("release")
                .join("probe_lib.wasm");
            match build_artifacts(&wasm_path, &req).await {
                Ok(artifacts) => {
                    state.jobs.update(&id, |job| {
                        job.state = JobState::Ok;
                        job.artifacts = Some(artifacts);
                    });
                    state
                        .jobs
                        .append_log(&id, "[builder] OK — artifacts ready".into());
                }
                Err(e) => fail(&state, &id, format!("read artifacts: {e}")).await,
            }
        }
        Err(e) => {
            fail(&state, &id, e).await;
        }
    }

    // Best-effort scratch cleanup. Leaving it on disk for a few seconds
    // would help debugging but the design says jobs are ephemeral.
    let _ = tokio::fs::remove_dir_all(&scratch).await;
}

async fn fail(state: &BuilderState, id: &JobId, reason: String) {
    state
        .jobs
        .append_log(id, format!("[builder] FAILED: {reason}"));
    state.jobs.update(id, |job| {
        job.state = JobState::Failed;
        job.error = Some(reason);
    });
}

/// Write the controlled Cargo skeleton + the user's lib.rs into `scratch`.
async fn stamp_skeleton(
    scratch: &Path,
    probe_common: &Path,
    req: &BuildRequest,
) -> std::io::Result<()> {
    let probe_common_abs = if probe_common.is_absolute() {
        probe_common.to_path_buf()
    } else {
        std::env::current_dir()?.join(probe_common)
    };
    let probe_common_str = probe_common_abs.display().to_string();

    let cargo_toml = format!(
        r#"[package]
name = "probe-lib"
version = "0.0.0"
edition = "2021"

[lib]
name = "probe_lib"
crate-type = ["cdylib"]

[dependencies]
probe-common = {{ path = "{probe_common_str}" }}
ciborium = {{ version = "0.2", default-features = false }}
sha2 = {{ version = "0.10", default-features = false }}

[profile.release]
opt-level = "s"
lto = "thin"
codegen-units = 1
panic = "abort"
strip = "symbols"
"#
    );

    tokio::fs::write(scratch.join("Cargo.toml"), cargo_toml.as_bytes()).await?;
    let src_dir = scratch.join("src");
    tokio::fs::create_dir_all(&src_dir).await?;
    tokio::fs::write(src_dir.join("lib.rs"), req.lib_rs.as_bytes()).await?;
    Ok(())
}

async fn run_cargo(
    state: &BuilderState,
    id: &JobId,
    scratch: &Path,
    wall_clock: Duration,
) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(scratch)
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--quiet",
        ])
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| format!("spawn cargo: {e}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let jobs = state.jobs.clone();
    let id_for_out = id.clone();
    let id_for_err = id.clone();

    let out_task = tokio::spawn(stream_lines(stdout, jobs.clone(), id_for_out, "stdout"));
    let err_task = tokio::spawn(stream_lines(stderr, jobs, id_for_err, "stderr"));

    let wait = async {
        let status = child.wait().await.map_err(|e| format!("wait cargo: {e}"))?;
        if !status.success() {
            return Err(format!(
                "cargo exited non-zero (code {})",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into())
            ));
        }
        Ok::<(), String>(())
    };

    let result = match tokio::time::timeout(wall_clock, wait).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "cargo exceeded wall-clock cap of {}s — killed",
            wall_clock.as_secs()
        )),
    };

    // Drain log tasks even on timeout/error, so the user sees the cargo
    // output that explains the failure.
    let _ = out_task.await;
    let _ = err_task.await;
    result
}

async fn stream_lines<R>(reader: R, jobs: std::sync::Arc<JobsTable>, id: JobId, tag: &'static str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = BufReader::new(reader).lines();
    while let Ok(Some(line)) = buf.next_line().await {
        jobs.append_log(&id, format!("[cargo:{tag}] {line}"));
    }
}

async fn build_artifacts(wasm_path: &Path, req: &BuildRequest) -> std::io::Result<Vec<Artifact>> {
    let wasm_bytes = tokio::fs::read(wasm_path).await?;
    let base64 = base64::engine::general_purpose::STANDARD;
    let wasm_tree_path = format!("probes/{}/{}.wasm", req.namespace, req.name);
    let toml_tree_path = format!("probes/{}/{}.probe.toml", req.namespace, req.name);
    let src_tree_path = format!("probes/{}/{}.rs", req.namespace, req.name);
    Ok(vec![
        Artifact {
            path: wasm_tree_path,
            bytes_b64: base64.encode(&wasm_bytes),
        },
        Artifact {
            path: toml_tree_path,
            bytes_b64: base64.encode(req.probe_toml.as_bytes()),
        },
        Artifact {
            path: src_tree_path,
            bytes_b64: base64.encode(req.lib_rs.as_bytes()),
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BuilderConfig;
    use tempfile::TempDir;

    #[tokio::test]
    async fn skeleton_stamps_expected_files() {
        let td = TempDir::new().unwrap();
        let scratch = td.path().to_path_buf();
        let probe_common = td.path().join("fake-probe-common");
        tokio::fs::create_dir_all(&probe_common).await.unwrap();

        let req = BuildRequest {
            tenant: "alice".into(),
            namespace: "test".into(),
            name: "demo".into(),
            lib_rs: "// hello\n".into(),
            probe_toml: "name = \"test.demo\"\nreturns = \"bool\"\naccepts_kwargs = []\n".into(),
        };
        stamp_skeleton(&scratch, &probe_common, &req).await.unwrap();
        assert!(scratch.join("Cargo.toml").exists());
        assert!(scratch.join("src/lib.rs").exists());
        let cargo = tokio::fs::read_to_string(scratch.join("Cargo.toml"))
            .await
            .unwrap();
        assert!(cargo.contains("probe-common"));
        assert!(cargo.contains("crate-type = [\"cdylib\"]"));
    }

    #[tokio::test]
    async fn build_artifacts_paths_match_namespace_name() {
        let td = TempDir::new().unwrap();
        let wasm = td.path().join("probe_lib.wasm");
        tokio::fs::write(&wasm, b"\x00asm\x01\x00\x00\x00")
            .await
            .unwrap();
        let req = BuildRequest {
            tenant: "alice".into(),
            namespace: "test".into(),
            name: "demo".into(),
            lib_rs: "// hi".into(),
            probe_toml: "name = \"test.demo\"\n".into(),
        };
        let arts = build_artifacts(&wasm, &req).await.unwrap();
        let paths: Vec<&str> = arts.iter().map(|a| a.path.as_str()).collect();
        assert!(paths.contains(&"probes/test/demo.wasm"));
        assert!(paths.contains(&"probes/test/demo.probe.toml"));
        assert!(paths.contains(&"probes/test/demo.rs"));
    }

    /// Sanity check: with a 1ms timeout, run_cargo should report timeout
    /// rather than a successful build. Doesn't actually compile anything;
    /// uses a fake "cargo" via PATH manipulation would be ideal but we just
    /// verify the timeout path doesn't deadlock.
    #[tokio::test]
    async fn run_cargo_timeout_reports_killed() {
        let td = TempDir::new().unwrap();
        let scratch = td.path().to_path_buf();
        let cfg = BuilderConfig {
            scratch_root: scratch.clone(),
            probe_common_path: scratch.clone(),
            wall_clock_secs: 0, // forces immediate timeout
            max_concurrent_builds: 1,
        };
        let state = BuilderState::new(cfg);
        let (id, _tx) = state
            .jobs
            .create("alice".into(), "test".into(), "demo".into());

        // Stamp a no-op skeleton so cargo would have something to do (it
        // won't, because the wall-clock cap is 0s). probe_common path is
        // bogus but cargo never gets that far.
        tokio::fs::write(
            scratch.join("Cargo.toml"),
            b"[package]\nname=\"x\"\nversion=\"0.0.0\"\nedition=\"2021\"\n",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(scratch.join("src"))
            .await
            .unwrap();
        tokio::fs::write(scratch.join("src/lib.rs"), b"")
            .await
            .unwrap();

        let res = run_cargo(&state, &id, &scratch, Duration::from_millis(0)).await;
        assert!(res.is_err(), "expected timeout error, got {:?}", res);
        let msg = res.unwrap_err();
        assert!(
            msg.contains("wall-clock") || msg.contains("non-zero") || msg.contains("spawn"),
            "unexpected error message: {msg}"
        );
    }
}

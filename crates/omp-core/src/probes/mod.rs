//! Probe loading + runtime orchestration.
//!
//! Two concerns live here:
//! 1. The **starter pack**: compiled `.wasm` blobs embedded at build time plus
//!    their sibling `.probe.toml` manifests. `omp init` writes these into a
//!    fresh repo's `probes/` directory.
//! 2. Running a probe: given the blob bytes, kwargs, and config, execute it
//!    inside a wasmtime sandbox and return the CBOR-encoded result.
//!
//! The ABI, sandbox configuration, and security story are described in
//! `docs/design/05-probes.md`.

pub mod cbor;
pub mod host;
pub mod manifest;
pub mod starter;

pub use host::{run_probe, ProbeConfig, ProbeResult};
pub use manifest::{ProbeLimitsOverride, ProbeManifest};

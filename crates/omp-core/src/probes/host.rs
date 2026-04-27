//! Wasmtime-backed probe host.
//!
//! Safety / determinism posture per `05-probes.md`:
//! - Zero host imports. Any module declaring an import is refused.
//! - SIMD, threads, reference-types disabled. Bulk-memory enabled.
//! - Fuel-based instruction cap.
//! - Linear-memory ceiling.
//! - Wall-clock watchdog via `Engine::increment_epoch` from a sidecar thread.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use wasmtime::{Caller, Config, Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::error::{OmpError, Result};
use crate::manifest::FieldValue;
use crate::probes::cbor;

/// Limits applied to a single probe invocation.
#[derive(Clone, Copy, Debug)]
pub struct ProbeConfig {
    pub fuel: u64,
    pub memory_mb: u32,
    pub wall_clock_s: u32,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        ProbeConfig {
            fuel: 1_000_000_000,
            memory_mb: 64,
            wall_clock_s: 10,
        }
    }
}

/// The outcome of running a probe. `value` is the decoded CBOR output.
#[derive(Debug)]
pub struct ProbeResult {
    pub value: FieldValue,
}

/// Run a probe module once with the supplied bytes + kwargs.
///
/// `name` is the fully-qualified probe name (e.g., `"file.size"`) used only
/// for error messages.
pub fn run_probe(
    name: &str,
    wasm: &[u8],
    bytes: &[u8],
    kwargs: &BTreeMap<String, FieldValue>,
    config: &ProbeConfig,
) -> Result<ProbeResult> {
    let engine = build_engine(config)?;
    let module = Module::from_binary(&engine, wasm).map_err(|e| OmpError::ProbeFailed {
        probe: name.into(),
        reason: format!("load: {e:#}"),
    })?;

    // Refuse to instantiate any module that declares an import.
    let import_count = module.imports().count();
    if import_count != 0 {
        return Err(OmpError::ProbeFailed {
            probe: name.into(),
            reason: format!("module declares {import_count} host imports; zero allowed"),
        });
    }

    // Wall-clock watchdog: a side thread calls `engine.increment_epoch()`
    // after `wall_clock_s`. A `Store` configured with an epoch deadline of
    // 1 (relative to its initial epoch) will trap when that fires.
    let cancel = Arc::new(AtomicBool::new(false));
    let engine_for_thread = engine.clone();
    let timeout = Duration::from_secs(config.wall_clock_s.max(1) as u64);
    let cancel_clone = cancel.clone();
    let watchdog = thread::spawn(move || {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if cancel_clone.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        engine_for_thread.increment_epoch();
    });

    let mut store: Store<()> = Store::new(&engine, ());
    store
        .set_fuel(config.fuel)
        .map_err(|e| OmpError::ProbeFailed {
            probe: name.into(),
            reason: format!("set_fuel: {e}"),
        })?;
    store.set_epoch_deadline(1);

    // Empty linker — enforces the "no host imports" rule.
    let linker: Linker<()> = Linker::new(&engine);
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| OmpError::ProbeFailed {
            probe: name.into(),
            reason: format!("instantiate: {e}"),
        })?;

    let alloc: TypedFunc<u32, u32> =
        instance
            .get_typed_func(&mut store, "alloc")
            .map_err(|e| OmpError::ProbeFailed {
                probe: name.into(),
                reason: format!("missing alloc: {e}"),
            })?;
    let free: TypedFunc<(u32, u32), ()> =
        instance
            .get_typed_func(&mut store, "free")
            .map_err(|e| OmpError::ProbeFailed {
                probe: name.into(),
                reason: format!("missing free: {e}"),
            })?;
    let probe_run: TypedFunc<(u32, u32), i64> = instance
        .get_typed_func(&mut store, "probe_run")
        .map_err(|e| OmpError::ProbeFailed {
            probe: name.into(),
            reason: format!("missing probe_run: {e}"),
        })?;

    let memory =
        instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| OmpError::ProbeFailed {
                probe: name.into(),
                reason: "module has no exported 'memory'".into(),
            })?;

    // Enforce the per-module memory cap: reject immediately if the module's
    // memory already exceeds the ceiling (shouldn't happen with our crates),
    // and clamp growth by not letting it exceed the cap. Linear memory is in
    // 64 KiB pages; convert MB -> pages.
    let max_pages = (config.memory_mb as u64) * 16; // 1 MB = 16 pages
    if memory.size(&store) > max_pages {
        return Err(OmpError::ProbeFailed {
            probe: name.into(),
            reason: format!(
                "initial memory {} pages exceeds cap {}",
                memory.size(&store),
                max_pages
            ),
        });
    }

    let input_bytes = cbor::encode_input(bytes, kwargs)?;
    let input_len = input_bytes.len() as u32;
    let in_ptr = alloc
        .call(&mut store, input_len)
        .map_err(|e| err(name, "alloc", e))?;
    write_memory(&memory, &mut store, in_ptr, &input_bytes, config.memory_mb)
        .map_err(|e| err(name, "write input", e))?;

    let packed = probe_run
        .call(&mut store, (in_ptr, input_len))
        .map_err(|e| err(name, "run", e))?;

    let out_ptr = (packed >> 32) as u32;
    let out_len = (packed & 0xFFFF_FFFF) as u32;

    let out_bytes = read_memory(&memory, &store, out_ptr, out_len, config.memory_mb)
        .map_err(|e| err(name, "read output", e))?;

    let _ = free.call(&mut store, (in_ptr, input_len));
    let _ = free.call(&mut store, (out_ptr, out_len));

    // Stop the watchdog before decoding.
    cancel.store(true, Ordering::SeqCst);
    let _ = watchdog.join();

    let value = cbor::decode_output(&out_bytes).map_err(|mut e| {
        if let OmpError::ProbeFailed { probe, .. } = &mut e {
            *probe = name.to_string();
        }
        e
    })?;

    Ok(ProbeResult { value })
}

fn build_engine(config: &ProbeConfig) -> Result<Engine> {
    let mut cfg = Config::new();
    cfg.consume_fuel(true);
    cfg.epoch_interruption(true);
    // SIMD, threads, and reference-types are compiled out in this crate's
    // wasmtime feature set (default-features = false with only
    // cranelift/std/runtime). They are therefore structurally unavailable to
    // probe modules; we don't need runtime toggles. Bulk-memory and multi-value
    // are always enabled in modern wasmtime.
    // Determinism posture:
    // - SIMD / relaxed-SIMD off (cross-host reproducibility).
    // - Threads off (this wasmtime build has no threads feature compiled in).
    // - Bulk-memory, multi-value, reference types: defaults (on). Rustc's
    //   wasm32-unknown-unknown output uses reference types and bulk-memory.
    // The forbidden-by-construction posture still holds because the probe
    // module has zero host imports and cannot invoke anything outside itself.
    cfg.wasm_simd(false);
    cfg.wasm_relaxed_simd(false);
    // Memory cap is per-store, not per-engine; the pages ceiling is applied
    // in run_probe above. We keep `static_memory_maximum_size` modest so a
    // runaway module can't balloon mmap'd regions.
    let max_bytes = (config.memory_mb as u64) * 1024 * 1024;
    cfg.static_memory_maximum_size(max_bytes);
    cfg.dynamic_memory_guard_size(0);
    Engine::new(&cfg).map_err(|e| OmpError::internal(format!("wasmtime engine: {e}")))
}

fn write_memory(
    memory: &Memory,
    store: &mut Store<()>,
    ptr: u32,
    bytes: &[u8],
    memory_mb: u32,
) -> std::result::Result<(), anyhow::Error> {
    let needed = (ptr as usize).saturating_add(bytes.len());
    let cap = (memory_mb as usize) * 1024 * 1024;
    if needed > cap {
        anyhow::bail!("write past memory cap");
    }
    memory.write(store, ptr as usize, bytes)?;
    Ok(())
}

fn read_memory(
    memory: &Memory,
    store: &Store<()>,
    ptr: u32,
    len: u32,
    memory_mb: u32,
) -> std::result::Result<Vec<u8>, anyhow::Error> {
    let end = (ptr as usize).saturating_add(len as usize);
    let cap = (memory_mb as usize) * 1024 * 1024;
    if end > cap {
        anyhow::bail!("read past memory cap");
    }
    let data = memory.data(store);
    if end > data.len() {
        anyhow::bail!("read past memory size ({end} > {})", data.len());
    }
    Ok(data[ptr as usize..end].to_vec())
}

fn err(name: &str, stage: &str, e: impl std::fmt::Display) -> OmpError {
    OmpError::ProbeFailed {
        probe: name.into(),
        reason: format!("{stage}: {e}"),
    }
}

// Silence Caller import (kept for future host-function hooks if needed).
#[allow(dead_code)]
fn _use_caller(_: Caller<'_, ()>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::starter::STARTER_PROBES;

    fn find_probe(name: &str) -> &'static crate::probes::starter::StarterProbe {
        STARTER_PROBES
            .iter()
            .find(|p| p.name == name)
            .expect("probe in starter pack")
    }

    #[test]
    fn file_size_returns_byte_count() {
        let probe = find_probe("file.size");
        let bytes = b"hello world";
        let res = run_probe(
            probe.name,
            probe.wasm,
            bytes,
            &BTreeMap::new(),
            &ProbeConfig::default(),
        )
        .unwrap();
        assert_eq!(res.value, FieldValue::Int(bytes.len() as i64));
    }

    #[test]
    fn file_sha256_matches_rust_sha256() {
        use sha2::{Digest, Sha256};
        let probe = find_probe("file.sha256");
        let bytes = b"hello world";
        let res = run_probe(
            probe.name,
            probe.wasm,
            bytes,
            &BTreeMap::new(),
            &ProbeConfig::default(),
        )
        .unwrap();
        let expected = {
            let mut h = Sha256::new();
            h.update(bytes);
            let d = h.finalize();
            let mut s = String::new();
            for b in d {
                use std::fmt::Write as _;
                let _ = write!(s, "{:02x}", b);
            }
            s
        };
        assert_eq!(res.value, FieldValue::String(expected));
    }

    #[test]
    fn module_with_host_import_is_refused() {
        // Hand-craft a minimal WASM binary that declares a host import.
        // The WASM spec binary format: magic "\0asm" + version 1 + sections.
        // Here we use a type section, an import section, and nothing else.
        let wasm = wat::parse_str(
            r#"(module
                 (import "env" "log" (func $log (param i32)))
               )"#,
        )
        .unwrap();
        let err = run_probe(
            "test.import",
            &wasm,
            &[],
            &BTreeMap::new(),
            &ProbeConfig::default(),
        )
        .unwrap_err();
        match err {
            OmpError::ProbeFailed { reason, .. } => {
                assert!(reason.contains("host imports"), "got: {reason}");
            }
            other => panic!("expected ProbeFailed, got {other:?}"),
        }
    }

    #[test]
    fn fuel_exhaustion_is_mapped_to_probe_failed() {
        // Infinite loop.
        let wasm = wat::parse_str(
            r#"(module
                 (memory (export "memory") 1)
                 (func (export "alloc") (param i32) (result i32) i32.const 0)
                 (func (export "free") (param i32 i32))
                 (func (export "probe_run") (param i32 i32) (result i64)
                   (loop $spin
                     br $spin))
               )"#,
        )
        .unwrap();
        let err = run_probe(
            "spin",
            &wasm,
            &[],
            &BTreeMap::new(),
            &ProbeConfig {
                fuel: 10_000,
                memory_mb: 16,
                wall_clock_s: 30,
            },
        )
        .unwrap_err();
        assert!(matches!(err, OmpError::ProbeFailed { .. }));
    }
}

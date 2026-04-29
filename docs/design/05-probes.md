# 05 — Probes

Probes are **WebAssembly modules** that extract structural information from file bytes. They are the only extractors OMP runs during ingest, and every probe is a blob committed to the repo's tree. OMP ships no probes of its own — it ships a WASM runtime, the probe ABI, and a *starter pack* that `omp init` drops into the new repo. Adding a new probe is a tree commit, never an OMP release.

This is a change from earlier drafts, which described probes as Python functions inside OMP's package. Motivation: adding a new file type shouldn't require cutting a new OMP release. Moving probes to the tree makes them behave like schemas — data that time-travels with the repo — while keeping safety by running them in the WASM sandbox.

## Why WebAssembly

Three reasons the sandbox has to be WASM, not subprocess-sandboxed Python or an in-process DSL:

1. **Safety by construction.** A WASM module has no syscalls, no filesystem, no network — not because OMP takes them away but because the instruction set has no way to express them. A probe sees its own linear memory and the input buffer. If OMP doesn't import a function into the module, the module physically cannot invoke it. No denylist to maintain, no sandbox escape history to worry about.
2. **Determinism.** WASM execution is bit-for-bit reproducible given the same inputs. With SIMD and threads disabled (which OMP does), probe outputs are stable across machines and across wasmtime versions. Manifest hashes stay stable.
3. **Language flexibility.** Any language that targets `wasm32-unknown-unknown` — Rust, C/C++, AssemblyScript, Zig — can author a probe. Novel filetypes that need a custom parser are expressible; no OMP code change is needed.

## Where probes live

Every probe is two co-located files under `probes/<namespace>/<name>.{wasm,probe.toml}`:

```
probes/
  file/
    size.wasm
    size.probe.toml
    mime.wasm
    mime.probe.toml
    sha256.wasm
    sha256.probe.toml
```

A schema reference `probe = "pdf.page_count"` resolves to `probes/pdf/page_count/probe.wasm` + `probes/pdf/page_count/probe.toml` in the current tree (per [`23-probe-marketplace.md`](./23-probe-marketplace.md), every probe lives in its own directory and may also carry `README.md` and a `source/` companion). Both required files are stored as `blob` entries — the same rule as `schemas/*.schema` and `omp.toml`.

Probes time-travel with the rest of the repo: at `--at <commit>`, the probes in effect are whatever was in `probes/` at that commit.

## The probe manifest — `probe.toml`

```toml
name = "pdf.page_count"       # must match <namespace>/<name> file path
returns = "int?"              # a supported manifest type; trailing "?" = nullable
accepts_kwargs = []           # declared kwargs; schema's `args` keys must be a subset
description = "PDF page count via a Rust pypdf port compiled to WASM."

[limits]
# All optional; unset fields fall back to omp.toml's [probes] defaults.
# A probe's TOML can lower these; it cannot raise them above the omp.toml ceiling.
memory_mb = 64
fuel = 1_000_000_000
wall_clock_s = 10
```

Probes declare their return type against the same type system as schemas (see 04-schemas.md). `returns = "int?"` means "int or null"; a null return triggers the schema field's `fallback`, if any.

There is no `wasm_hash` field — OMP derives it from the sibling `.wasm` blob.

## The ABI

Every probe module exports exactly three functions:

```
(func $alloc     (param i32) (result i32))      ;; allocate a buffer in module memory
(func $free      (param i32 i32))               ;; release a buffer
(func $probe_run (param i32 i32) (result i64))  ;; run the probe
```

**Host imports: none.** No `log`, no clock, no randomness, no WASI. If a module declares any import, OMP refuses to instantiate it. Purity and determinism are enforced structurally — no reviewer vigilance required.

### Calling convention

1. Host CBOR-encodes `{"bytes": <file bytes>, "kwargs": {<schema args>}}`.
2. Host calls `alloc(input_len)` → pointer into module memory.
3. Host writes the CBOR payload at that pointer.
4. Host calls `probe_run(input_ptr, input_len)` → returns an `i64`. The high 32 bits are the output pointer; the low 32 bits are the output length.
5. Host reads CBOR from the output buffer.
6. Host calls `free(input_ptr, input_len)` and `free(output_ptr, output_len)`.

Output CBOR is a JSON-equivalent value. CBOR-null means the probe returned null, and the schema field's fallback (if any) takes over.

**Why CBOR and not JSON?** Raw file bytes are in the input payload. JSON would force base64-encoding every PDF, image, and audio file crossing the ABI. CBOR passes bytes natively, at significantly lower overhead.

## Sandbox configuration

Every probe instantiation runs in wasmtime with:

- **Fuel**: per-module cap, from `probe.toml` or `[probes]` default (1B instructions). Exhaustion traps the module.
- **Memory**: per-module cap, from `probe.toml` or `[probes]` default (64 MB). `memory.grow` past the cap traps.
- **Wall-clock watchdog**: a host-side thread interrupts probes exceeding `wall_clock_s` (default 10s). Belt-and-suspenders over fuel; cheap insurance.
- **SIMD**: disabled — cross-host determinism risk.
- **Threads**: disabled.
- **Reference types**: disabled.
- **Bulk memory ops**: enabled (practical codecs need them).
- **Import allowlist**: empty. Any declared import → refuse to instantiate.

These flags are part of the fixed ABI (see 10-why-no-v2.md) — they are not configurable per-probe.

## The starter probe pack

OMP ships a deliberately tiny starter pack as compiled WASM blobs bundled as package data. `omp init` writes them into the new repo's `probes/` directory alongside the starter schema. The v1 starter pack is three probes — all universal, all non-trivial.

### `file.*` — universal

- **`file.size`** → `int` — byte length.
- **`file.mime`** → `string` — detected MIME type via header sniff; falls back to `application/octet-stream`.
- **`file.sha256`** → `string` — hex SHA-256 of raw bytes. Note this is the *blob's own hash*, distinct from the framed-object hash used in the tree.

That's it. No `file.name` (the basename is already in the ingest request's `path`), no `text.*` probes (line counts / first-lines / frontmatter are genuinely useful but they belong with a committed `schemas/text.schema` + `probes/text/*` pair in whatever repo actually needs them, not baked into every fresh `omp init`), and no `pdf.*` stubs (a starter pack that returns null for half its probes is worse than no starter probe at all — it adds entries to `probe_hashes` that carry no information). Real `text.*` / `pdf.*` / `image.*` / `audio.*` probes are iteration-2 work; adding them is a committed blob in the target repo, not an OMP release.

Starter-pack source lives in the OMP repo (Rust, targeting `wasm32-unknown-unknown`); the sibling `probes-src/` cargo workspace builds the probes, and each compiled `.wasm` blob is embedded into the host binary via `include_bytes!` at build time. Users see only the compiled blobs. Auditors can inspect the source at the OMP repo.

One toolchain covers both sides — the host binary and the probes ship from one build flow. Adding a new starter probe is a new crate in `probes-src/`, not a cross-language build dance.

Iteration 2 ships additional starter probes (real `pdf.*`, `image.*`, `audio.*`) as a new starter-pack release. No OMP API changes, no wire-format changes.

## Writing a new probe

For a novel filetype not covered by the starter pack:

**1. Write the probe in a WASM-targeting language.** Rust example:

```rust
#[no_mangle]
pub unsafe extern "C" fn alloc(size: usize) -> *mut u8 { /* ... */ }

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut u8, size: usize) { /* ... */ }

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: *const u8, in_len: usize) -> u64 {
    let input: Input = cbor_decode(slice::from_raw_parts(in_ptr, in_len));
    let result = extract(input.bytes, input.kwargs);
    let encoded = cbor_encode(&result);
    let out_ptr = alloc(encoded.len()) as u64;
    write(out_ptr as *mut u8, &encoded);
    (out_ptr << 32) | (encoded.len() as u64)
}
```

**2. Compile**: `cargo build --release --target wasm32-unknown-unknown`.

**3. Write the sibling manifest** at `probes/<namespace>/<name>/probe.toml` declaring `name`, `returns`, kwargs.

**4. Stage** (per-probe folder layout from [`23-probe-marketplace.md`](./23-probe-marketplace.md)):

```
POST /files path=probes/<namespace>/<name>/probe.wasm  file=@target/.../module.wasm
POST /files path=probes/<namespace>/<name>/probe.toml  file=@probe.toml
POST /files path=probes/<namespace>/<name>/README.md   file=@README.md   # optional
```

**5. Dry-run** via `POST /test/ingest` with a sample file and a schema that references the new probe. OMP validates the probe loads, runs it, and returns the would-be manifest without staging.

**6. Commit** via `POST /commit` if the dry-run looks right.

After commit, any schema can reference `probe = "<namespace>.<name>"`. Existing schemas and manifests are unaffected.

## Time-travel: `probe_hashes` on manifests

Because probes are blobs that can change independently of schemas, `schema_hash` alone no longer fully pins the extraction. Each manifest records a `probe_hashes` map listing the **framed-object hash** of every probe that fired during ingest — the same hash the tree uses to reference that probe's `.wasm` blob (see 02-object-model.md).

Given any historical manifest, replaying its exact extraction is: read `probe_hashes` → `ObjectStore.get(hash)` to fetch each `.wasm` blob → run them on the file at that commit. Bit-identical output guaranteed.

## Anti-patterns

Things that are **not** legitimate probes:

- **"Call an external API."** Forbidden by construction — no imports, no network.
- **"Summarize with an LLM."** Same. This is what `user_provided` fields are for (see 04-schemas.md).
- **"Write a thumbnail to disk."** Same — no syscalls.
- **"Use non-deterministic features (timestamps, randomness)."** No clock or random imports are provided; probes needing entropy must derive it from input bytes.
- **"Probe that takes 30 seconds on 1 MB of input."** Not strictly forbidden, but the default wall-clock cap kills it. Raise `wall_clock_s` only when the probe's work is inherently expensive (e.g., extracting text from a huge PDF), and document the cost in its `description`.

## Why this shape

- **Single extension model.** Schemas, probes, and repo config are all tree-committed data. No separate "native plugin" concept.
- **Safe by construction.** The WASM sandbox isn't guarding against malicious probes; it's structurally incapable of hosting them.
- **Time-travel is free.** Probes are blobs; they behave like any other versioned artifact.
- **No OMP release ever needed for new filetypes.** Once the ABI is frozen, every new filetype is purely a repo commit.

# 10 — Why v2 is unlikely to be needed

The goal of the design is that v1 survives contact with the real world. "v2" here means a **breaking change to the object model or wire format** — not "adding features," which is supposed to happen freely. A new file type, new manifest fields, new schemas, new probes: all of those are v1.x additions. A change that breaks how existing objects are read or hashed: that's v2.

This doc names the **fixed points** — the decisions that, if wrong, would force a v2 — and argues each is defensible enough that it probably isn't.

## The fixed points

### 1. SHA-256 of canonical wire bytes

Every object's hash is `sha256(<framed bytes>)`. Canonical framing is `<type> <size>\0<content>`, zlib-compressed for storage but hashed pre-compression. This is Git's convention with SHA-1 replaced by SHA-256.

**Why it holds up:** SHA-256 is not cryptographically broken. Git itself is migrating to SHA-256 for the same reasons. If SHA-256 ever needs replacement, Git's migration plan is the template: dual-hash for a transition period, then cut over. That's a well-understood refactor, not a redesign.

**What would force a rewrite:** a break in the underlying assumption that content addressing is the right identity model. No known reason to expect this.

### 2. Git-style object framing

Stored bytes are `<type> <size>\0<content>`, zlib-compressed. Loose objects under `.omp/objects/<first-2-hex>/<remaining>`.

**Why it holds up:** it's been the Git model for 20 years, works, is widely understood, interoperates with plenty of existing tooling. The compression isn't fancy, the layout isn't clever — it's just durable.

**What would force a rewrite:** adding pack files for efficiency at scale. Not a wire-format break — pack files can coexist with loose objects as a later optimization, just as they do in Git.

### 3. `ObjectStore` as the single storage contract

Nine methods (put, get, has, iter_refs, read_ref, write_ref, delete_ref, read_head, write_head). Everything storage-related goes through them.

**Why it holds up:** the interface leaks nothing about implementation — no paths, no SQL, no connection strings. Implementations can be disk, S3, Postgres, anything. If we ever discover an operation that belongs in the interface but isn't there (e.g., batch fetch for performance), that's an additive change to the interface, not a replacement.

**What would force a rewrite:** discovering that the interface is wrong at a deeper level — for example, that content-addressed puts need to be transactional across multiple objects. This would be bad but recoverable: extend the interface, keep the old methods, migrate callers.

### 4. Closed set of four field sources plus fallback wrapper

`constant`, `probe`, `user_provided`, `field`, with `fallback` as a wrapper that attaches to any of the four. New source types in future versions are additive (old schemas don't mention them, so they don't break), but the idea of *having* a closed set is fixed.

**Why it holds up:** every enrichment scenario we've thought of is expressible:
- Static values → `constant`
- File-derived structural metadata → `probe`
- LLM-computed fields → `user_provided`
- Composed values → `field`
- Graceful degradation → any of the above wrapped in `fallback`

The scenarios that might *seem* to need a new source — "call an external API to enrich this field" — are correctly an anti-pattern (OMP doesn't call out). The caller does that work externally and submits via `user_provided`.

**What would force a rewrite:** none come to mind.

### 5. WASM probe ABI

Every probe is a WebAssembly module committed as a blob in the tree (see [`05-probes.md`](./05-probes.md)). The module exports exactly three functions — `alloc(i32) -> i32`, `free(i32, i32)`, `probe_run(i32, i32) -> i64` — and declares zero imports. The input/output encoding is CBOR on both sides. The sandbox runs with SIMD, threads, and reference types disabled; fuel, memory, and wall-clock caps are enforced.

**Why it holds up:** the interface is tiny. Three exports, one calling convention, zero host surface. There's nothing to drift. Any language that targets `wasm32-unknown-unknown` can produce a conforming module, so the ABI doesn't constrain the ecosystem. Zero imports means probes are structurally pure — no way to express network/FS/clock access in the bytecode at all. Determinism is a wasmtime config away.

**What would force a rewrite:** an extraction requirement that genuinely needs a side effect or nondeterminism — which is expressly not what probes are for (side effects and LLM calls belong in `user_provided` fields). A more plausible pressure is "probes want to share a hashing stdlib to avoid shipping SHA-256 inside every `.wasm`." Answer: add a namespaced, time-bounded host import for pure utilities. Additive change, doesn't invalidate existing probes.

## The explicitly-designed-for-change points

These are the things v1 intentionally makes easy to edit after the fact:

### New file types

Commit a new schema file at `schemas/<type>.schema`. Zero code change. The LLM can do this.

### New manifest fields

Edit a schema. Zero code change.

### New LLM enrichment logic

The LLM runs its own prompt in its own process, submits results as `user_provided`. OMP doesn't change at all.

### New storage backend

Implement `ObjectStore`'s nine methods against a new substrate (S3, Postgres, Blob storage, in-memory). ~200–400 lines. No change to ingest, API, CLI.

### New deployment target

If the target can run a static Rust binary + disk, the disk backend works. If not, use the appropriate `ObjectStore` implementation from the point above.

### New extraction primitive (probe)

Write a WASM module conforming to the probe ABI, commit it at `probes/<namespace>/<name>.{wasm,probe.toml}`. Existing schemas continue to work; new schemas can reference the new probe. Zero OMP code change, zero OMP release. This is the *only* way a new probe is ever added now — there is no host-language registry, no plugin system, nothing but the ABI.

### More manifest structure

The manifest TOML has a `[fields]` subtable whose shape is schema-defined. Adding structure *around* `[fields]` (new top-level keys in manifests) would be a format change; extending `[fields]` is a schema change.

If we discover a genuinely new top-level manifest concept (say, a `[lineage]` block recording transformation history), that's a carefully-considered format extension — old manifests without the field are readable (treat missing as empty), new ones have it. Backwards-compatible addition, not a wire break.

## The things that *might* force a v2 someday

Full intellectual honesty: these are the scenarios where v2 is genuinely plausible.

### Drop-in replacement of TOML for manifests

If it turns out TOML is the wrong format (say, we need arbitrary-depth nesting with sharp type distinctions, or we need JSON interoperability that can't be faked), switching to something else is a manifest-format break. Mitigation: a read-both / write-new period, then a one-time rewrite commit.

### Replacing the tree text format

Plain-text trees are nice for inspectability but awkward for very wide directories (10k entries per directory). If a real scaling need emerges, moving to a binary tree format (Git's) is a one-type format break. Objects in the store would have to be rewritten (`omp admin verify` could handle this during a migration).

### Content addressing on more than just file bytes

Today, the blob's hash is of its raw bytes; the manifest's hash is of its canonical TOML. If we ever want to content-address something that isn't bytes or TOML (unlikely, but possible), we'd need a different identity strategy for those objects.

None of these are near-term concerns.

## The disciplined position

The design is saying: **we expect to ship new features continuously for years without ever changing how objects are framed, hashed, or stored.** Each new capability either fits into existing primitives (a schema, a probe, a field source composition) or exercises an already-anticipated extension point (a new backend behind `ObjectStore`).

If that's wrong in a way we didn't foresee, it'll be because a requirement emerged that we genuinely couldn't have anticipated. That's fine — deal with it then, with the benefit of real-world data about what went wrong. The alternative, "design for every future requirement up front," is how projects spend their first year building for things no one ever asks for.

v1 is a bet that the fixed points are small and defensible. If this doc is still accurate in two years, the bet worked.

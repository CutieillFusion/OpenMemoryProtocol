# 00 — Overview

## What OMP is

OpenMemoryProtocol (OMP) is a **Git-like content store, specialized for files that LLMs work with**. Think of it as "a Git server whose primary client is an LLM agent, not a human developer."

Everything OMP does is derivable from two bets:

> **Bet 1 — Every stored file carries a rich, LLM-consumable manifest alongside its bytes.**
>
> Git's unit is an opaque blob. OMP's unit is `{blob + manifest}`. The manifest is structured text (TOML) — title, summary, tags, structural metadata, and whatever custom fields the repo's active schema declares. An LLM can browse the store by reading manifests only — cheap, fast, no content loading — and fetches raw bytes only when it needs them.

> **Bet 2 — The manifest shape, and the extractors that populate it, are data, not code.**
>
> Each repo has *schema files* in its tree (`schemas/pdf.schema`, `schemas/audio.schema`, etc.) that declare what a manifest for that file type should contain. The **probes** those schemas reference are also tree blobs — WebAssembly modules committed under `probes/`, sandboxed to pure, deterministic execution. Adding a new file type = committing a new schema (and, if the filetype is genuinely novel, a new WASM probe). The LLM can compose schemas freely; authoring a new WASM probe needs a compile-to-WASM toolchain. Either way, no one touches OMP's code and no release is cut.

Everything else is Git: content-addressable storage, Merkle trees, commits, branches, DAG, time-travel. On-disk layout mirrors Git's loose-object store. Commits are Git-style text. Tree format mirrors Git's nested-tree model. If you know Git, you know most of OMP's plumbing; the novelty is concentrated in the manifest + schema layer.

## What OMP is NOT

- **Not an LLM client.** OMP never calls an LLM. No API keys, no provider config, no network dependency. Tests are fully deterministic and hermetic.
- **Not a tool framework.** OMP doesn't define `read_pdf_pages` or `transcribe_audio`. Whoever integrates OMP with their LLM builds their own tool surface on top.
- **Not a RAG layer.** No embeddings, no semantic search in v1.
- **Not a microservice project.** v1 is a single focused Rust crate. Decomposition into services is an explicit future concern, not a day-one one.

## Who uses OMP, and how

OMP is meant to be hosted like a Git server: **many independent tenants**, each with their own repos and refs, sandboxed from one another the way Git hosts isolate accounts. A deployment serves multiple concurrent agents without cross-tenant bleed. Single-tenant `omp serve` on a laptop is the degenerate case; the design target is the hosted case.

The primary client is an **LLM agent** running somewhere — a coding assistant, a research agent, a pipeline that processes documents. That agent:

1. Computes whatever metadata it wants for a file (title, summary, tags) using its own LLM.
2. Calls OMP to store the file + those fields.
3. OMP auto-fills structural metadata (page count, duration, dimensions) via **probes** — small deterministic WebAssembly modules committed to the repo's tree, sandboxed to have no syscalls or host imports.
4. OMP validates the combined manifest against the schema for that file type and stores it, content-addressed, in a commit.

Later, the agent (or a different agent on a different branch) can:
- List files, filtered by path or tag.
- Fetch a manifest without fetching the file bytes.
- Fetch the file bytes when needed.
- **Time-travel**: retrieve a file's manifest as it was at a past commit, and retrieve the schema that produced it.
- Fork a branch, modify a schema, re-ingest files under the new schema, commit, compare.

## The demo moment

The one thing that should make a viewer go "oh, that's interesting":

**Time-travel through an LLM's beliefs.** An agent's view of a document changes over time — new summaries, corrected tags, richer metadata. OMP preserves every version of those views, linked to the exact schema that produced them. Asking "what did the LLM believe about this PDF on April 10 vs April 21?" is one command.

## The non-negotiables

These five decisions are fixed points — if they're wrong, we rewrite. Everything else is meant to be editable:

1. **Content addressing with canonical hashing.** Every object's identity is determined by the canonical bytes of its wire format, hashed pre-compression with SHA-256.
2. **Git-style on-disk format.** Loose objects in `.omp/objects/ab/cdef...`, zlib-compressed, prefixed with `<type> <size>\0`. Refs as text files.
3. **`ObjectStore` as a narrow interface.** The storage backend is pluggable (disk for v1; S3 / Postgres / SQLite later) behind this one interface.
4. **Closed set of four field sources plus a fallback wrapper.** Schemas compose fields from `constant`, `probe`, `user_provided`, and `field` — and nothing else. `fallback` wraps any of the four. New source types are additive; the idea of *having* a closed set is fixed.
5. **WASM probe ABI.** Every probe is a WebAssembly module exporting `alloc`, `free`, and `probe_run`, with zero host imports, CBOR on the wire, and deterministic sandbox flags. Tiny surface, no drift path, and structurally pure — the bytecode has no way to express I/O.

See [`10-why-no-v2.md`](./10-why-no-v2.md) for the full treatment of which decisions are fixed and which are meant to be edited.

## Document map

- [`01-on-disk-layout.md`](./01-on-disk-layout.md) — what files live where in a repo
- [`02-object-model.md`](./02-object-model.md) — blob, tree, manifest, commit: wire formats
- [`03-hierarchical-trees.md`](./03-hierarchical-trees.md) — nested trees and path resolution
- [`04-schemas.md`](./04-schemas.md) — schema TOML spec, field sources, validation
- [`05-probes.md`](./05-probes.md) — the bounded set of built-in extractors
- [`06-api-surface.md`](./06-api-surface.md) — HTTP and CLI operations
- [`07-config.md`](./07-config.md) — versioned repo config vs machine-local config
- [`08-deployability.md`](./08-deployability.md) — where OMP runs, what it needs
- [`09-roadmap.md`](./09-roadmap.md) — v1 scope, iteration 2, deferred items
- [`10-why-no-v2.md`](./10-why-no-v2.md) — why the design should outlive v1

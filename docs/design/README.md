# OpenMemoryProtocol — Design docs

These docs capture the v1 design before any code is written. Each doc is focused on one topic so you can open an issue or comment against a specific section without rereading the whole thing.

## Reading order

Start with the overview, then read the technical docs in order — each builds on the previous. The roadmap and fixed-points docs sit at the end because they make more sense once you know what the system actually does.

1. [**00-overview.md**](./00-overview.md) — What OMP is, what it isn't, who uses it, the demo moment.
2. [**01-on-disk-layout.md**](./01-on-disk-layout.md) — Directory structure: working tree vs. `.omp/`, where schemas and user files live.
3. [**02-object-model.md**](./02-object-model.md) — The four content-addressable object types (blob, tree, manifest, commit) and their wire formats.
4. [**03-hierarchical-trees.md**](./03-hierarchical-trees.md) — Nested trees (Git-style) and path resolution through them.
5. [**04-schemas.md**](./04-schemas.md) — Schema TOML spec, the four field sources (plus `fallback` wrapper), validation, and dry-run iteration.
6. [**05-probes.md**](./05-probes.md) — Probes are WASM modules committed to the tree; the starter pack + ABI + sandbox config.
7. [**06-api-surface.md**](./06-api-surface.md) — HTTP and CLI operations, the `omp_core::api` contract that powers both.
8. [**07-config.md**](./07-config.md) — Versioned repo config (`omp.toml`) vs. machine-local config (`.omp/local.toml`).
9. [**08-deployability.md**](./08-deployability.md) — `ObjectStore` abstraction and supported deployment targets (local, Docker, K8s; serverless deferred).
10. [**09-roadmap.md**](./09-roadmap.md) — v1 scope, iteration 2 additions, and explicitly-deferred items.
11. [**10-why-no-v2.md**](./10-why-no-v2.md) — Which design decisions are fixed points, which are meant to be edited, and why we think v1 should survive.
12. [**11-multi-tenancy.md**](./11-multi-tenancy.md) — Tenant model, auth boundary, per-tenant namespace over `ObjectStore`, quota strategy. Headline feature of iteration 2.
13. [**12-large-files.md**](./12-large-files.md) — Per-file sizes up to 200 GB via a new `chunks` object type, streaming ingest, and probe gating. Additive; preserves all five fixed points.
14. [**13-end-to-end-encryption.md**](./13-end-to-end-encryption.md) — Client-side encryption so the server never sees plaintext; keys derived from a user passphrase, shares via age-style X25519 recipient wraps. Defense-in-depth on top of `11-multi-tenancy.md`.
15. [**14-microservice-decomposition.md**](./14-microservice-decomposition.md) — Service seams (gateway, ingest, object-store, refs, query); hybrid wire (gRPC for the object-store data plane, HTTP/JSON for the control plane); HTTP `If-Match` for ref CAS; signed tenant context; one concrete cross-service flow.
16. [**15-query-and-discovery.md**](./15-query-and-discovery.md) — Predicate grammar over manifest fields, cursor pagination, change feed. Reconciles Bet 1 with what the API actually delivers at scale.
17. [**16-event-streaming.md**](./16-event-streaming.md) — Six event types over a Kafka/Redpanda broker; topic-per-type, tenant-partitioned; at-least-once delivery; broker as notification, not source of truth.
18. [**17-deployment-k8s.md**](./17-deployment-k8s.md) — Per-service Deployment/StatefulSet shapes, Helm chart structure, scaling story for refs, network policies, dev vs. prod values files.
19. [**18-observability.md**](./18-observability.md) — Three operator planes (logs, metrics, traces) plus a tenant-facing audit log stored as objects in the tree.

## How to give feedback

Each doc is a markdown file. Comments, edits, or "change this section" requests against a specific file are fastest — the filename + section heading uniquely identifies what to change.

If a design decision spans multiple docs and you want to revisit it, flag it at the top level; editing in several places at once is fine.

## What these docs are NOT

- Not implementation plans — the step-by-step build order lives in `/home/norquistd/.claude/plans/humming-tumbling-marble.md` (outside the repo).
- Not exhaustive specs — they cover the shape of the design, not every edge case. Edge cases are the implementation's job.
- Not marketing — they're written for collaborators evaluating whether the design is coherent.

## One-line summary for each

| Doc | One line |
|---|---|
| 00-overview | OMP = Git for LLM files; every file has a manifest; manifest shape is schema-driven data. |
| 01-on-disk-layout | Working tree has user files + `schemas/` + `omp.toml`; `.omp/` holds private state like Git's `.git/`. |
| 02-object-model | Four types (blob, tree, manifest, commit); Git-style framing with SHA-256. |
| 03-hierarchical-trees | Nested trees (each directory is its own object); paths are walks, not flat names. |
| 04-schemas | TOML files declaring manifest shape; four field sources plus a fallback wrapper; dry-run ingest for safe iteration. |
| 05-probes | WASM modules in `probes/` extract structural metadata; sandboxed, deterministic, tree-versioned. Adding a new filetype is a repo commit, not an OMP release. |
| 06-api-surface | `omp_core::api` is the contract; HTTP + CLI are thin adapters; small surface, staging-then-commit. |
| 07-config | `omp.toml` (versioned, semantic) vs. `.omp/local.toml` (machine-local, ephemeral). |
| 08-deployability | `ObjectStore` is the narrow backend interface; disk v1; S3/Postgres are backend swaps. |
| 09-roadmap | v1 is a 2–3 week Rust core; iteration 2 adds multi-tenancy + image/audio + merge + alt backends; rest is deferred. |
| 10-why-no-v2 | Five fixed points (SHA-256, framing, `ObjectStore`, four field sources + fallback wrapper, WASM probe ABI); everything else is designed for change. |
| 11-multi-tenancy | Tenant = unit of isolation; `TenantStore` wraps `ObjectStore`; Bearer-token auth middleware; per-tenant quotas and locks. |
| 12-large-files | Files up to 200 GB via a chunked Merkle `chunks` object + streaming ingest; probes gate on `max_input_bytes`; no wire-format break. |
| 13-end-to-end-encryption | Client holds the keys; server stores ciphertext; sharing via X25519 wraps. Probes move to the client. Fixed points untouched. |
| 14-microservice-decomposition | Five services (gateway, ingest, object-store, refs, query); gRPC for the data plane only, HTTP/JSON for the rest; HTTP `If-Match` for ref CAS; signed tenant context; idempotency keys. |
| 15-query-and-discovery | Predicate filtering over manifest fields, cursor pagination, change feed. Makes Bet 1 hold at hosted-multi-tenant scale. |
| 16-event-streaming | Six event types on Kafka/Redpanda; tenant-partitioned topics; broker is notification optimization, not source of truth. |
| 17-deployment-k8s | Helm chart with per-service Deployments/StatefulSets, mTLS via cert-manager, network policies, observability sub-charts, dev/prod values. |
| 18-observability | Logs/metrics/traces for operators plus a tenant-facing audit log stored as objects; cardinality-aware metric labels; OTel tracing through probe spans. |

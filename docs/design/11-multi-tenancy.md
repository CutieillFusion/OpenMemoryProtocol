# 11 — Multi-tenancy

OMP is hosted the way Git hosting services are hosted: **many tenants share one deployment; each tenant's repo state is strictly sandboxed from every other tenant's.** This doc captures the tenant model, the auth boundary, the namespace that layers over `ObjectStore`, and the quota strategy.

v1 ships single-tenant. Multi-tenancy is the headline iteration-2 feature. Everything here is designed so the v1 core works unchanged — the tenant layer wraps it, doesn't modify it.

## Model

A **tenant** is the unit of isolation. Think of it as a GitHub account or a GitLab namespace: one tenant owns one or more repos, has its own refs, its own quotas, and cannot see or affect any other tenant.

Within a tenant, nothing changes from v1: branches, commits, schemas, probes, trees, manifests — all identical. The tenancy layer is external to the object model.

```
  /tenants/alice/repos/docs/    ← independent ObjectStore namespace
  /tenants/alice/repos/videos/
  /tenants/bob/repos/default/   ← no shared state with alice
```

Tenants are named (e.g., `alice`) or anonymous-by-token. Names are opaque to OMP — no SSO, no email, no organization tree. A tenant registry (see below) maps token → tenant id; everything downstream uses the id.

## The tenant boundary in `ObjectStore`

Every key in the `ObjectStore` trait is scoped by tenant. Two shapes of backend compose this differently:

- **Disk backend**: each tenant gets a directory under `/repo/tenants/<tenant-id>/`. Ref files, HEAD, and objects all live inside. No flag changes, no code paths that cross the boundary.
- **S3 / Postgres backend**: tenant id is prepended to object keys (`t/<tenant>/objects/<hash>`) and row keys (`WHERE tenant_id = $1`). The backend enforces this at the query layer; callers can't forget.

The `ObjectStore` trait itself doesn't grow a `tenant` parameter. Instead, a tenant-scoped `ObjectStore` is constructed by wrapping the raw backend:

```rust
pub struct TenantStore<S: ObjectStore> {
    inner: S,
    tenant: TenantId,
}

impl<S: ObjectStore> ObjectStore for TenantStore<S> { /* prepends tenant to every key */ }
```

The `Repo` handle carries its `TenantStore` by value. **Cross-tenant access is a compile error** — a request handler for tenant `alice` cannot invoke operations against tenant `bob`'s `Repo` because it doesn't have a handle to one. This is exactly the kind of safety Rust's type system buys you that language-level docs can't.

## Auth boundary

An `axum` middleware layer terminates auth before any `omp_core::api` call:

```
Authorization: Bearer <token>
```

The middleware looks up the token in a **tenant registry** — a simple indexed table keyed by a hash of the token, returning `(tenant_id, quota_ref)`. The registry lives in its own backend (initially a TOML file on disk; iteration 2+ can move it to Postgres without touching request paths).

- Invalid token → `401`.
- Valid token → request handler receives a `TenantCtx { tenant: TenantId, quotas: Quotas }` extractor; all downstream calls route through it.
- Missing token on a route marked `#[tenant_required]` → `401`. Marked public routes (`/status`, `/healthz`) bypass.

Token issuance is explicitly out of scope for the initial tenant-layer release; tokens are minted by an admin CLI command (`omp admin tenant create <name>`) and handed out manually. OAuth / SSO is deferred.

## Quotas

Per-tenant limits, evaluated on every write path:

- **Total stored bytes** (sum of object content sizes; soft-checked on `put`, hard-checked per commit).
- **Object count**.
- **Per-request probe fuel ceiling** (lower than the global max, configurable per tenant).
- **Per-request wall-clock ceiling** (same).
- **Concurrent write operations** (a tenant can have at most one in-flight commit; reads are unlimited).

Quotas live next to tenant metadata in the registry. Exceeding a quota returns `429 quota_exceeded` with a JSON body naming which limit tripped.

The probe sandbox already enforces fuel and wall-clock at the WASM level per [`05-probes.md`](./05-probes.md); per-tenant caps sit *below* the global `[probes]` ceiling and clip earlier for noisier tenants.

## Concurrency invariants

The single-writer invariant from v1 becomes **single-writer-per-tenant**. Tenants are independent, so:

- Two different tenants can have concurrent in-flight commits.
- Two requests against the same tenant's repo are serialized by a tenant-scoped lock (in-process `tokio::sync::Mutex` per tenant id in the initial tenant-layer release; promoted to a distributed lease — Postgres advisory lock, Redis lock, or S3 conditional writes — when scaling past one replica).
- Reads are never serialized against each other, even within a tenant.

Horizontal scale to multiple replicas becomes possible once lock state moves out of the process. Until then, multi-replica deployments route a given tenant to a single replica (consistent-hash on tenant id) and scale by distributing the tenant population, not by parallelizing one tenant across replicas.

## What multi-tenancy does *not* do

- **Does not add LLM calls.** Tenant-aware or not, OMP still never calls out. User-provided fields are the only way enrichment data enters.
- **Does not change probe determinism.** The tenant boundary is above the probe sandbox; probe inputs are still just `{bytes, kwargs}`, and probe determinism is unaffected.
- **Does not introduce cross-tenant sharing.** No "public repos," no cross-tenant reads, no shared object deduplication in v1 of the tenant layer. Content-addressed dedup *across* tenants is a deferred optimization (attractive, but a per-object reference-counting concern that doesn't fit in iteration 2). A cryptographic sharing primitive is designed in [`13-end-to-end-encryption.md`](./13-end-to-end-encryption.md) and lands with that feature, not here.
- **Does not defend against the server operator.** The tenant boundary is a type-system boundary enforced by code OMP controls; anyone with shell on the host can read every tenant's plaintext from `.omp/objects/`. Closing that gap is the job of end-to-end encryption — see [`13-end-to-end-encryption.md`](./13-end-to-end-encryption.md).
- **Does not define tenants-within-tenants.** No organizations, no teams, no sub-namespaces. One flat list of tenants. If organization structure is needed later, it wraps this layer rather than replacing it.

## Relationship to the course-project microservices story

The tenant layer is a clean seam to split along for the microservices grade (see [`09-roadmap.md`](./09-roadmap.md)). Once tenants exist, the natural service decomposition is:

- **Gateway / auth service** — token verification, tenant lookup, routing.
- **Ingest service** — probes + engine, per-tenant.
- **Store service** — `ObjectStore` trait exposed over gRPC, per-backend.
- **Refs service** — commit serialization and ref CAS.
- **Query service** — listing, time-travel, search.

Each service sees the tenant id on the wire; the gateway verifies it, downstream services trust it. This is the shape that earns the microservices + event-streaming extra credit without designing v1 around the split.

## Fixed points this layer does *not* move

All five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md) remain untouched:

1. SHA-256 of canonical wire bytes — unchanged (hashes are per-object, tenant-agnostic).
2. Git-style object framing — unchanged.
3. `ObjectStore` as the single storage contract — tenant scoping is a wrapping layer, not a new method.
4. Four field sources + fallback wrapper — unchanged.
5. WASM probe ABI — unchanged.

Adding multi-tenancy is additive. Removing it reverts to v1 cleanly. The layer's correctness is localized; the core object model proves itself independently.

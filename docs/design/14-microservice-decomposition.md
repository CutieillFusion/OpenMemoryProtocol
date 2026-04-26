# 14 — Microservice decomposition

OMP v1 is a single Rust binary. v1.5 (multi-tenancy + large files + E2E encryption) is the same single binary with more features. This doc describes how that single binary is split into a handful of cooperating services without changing what `omp_core::api` does or how the object model works.

The split has two purposes. First, a hosted multi-tenant deployment scales by giving each part of the request path its own resource envelope (ingest is CPU-heavy, object-store is I/O-heavy, the gateway is connection-heavy — they don't share a sweet spot). Second, the decomposition is the seam the course-project bonus categories hang off (event streaming, K8s orchestration, multiple storage backends).

This doc is a design, not an implementation plan. It commits to seams, contracts, and one concrete cross-service flow. Everything else is left to the implementation step.

## Implementation status

What ships in code today (see `crates/omp-store`, `crates/omp-store-client`, `crates/omp-gateway`, `crates/omp-tenant-ctx`):

- ✅ **`proto/store.proto` + the Store gRPC service** — `omp-store` binary serves it; `omp-store-client::RemoteStore` is a sync `ObjectStore` impl that talks gRPC. Verified by 5 integration tests at `crates/omp-store-client/tests/grpc_round_trip.rs`.
- ✅ **Gateway service** with Bearer auth, sha256-of-tenant routing across N shards, signed tenant-context propagation, and 412→409 status translation. 4 integration tests at `crates/omp-gateway/tests/proxy_routing.rs`.
- ✅ **`X-OMP-Tenant-Context` Ed25519-signed context** — `omp-tenant-ctx` crate with 5 unit tests covering signing, verification, expiry rejection, tampering rejection, and wrong-key rejection.

What's deferred from the design above:

- ⏸ **Per-service split into ingest / refs / query** — The current implementation uses a horizontal-scale pattern (gateway + sharded `omp-server` backends, each running the full HTTP API). The reason is that `omp_core::api::Repo` is concretely typed to `DiskStore`; making it generic over `Arc<dyn ObjectStore>` is a wide-blast-radius refactor (~30 sites in `api.rs` use disk-specific paths and locks). The gRPC `Store` service is implemented and tested, so the abstraction is in place when the refactor happens.
- ⏸ **mTLS between services** — designed below; the chart in [`17-deployment-k8s.md`](./17-deployment-k8s.md) ships cert-manager wiring, but the v1 implementation runs with plain HTTP/HTTP2 inside the cluster trust boundary. mTLS is a deployment-flag flip when wanted.
- ⏸ **HTTP `If-Match` ref CAS at the wire** — the `412→409` translation is wired in the gateway; the actual `If-Match` check is the next change to `omp-server`'s ref handlers.

## What does *not* change

- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). Every service participates in the same SHA-256 / Git-style framing / TOML manifest / WASM probe ABI / closed field-source set. The split rearranges *where* the bytes live, not *what* the bytes are.
- The `omp_core::api` contract from [`06-api-surface.md`](./06-api-surface.md). It remains the single source of truth for operation shapes — both the gRPC `Store` service and the internal HTTP routes are mechanical projections of it.
- The tenant boundary from [`11-multi-tenancy.md`](./11-multi-tenancy.md). Tenant id rides on every internal call; no service is tenant-blind.
- The probe sandbox from [`05-probes.md`](./05-probes.md). Probes still execute in `wasmtime` with zero host imports; the only change is *which* service hosts the runtime.

If a service split would force any of the above to change, the split is wrong, not the constraint.

## The five services

The seams are chosen so that each service owns one verb of the request path: *terminate, run, store, sequence, query*. Six if you count the client as a service; we don't.

| Service | Owns | Stateless? | Scales on |
|---|---|---|---|
| **Gateway** | TLS termination, auth, tenant resolution, rate limiting, request routing. | Yes | Connection count |
| **Ingest** | Probe execution, schema validation, manifest assembly. | Yes | CPU |
| **Object store** | The `ObjectStore` trait, behind a wire interface. | Effectively yes (state lives in the backend it wraps) | Backend I/O |
| **Refs** | Ref reads/writes, commit serialization, single-writer-per-tenant invariant. | No (holds tenant locks) | Per-tenant write QPS |
| **Query** | Listing, time-travel reads, manifest predicate queries (see [`15-query-and-discovery.md`](./15-query-and-discovery.md)). | Yes (cache-only state) | Read QPS |

The CLI and the LLM agent are clients. They talk only to the gateway.

```
   ┌───────────┐
   │  client   │  (CLI / LLM agent / curl)
   └─────┬─────┘
         │ HTTPS  (per 06-api-surface.md)
   ┌─────▼─────┐
   │  gateway  │  auth, tenant lookup, routing, rate limit
   └──┬──┬──┬──┘
      │  │  │  HTTP/JSON + gRPC (mTLS, signed tenant context)
   ┌──▼┐ │  └────────────┐
   │ingest│             ┌▼──────┐
   └──┬──┘              │ query │
      │                 └───┬───┘
      │                     │
   ┌──▼─────────────────────▼──┐
   │       object store        │  put/get/has/iter
   └──────────────┬────────────┘
                  │
            ┌─────▼─────┐
            │   refs    │  read_ref / write_ref / CAS
            └───────────┘
```

Refs is drawn beneath object-store because it depends on the same backend storage but enforces tenant-scoped serialization on top. In a single backend (one disk, one Postgres, one S3 bucket) refs and object-store can be co-located in the same pod and split later; the wire boundary still exists.

## Wire format

OMP uses **two transports internally**, chosen per service for value, not uniformity:

- **gRPC over HTTP/2 with mTLS** — for the **object-store data plane only**. The `ObjectStore` trait from [`08-deployability.md`](./08-deployability.md) maps almost line-for-line to a `service Store { ... }` proto, server-streaming makes `IterRefs` cheap, and `bytes` fields carry framed object content without base64 overhead. One proto file, one tidy story.
- **HTTP/JSON over HTTP/1.1 with mTLS** — for everything else (gateway↔ingest, gateway↔refs, gateway↔query). These are control-plane calls: small payloads, low QPS relative to the data plane, and operationally far easier to debug with `curl` and a saved-request tab. Conditional ref writes use HTTP `If-Match` headers — the exact semantics we want for CAS — so reinventing them in protobuf would lose more than it earns.

**External (gateway → client)**: the same HTTP/JSON surface from [`06-api-surface.md`](./06-api-surface.md). Clients see no change. Internal HTTP routes live on a different port than the gateway's public listener and use mTLS instead of bearer auth.

One proto file describes the inter-service RPC surface:

```
proto/store.proto    # ObjectStore as RPC: Put, Get, Has, IterRefs (server-streaming),
                     # plus PutChunk / GetChunk for the large-file path from 12-large-files.md
```

A second proto file, `proto/events.proto`, describes broker payloads (see [`16-event-streaming.md`](./16-event-streaming.md)). It's a separate concern — events are publish/subscribe, not request/response — but it shares the directory and the protoc invocation.

Internal HTTP routes are documented in each service's source as OpenAPI annotations; the chart in [`17-deployment-k8s.md`](./17-deployment-k8s.md) optionally bundles a Swagger UI per service in dev mode.

The client-facing query API (predicates, pagination, watch) lives in [`15-query-and-discovery.md`](./15-query-and-discovery.md) and is HTTP-only end-to-end.

Versioning: the proto includes a `string omp_api_version = 1` for forward-compat; the internal HTTP services include the same field as a top-level JSON key. Breaking changes bump the version and force a coordinated rollout — same discipline as the TOML schemas in the tree.

### Why not gRPC everywhere

"gRPC everywhere" was the first draft of this doc. The reasons it didn't survive: every internal HTTP call is curl-able by an operator with a workload cert, which is a real debugging affordance for a course project; ref CAS via HTTP `If-Match` is the same primitive S3 (since 2024) and GitHub's refs API already use, so we're not inventing a wire format we'd have to maintain; and one proto file is meaningfully less build-toolchain weight than three. The cost of the hybrid is two transport stacks instead of one — accepted because the data plane and the control plane have genuinely different shapes.

## One concrete flow — `POST /files`

This is the hardest path because it touches every service. Once this flow makes sense, the others (read paths, branch ops, dry-runs) fall out trivially.

```
1.  client  → gateway:  HTTPS POST /files  (multipart: path, file, fields)
                        Authorization: Bearer <token>
2.  gateway:            verify token; resolve tenant; rate-limit; sign tenant ctx
3.  gateway → ingest:   HTTP  POST /internal/files  (X-OMP-Tenant-Context header)
4.  ingest  → store:    gRPC  Store.Put(blob)        → blob_hash       [data plane]
5.  ingest:             load schema for file_type via Store.Get (cached)
6.  ingest:             run probes inside wasmtime; assemble manifest
7.  ingest  → store:    gRPC  Store.Put(manifest)    → manifest_hash   [data plane]
8.  ingest  → refs:     HTTP  POST /internal/stage   { path, manifest_hash, blob_hash }
9.  refs:               record in tenant's staging area; reply { stage_id }
10. ingest  → gateway:  HTTP  200 { manifest_hash, blob_hash, stage_id }
11. gateway → client:   HTTPS 200 { manifest, ... }
```

Steps 4 and 7 are gRPC because they ship object bytes; everything else is HTTP/JSON because it ships hashes and small JSON.

Then `POST /commit` walks `refs` only — it builds the new tree objects from the staged set, writes a commit, CAS-updates the branch ref. The ingest path doesn't see commit at all.

## Idempotency and ref CAS

Two failure modes deserve named handling. The rest is generic retry.

**`commit` idempotency.** A retried `POST /commit` on a flaky network must not produce two commits with different hashes. Clients pass an `Idempotency-Key` header (UUID); the gateway forwards it to `refs`, which keys the staged-set + resulting commit hash by `(tenant, idempotency_key)` for a 24-hour window. A retry returns the original commit. No key → no idempotency, just like Stripe.

**Ref CAS.** Every ref write is conditional on the previous hash, expressed as standard HTTP conditional headers on the internal route:

```
PUT  /internal/refs/refs/heads/main
If-Match: "<previous-hash>"        (or  If-None-Match: *  to create)
Content-Type: application/json
{ "new_hash": "<...>" }
```

Returns `200 { "new_hash": "..." }` on success or `412 Precondition Failed { "current_hash": "..." }` on conflict. This is the same shape S3 uses for conditional writes (GA 2024), GitHub uses for ref updates, and every CDN uses for cache validation — the wire format is the design, and we don't reinvent it.

Backends implement CAS differently underneath: the disk backend uses `flock` + read-modify-write under the lock; the Postgres backend uses a transaction with a `WHERE current_hash = $expected` predicate; the S3 backend forwards `If-Match` directly. The wire contract is the same regardless.

The gateway translates the internal `412` to the external `409 conflict` from [`06-api-surface.md`](./06-api-surface.md) so the public error vocabulary stays unchanged. v1's single-writer-per-tenant lock means CAS rarely contends; v2's multi-writer-with-merge needs CAS to be correct from day one (which is why we're spending the bytes on it now).

## Inter-service auth

mTLS between services for transport identity (each service has a workload cert from a small CA — cert-manager in K8s, or a one-off CA in `scripts/`). On top of that, the gateway issues a **signed tenant context** for each downstream call.

The context travels as a single header on both transports, so signature verification is one code path:

```
X-OMP-Tenant-Context: <base64(cbor(TenantContext))>
```

The CBOR payload is fixed:

| Field | Type | Notes |
|---|---|---|
| `tenant_id`  | string | Opaque tenant identifier. |
| `quotas_ref` | bytes  | Pointer into the registry, not full quotas. |
| `exp_unix`   | i64    | Short — request budget + 30s. |
| `signature`  | bytes  | Ed25519 over the other fields, signed with the gateway's key. |

Downstream services verify the signature and reject expired contexts. Two consequences worth naming:

- Compromise of one downstream service does not let it impersonate other tenants — it can only act for tenants whose contexts the gateway has already signed for in-flight requests.
- The token bearer model from [`11-multi-tenancy.md`](./11-multi-tenancy.md) terminates at the gateway. Bearer tokens never appear on the internal wire.

## What does *not* split

Some seams that look obvious are deliberately kept inside the existing services:

- **Schemas are not their own service.** They're stored in the object store like everything else; ingest and query read them on demand and cache them. A "schema service" would be a cache that pretended to be a backend.
- **WASM module store is not separate from the object store.** Probes are blobs in the tree (per [`05-probes.md`](./05-probes.md)). They're loaded by ingest from the object store like any other blob.
- **The probe sandbox is not its own service.** wasmtime runs in-process inside ingest. Splitting it out adds a marshalling boundary without buying isolation that the WASM sandbox doesn't already provide.
- **The tenant registry is part of the gateway**, not a separate service. It's a small, low-QPS lookup; making it a service is overhead that doesn't pay back.

If a future feature changes one of these answers, the change is contained — none of the existing services have to break.

## Relationship to the v1 monolith

The v1 monolith already separates `omp_core::api` from `omp-server` cleanly. The decomposition path is therefore mechanical:

1. Introduce `proto/store.proto`; generate Rust client + server stubs via `tonic-build`.
2. Wrap the existing disk-backed `ObjectStore` impl in a `tonic` server. Add a `tonic`-client wrapper that also implements `ObjectStore` so callers don't know which is in use.
3. Carve the existing axum app into per-service axum apps (gateway, ingest, refs, query) that share `omp_core::api`. The internal HTTP routes are new but thin — each is a few lines wrapping an `omp_core::api` call.
4. Containerize each role into its own image; deploy via the chart in [`17-deployment-k8s.md`](./17-deployment-k8s.md).

No `omp_core::api` function changes. Tests that target `omp_core::api` directly continue to pass because the in-process transport is still a supported deployment shape (still useful for `omp serve --monolith` on a laptop and for hermetic CI).

## What the split unlocks

These features are designed in their own docs but only become *coherent* once the services exist:

- **Event streaming** — see [`16-event-streaming.md`](./16-event-streaming.md). Refs and ingest are the natural producers; query and any external consumers subscribe.
- **Real K8s deployment with horizontal scale** — see [`17-deployment-k8s.md`](./17-deployment-k8s.md). Each service is its own Deployment with its own replica count and resource envelope.
- **Multiple ObjectStore backends in production** — once the object-store role is a service, its backend choice is a deployment decision, not a build-time decision.
- **Observability that distinguishes who's slow** — see [`18-observability.md`](./18-observability.md). Per-service latency histograms answer "is ingest slow or is the backend slow?"

## Fixed points this layer does *not* move

All five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md) hold across both wire formats:

1. **SHA-256 of canonical wire bytes** — every service that sees an object's bytes can re-hash them and verify. Neither transport strips framing.
2. **Git-style object framing** — the gRPC `bytes` field carries framed object content opaquely; HTTP-side hashes refer to the same canonical bytes. End-to-end preservation either way.
3. **`ObjectStore` as the storage contract** — `proto/store.proto` is its one and only wire projection.
4. **Closed field-source set + fallback** — assembly happens entirely inside ingest; the wire only sees finished manifests.
5. **WASM probe ABI** — unchanged; the sandbox is in-process to ingest.

The decomposition is additive. Reverting to the monolith is a single-binary build with the in-process transports re-selected. The bytes on disk are identical either way.

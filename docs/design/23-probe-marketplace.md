# 23 — Probe marketplace + per-folder probe layout

[`05-probes.md`](./05-probes.md) committed OMP to "probes are user-authored content," and [`20-server-side-probes.md`](./20-server-side-probes.md) wired up a build path so users could compile a probe inside the gateway. Both docs deliberately stopped at "user has a `.wasm` blob in their own tree." Neither addressed the next question: how does a useful probe get from one user's repo into someone else's?

This doc adds two things alongside the existing design:

1. **A per-folder on-disk layout for probes.** Each probe becomes its own directory, so a probe can ship companions (README, source for reproducibility, examples) without colliding with neighboring probes that happen to share a namespace.
2. **A separate `omp-marketplace` microservice** for publishing probes, browsing them, and one-click installing them into a tenant's repo. WorkOS identity (doc 22) is the publisher attribution. The marketplace itself is stateless about which tenants exist; it just stores and serves probe folders by content hash.

The layout cutover lands as code in this iteration. The marketplace is a design commitment with the API contract pinned, so the future implementation has a target; the actual `omp-marketplace` crate, frontend pages, and Helm wiring are deferred.

## Why this is a separate service

Three reasons the marketplace doesn't fit inside the gateway:

- **Different access pattern.** Gateway is per-tenant: every request resolves a tenant id and routes to that tenant's shard with a signed `TenantContext`. Marketplace is the opposite — a public read surface where every probe is visible to every authenticated user, plus a write surface where the publisher's identity is what matters, not the consumer's tenant. Stuffing a "no-tenant" code path into the gateway would special-case a corner of the routing logic that today is uniform.
- **Different storage shape.** Gateway-fronted shards hold per-tenant Git-shaped repos: tenant root → tree → manifests. The marketplace holds a *catalog table* (publisher, namespace, name, version, description, hashes, timestamps, download counts) plus a flat blob store for the wasm/manifest/readme bytes. Different schema, different indexes, different lifecycle (catalog rows can be yanked; tenant trees can't).
- **Different deploy cadence and blast radius.** A bad marketplace deploy should not be able to take down the data plane. Process isolation is the simplest way to guarantee that.

This mirrors [`14-microservice-decomposition.md`](./14-microservice-decomposition.md)'s rationale for splitting `omp-gateway` from `omp-server` and [`20-server-side-probes.md`](./20-server-side-probes.md)'s rationale for `omp-builder` as its own pod.

## What does *not* change

- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). The marketplace is strictly additive.
- The `TenantContext` envelope from [`14-microservice-decomposition.md`](./14-microservice-decomposition.md), with **one additive field** (`sub`, see below). Old verifiers ignore unknown CBOR fields, so existing shards keep working.
- The HTTP API contract from [`06-api-surface.md`](./06-api-surface.md) for tenant-scoped routes.
- Doc 22's WorkOS auth flow. The marketplace reuses `omp_session` cookies / Bearer tokens through the gateway exactly as every other authed endpoint.
- Doc 20's `/probes/build` flow. Building and publishing are separate steps; building stays unchanged.

If a marketplace requirement would force any of the above to change, the requirement is wrong, not the constraint.

## Per-probe folder layout (in this iteration, code)

[`05-probes.md`](./05-probes.md) and the existing implementation use a flat layout:

```
probes/<namespace>/<name>.wasm
probes/<namespace>/<name>.probe.toml
```

This works for the three universal `file.*` probes in the starter pack but doesn't scale. A serious probe wants companions — a README explaining the field semantics, the original source for reproducibility, example inputs, possibly multiple WASM variants. The flat layout has nowhere to put any of those without sharing a namespace directory with unrelated probes.

The new layout is one directory per probe:

```
probes/<namespace>/<name>/
├── probe.wasm                  # required: the compiled probe
├── probe.toml                  # required: manifest (name, returns, kwargs, limits)
├── README.md                   # optional: prose for marketplace + Build page
└── source/                     # optional: original Rust src for reproducibility
    └── lib.rs
```

Two things to notice:

- `probe.toml`, not `<name>.probe.toml`. The directory carries the name; repeating it in the filename is noise.
- The qualified registry name remains `<namespace>.<name>`. The on-disk reshuffle changes only path strings — every consumer that addresses a probe by its dotted name (schemas, audit log, manifest `[probe_hashes]`, the engine) is unaffected.

**Hard cutover, no back-compat.** The discovery walker in `crates/omp-core/src/api.rs::current_probes` stops accepting the flat layout entirely and emits a warning if it encounters one (so a half-migrated repo surfaces as visibly inert rather than silently broken). The only repo containing flat-layout probes today is the demo `_local` tenant, which we regenerate by deleting `tenants/_local/` and letting `init_tenant` re-seed.

The change touches a small number of files:

- **`crates/omp-core/src/probes/starter.rs`** — `tree_path_wasm`/`tree_path_manifest` return the new paths; new `tree_path_dir` helper.
- **`crates/omp-core/src/api.rs::current_probes`** — walker strips `/probe.wasm` suffix instead of `.wasm`; sibling manifest is at `…/probe.toml`.
- **`crates/omp-core/src/api.rs::current_probe_names`** — same walker change for the name-only fast path used by schema validation.
- **`crates/omp-core/src/api.rs::Repo::init_tenant`** — uses `tree_path_*`, picks up the new layout for free; `fs::create_dir_all` already creates the new deeper parent.
- **`crates/omp-builder/src/builder.rs`** — emits artifacts at `probes/<ns>/<name>/{probe.wasm,probe.toml,source/lib.rs}` instead of the flat shape.
- **Tests** — `crates/omp-core/tests/{end_to_end,dynamic_probes,reprobe}.rs` and `crates/omp-builder/tests/build_pipeline.rs` had hardcoded path strings; now updated.

Schemas (`schemas/<file_type>.schema`) are deliberately not folder-ified. A schema is a single artifact with no companions, so the per-folder argument doesn't apply. If schemas later grow companions (sample documents, prose docs), they should follow the same pattern; for now the asymmetry is intentional and called out so a future maintainer doesn't accidentally normalize it.

## Marketplace architecture (deferred implementation)

A new microservice, mirroring the structure of `omp-builder`:

```
crates/omp-marketplace/
├── Cargo.toml
├── src/
│   ├── main.rs           # bind, config, axum::serve
│   ├── lib.rs            # router + state assembly
│   ├── catalog.rs        # SQLite-backed catalog table
│   ├── blobs.rs          # blob put/get against an omp-store-client instance
│   └── routes.rs         # the HTTP handlers below
└── tests/
    └── publish_install.rs
```

The deployable runs at its own bind address. The gateway adds one path-prefix routing rule, exactly the same shape as `/probes/build` → `omp-builder` from doc 20:

```
^/marketplace/  → omp-marketplace
```

When `gateway.config.marketplace` is unset, the gateway returns `503 marketplace_unavailable` for any `/marketplace/*` request, mirroring the existing builder-unavailable pattern. Local-only OMP installs keep working.

### Endpoints

Public (no auth required):

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/marketplace/probes` | List/search published probes. Query params: `namespace`, `name`, `publisher_sub`, `q` (substring against description), `limit`, `cursor`. Returns paginated catalog rows. |
| `GET` | `/marketplace/probes/<id>` | Single catalog entry plus a parsed manifest preview. |
| `GET` | `/marketplace/probes/<id>/blobs/<hash>` | Download a specific blob (`probe.wasm`, `probe.toml`, `README.md`) by its content hash. |

Authenticated (gateway forwards a signed `TenantContext` carrying `sub`):

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/marketplace/probes` | Publish a probe folder. Body is a multipart form: `namespace`, `name`, `version`, `description`, `wasm` (file), `manifest` (file), `readme` (file, optional). Server stores blobs by sha256, writes a catalog row, returns the catalog `id`. |
| `DELETE` | `/marketplace/probes/<id>` | Yank a probe (publisher only). Catalog row is marked unpublished; **blobs are preserved forever** so historical replay against past `probe_hashes` keeps working (per `02-object-model.md` §74-82). |

Install is a *gateway* endpoint (not a marketplace one):

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/marketplace/install/<id>` | Gateway pulls the probe folder from the marketplace, stages each blob into the caller's tenant under `probes/<ns>/<name>/{probe.wasm,probe.toml,README.md}` via the same ingest path as `POST /files`, and returns the staged manifest hashes. The user clicks Commit on the existing Commit page to make it durable. |

Keeping install on the gateway means the marketplace stays stateless about consumer tenants. The marketplace knows publishers and probes; it does not know who installed what.

### Catalog schema

```sql
CREATE TABLE probes (
    id              TEXT PRIMARY KEY,         -- sha256(publisher_sub|namespace|name|version)
    publisher_sub   TEXT NOT NULL,            -- WorkOS user id
    namespace       TEXT NOT NULL,
    name            TEXT NOT NULL,
    version         TEXT NOT NULL,            -- free-text; semver enforcement deferred
    description     TEXT,
    wasm_hash       TEXT NOT NULL,            -- framed sha256 (per 02-object-model)
    manifest_hash   TEXT NOT NULL,
    readme_hash     TEXT,
    published_at    INTEGER NOT NULL,         -- unix seconds
    yanked_at       INTEGER,                  -- NULL if live
    downloads       INTEGER NOT NULL DEFAULT 0,
    UNIQUE(publisher_sub, namespace, name, version)
);
CREATE INDEX probes_namespace_name ON probes(namespace, name);
CREATE INDEX probes_publisher ON probes(publisher_sub);
```

SQLite is fine for the course-project scale and keeps the marketplace pod self-contained. Production would use Postgres; the schema is portable and the only sqlite-specific bit is the file-on-disk choice, which is configurable.

### Storage

Blobs (wasm/manifest/readme) live in a dedicated `omp-store` instance — the same gRPC blob service the shards use, on a separate logical deployment named `omp-store-marketplace`. Reusing the existing storage primitive avoids inventing a third object format. Blobs are content-addressed sha256 already; deduplication across publishers happens for free.

### Trust model

- **Identity = WorkOS `sub`.** The publish endpoint requires a valid `omp_session` cookie or a token-mode Bearer; the gateway forwards a signed `TenantContext` to the marketplace as it does for every other internal hop. The marketplace records `sub` from the envelope as `publisher_sub` and never trusts a client-provided identifier.
- **One additive `TenantContext` field.** `sub: Option<String>` is added to the CBOR envelope in `crates/omp-tenant-ctx/src/lib.rs`. Old shards (no marketplace knowledge) keep ignoring unknown fields and verifying signatures unchanged. The signing primitive is unchanged; only the canonical signed-body's struct definition gains an optional field.
- **No code-signing in v1.** Probes execute under wasmtime fuel + memory caps already (per [`05-probes.md`](./05-probes.md)), so a malicious probe is bounded inside the sandbox. Reputation systems and code signing land in v2 if there's a real abuse vector. Course-project scope doesn't need either.
- **Yanked probes still resolve.** A user who ingested a file when probe X was live has `probe_hashes["X"] = <hash>` in their manifest. Replay/audit code calls `marketplace.GET /probes/<id>/blobs/<hash>`. Yanked probes still answer that call (only the catalog row is hidden, not the blobs). This keeps historical reproducibility intact even after the publisher pulls the listing.

### Frontend (deferred to a follow-up)

Two new surfaces, contracts pinned now so the API is stable when the implementation lands:

- **`/ui/marketplace`** — browse + search + per-row "Install" button. Clicking install does `POST /marketplace/install/<id>` (gateway endpoint), then redirects to `/ui/commit` with the staged probe folder ready to commit.
- **"Publish to marketplace" button** on the existing `/ui/probes/build` page, shown after a successful build. Opens a small modal asking for `version` and optional `README.md`, then `POST /marketplace/probes`.

Both pages reuse the existing auth gate from doc 22 — no new auth surface.

## Build & deployment

**Helm.** New values block, mirroring the gateway/builder shape:

```yaml
marketplace:
  enabled: false
  replicas: 1
  port: 8082
  resources: { … }
  storage:                   # the dedicated omp-store-marketplace instance
    enabled: true
    size: 5Gi
  database:
    # SQLite file on a small PVC. Postgres support is a config swap later.
    pvcSize: 1Gi

gateway:
  marketplace: ""            # http://release-omp-marketplace:8082; unset → 503 on /marketplace/*
```

**CI.** New `cargo test -p omp-marketplace` step; the `publish_install.rs` integration test exercises the publish → list → fetch → install round trip against a stub gateway and an in-memory store.

**No new external secrets.** WorkOS lives only in the gateway; the marketplace trusts the signed `TenantContext` and never sees client cookies or tokens directly.

## Risks & deferrals

- **Spam / abuse.** Mitigation: WorkOS auth gate on publish, future per-publisher rate limiting, future content-moderation queue. Course-project demo doesn't need any of these. If someone publishes garbage, yanking is one DELETE away.
- **Versioning.** v1 stores `version` as a free-text tag. Semver enforcement, automatic bump on republish, and version-range install (`>=1.2,<2`) are deferred — the catalog schema accommodates them when added.
- **Search.** v1 supports exact-match `namespace`/`name` and substring on description. Real full-text search (sqlite FTS5 or Postgres tsvector) is a follow-up; the catalog row shape already has the columns.
- **Probe signing / supply-chain trust.** v1 relies on the wasmtime sandbox + WorkOS publisher identity. If the platform attracts real adoption, signing pre-publish (publisher signs the bundle hash with a key bound to their WorkOS identity) lands as a v2.
- **Multi-region / mirror.** Single-region only. Catalog blobs are content-addressed; future mirroring is a downstream concern, not a schema concern.

## Relationship to other docs

- [`02-object-model.md`](./02-object-model.md) — content-addressed framed-hash storage is the basis for the marketplace's blob model. The `[probe_hashes]` invariant in manifests is what makes yanking safe.
- [`05-probes.md`](./05-probes.md) — superseded for the on-disk layout (flat → per-folder); reaffirmed for everything else (manifest schema, sandbox limits, starter pack philosophy).
- [`14-microservice-decomposition.md`](./14-microservice-decomposition.md) — adds one service. Tenant-context envelope gains an optional `sub` field; downstream verifiers are unaffected.
- [`20-server-side-probes.md`](./20-server-side-probes.md) — preserved. Building stays inside `omp-builder`. Publish-to-marketplace is a downstream step the builder doesn't know about.
- [`22-workos-auth.md`](./22-workos-auth.md) — `sub` from the WorkOS session is the publisher identity. The marketplace runs entirely behind the existing WorkOS gate.

## What's deferred

- The `omp-marketplace` crate, its endpoints, its catalog table, its blob storage wiring.
- Frontend `/ui/marketplace` page and "Publish to marketplace" affordance on the Build page.
- Schema marketplace. Schemas have no companions today; the question of whether they need a folder-ified layout and a marketplace is reopened only if real usage forces it.
- Semver-aware publishing, signing, full-text search, rate limiting, content moderation.
- The `sub` field in `TenantContext`. Lands when `omp-marketplace` lands; until then no caller needs it.

## Implementation status

This iteration commits to:

- ✅ Per-probe folder layout — code lands now.
- ✅ Walker + init + builder + tests updated.
- ✅ `docs/design/05-probes.md` and `docs/design/20-server-side-probes.md` updated to reference the new layout.
- ⏸ `crates/omp-marketplace/` — entire crate, deferred.
- ⏸ Gateway path-prefix routing for `/marketplace/*` — deferred until the service exists.
- ⏸ Frontend marketplace page + Publish button — deferred.
- ⏸ Helm chart `marketplace.*` values — deferred.
- ⏸ `TenantContext.sub` field — deferred.

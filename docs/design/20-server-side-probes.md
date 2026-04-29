# 20 — Server-side probe compilation

The probe story today is asymmetric. Schemas and probes are designed to live in the tree as a property of the tenant — adding a file type is supposed to be a tree commit, not an OMP release ([`05-probes.md`](./05-probes.md)). But two gaps make that promise hollow for non-developers:

1. The engine never reads probes from the tree. It only loads the embedded starter pack baked into the binary at compile time via `include_bytes!` (`crates/omp-core/src/probes/starter.rs`). A `probes/foo.wasm` blob in the tree is inert.
2. Even if (1) were fixed, building a probe requires `cargo` and the `wasm32-unknown-unknown` toolchain on the user's machine. A tenant with a browser and a Rust idea has no path.

This doc designs a server-side build service that closes both gaps. A tenant POSTs Rust source through the web UI, a new `omp-builder` microservice compiles it in a sandboxed environment, and the resulting `.wasm` plus its source files are returned for the tenant to commit into their tree. From there, the engine — extended in the same change — picks them up at ingest. End to end: paste Rust, click build, click commit, files of that type now extract the new field.

The new feature falls naturally into the project's microservice decomposition from [`14-microservice-decomposition.md`](./14-microservice-decomposition.md). `omp-builder` is a sibling to `omp-server` and `omp-store`, not a layer in front of them.

## Goals

- Tenants submit Rust probe source through the web UI from [`19-web-frontend.md`](./19-web-frontend.md). The service compiles it server-side and returns artifacts ready to stage in the tree.
- The compiled `.wasm` runs in the same wasmtime sandbox as the starter pack ([`05-probes.md`](./05-probes.md)) — no new ABI, no new host imports, no relaxed limits.
- Per-tenant, per-repo. Tenants cannot see each other's source, build artifacts, or build logs. Same isolation guarantees as [`11-multi-tenancy.md`](./11-multi-tenancy.md).
- Reproducible. Re-building the same source on the same builder image yields the same `.wasm` bytes. The build is a function of the input, not the wall clock.
- Observable. The user sees the cargo build log as it streams (the SSE primitive from [`16-event-streaming.md`](./16-event-streaming.md) and [`19-web-frontend.md`](./19-web-frontend.md) was already wired into the gateway).

## Non-goals

- **Languages other than Rust.** AssemblyScript, TinyGo, and friends could target the same WASM ABI; deferring them keeps the toolchain image to one compiler in v1. Adding a second language means adding a second build path, not a fundamental redesign.
- **Schema authoring assistance.** A probe is only useful when a schema declares which fields it produces. v1 has no in-app schema editor — the user uploads a `.schema` TOML separately, the same way they would today. (Consistent with `19-web-frontend.md` §What's deferred.)
- **Persistent build queue across pod restarts.** Job state lives in `Arc<Mutex<HashMap<JobId, Job>>>`. A pod restart drops in-flight jobs; the UI surfaces this as "build expired, re-submit". Persistence is straightforward to add later.
- **Build caching across tenants.** Same-source-different-tenant rebuilds independently in v1. sccache + a shared `target/` could bring this down to milliseconds; it's a deploy-time optimization, not a v1 capability.
- **Arbitrary third-party Cargo dependencies.** A whitelist (`probe-common`, `ciborium`, `sha2`, `infer`) ships vendored. Anything else in `Cargo.toml` is rejected at build time. A user who needs another crate files a request to expand the whitelist; this is the security boundary, not an oversight.
- **In-memory probe hot-reload.** A new probe takes effect on the next ingest after commit, not before. The engine reads probes off the current tree at the start of each ingest (post-Phase-1).

## What does *not* change

- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). The probe ABI (host-imports refused, fuel + memory + wall-clock caps, CBOR input/output framing) is identical for compiled-on-the-server probes as it is for the starter pack.
- The schema TOML format from [`04-schemas.md`](./04-schemas.md). Schemas reference probes by qualified name (`<namespace>.<name>`); whether the probe was server-built or hand-built is invisible to the schema.
- The auth boundary from [`11-multi-tenancy.md`](./11-multi-tenancy.md). The builder reads `X-OMP-Tenant-Context` (signed by the gateway, see [`14-microservice-decomposition.md`](./14-microservice-decomposition.md)) to scope quotas and concurrency. No new bearer-token flow.
- The gateway as the sole external listener. `omp-builder` is internal-only, reached through the gateway's path-prefix routing for `/probes/build*`. Same TLS story, same rate-limit story.
- The tree-as-source-of-truth invariant. The user's Rust source files are committed alongside the `.wasm` output. Any historical commit can be rebuilt bit-for-bit from its source at that ref.

If the build feature would force any of the above to change, the feature is wrong, not the constraint.

## Architecture

```
   ┌───────────┐
   │  browser  │
   └─────┬─────┘
         │ HTTPS
   ┌─────▼─────────────────────────────────────────┐
   │                  gateway                      │
   │  /probes/build*       /  /ui/*    everything  │
   │  ─────────┐           │  ──────   else        │
   │           │           │  embedded             │
   └───────────┼───────────┴───────────┬───────────┘
               │                       │
        ┌──────▼──────┐         ┌──────▼──────┐
        │ omp-builder │         │ omp-server  │
        │  port 9100  │         │  shards     │
        └─────────────┘         └─────────────┘
            no egress                  │
            cargo + wasm                │
            target chain          ┌─────▼─────┐
                                  │ omp-store │
                                  │   gRPC    │
                                  └───────────┘
```

The gateway grows one more upstream choice: requests for `/probes/build*` route to `omp-builder`; everything else continues to the per-tenant shard exactly as today. The SSE branch already added to the proxy in `crates/omp-gateway/src/lib.rs` handles `/probes/build/{id}/log` without further changes.

`omp-builder` is the project's first stateful decomposition step. It holds in-memory job state for the duration of a build (and a 30-minute TTL after). Stateful in the sense of "remembers what it's working on", not "owns durable data" — the source of truth for any successful build is still the tenant's tree once the user commits.

## The build flow

A concrete walkthrough of one build, end to end:

```
1.  client → gateway:        POST /probes/build  (multipart)
                             X-OMP-Tenant-Context: <signed by gateway>
                             body: namespace, name, Cargo.toml, src/lib.rs,
                                   .probe.toml, optional extra files
2.  gateway → omp-builder:   forward, identical body
3.  builder:                 verify tenant ctx; check per-tenant rate
                             limit; allocate job_id; persist to in-memory
                             job table in state "queued"
4.  builder → client:        202  { "job_id": "..." }
5.  client → gateway:        GET  /probes/build/<id>/log    (SSE)
6.  gateway → omp-builder:   forward; SSE proxy fix from doc 19 streams
7.  builder:                 stamp scratch dir <state>/builds/<id>/ with
                             user files + skeleton (rust-toolchain.toml,
                             .cargo/config.toml, vendored registry)
                             validate Cargo.toml deps against whitelist
                             spawn `cargo build --release --offline`
                             with rlimit + wall-clock cap
                             pipe stdout+stderr to broadcast → SSE
8.  builder:                 cargo finishes ok → read
                             target/wasm32-unknown-unknown/release/<crate>.wasm,
                             pack source files + .wasm + .probe.toml into
                             the artifacts array, transition to "ok"
9.  client → gateway:        GET  /probes/build/<id>
10. builder → client:        200 { "state": "ok", "artifacts": [...] }
11. client (UI):             for each artifact, POST /files multipart
                             (path = probes/<ns>/<name>/...,
                              file = base64-decoded bytes) — uses the
                             existing single-shot upload flow
12. user clicks "commit":    standard /commit; the new probe is in the
                             tree at HEAD
13. next ingest:             engine's current_probes() — extended in
                             Phase 1 — finds the new probe in the tree
                             and registers it; schemas referencing
                             <ns>.<name> resolve to it; manifest fields
                             from the new probe land in the next
                             ingested file
```

Failure paths follow the same shape: the `state` transitions to `failed` instead of `ok`, the response includes `errors[]` parsed from rustc's `--error-format=short` output, and the user iterates on the source in the same UI form.

## The prerequisite — dynamic probe loading from the tree

This change is independent of the rest of the doc and shippable on its own. It's also load-bearing for everything else.

`current_probes()` in `crates/omp-core/src/api.rs:650-673` builds the probe registry that the ingest engine consults. Today it returns a HashMap keyed by `<namespace>.<name>` populated entirely from the embedded starter pack. The fix:

1. Walk the current `TreeView` for every blob under `probes/<ns>/<name>/probe.wasm` (per the per-probe folder layout from [`23-probe-marketplace.md`](./23-probe-marketplace.md)).
2. For each one, look for a colocated `probes/<ns>/<name>/probe.toml`. Parse `[limits]` (matching the format already used by the starter pack, so `crates/omp-core/src/probes/starter.rs` exposes a reusable parser).
3. Build a `ProbeBlob` (the same struct the starter pack populates) and insert it into the registry under `<ns>.<name>`.
4. If a starter probe and a tree probe share a name, the tree wins, and a `WARN` is logged. Tenants overriding `file.size` is a power-user move; the policy is "tree is source of truth", consistent with the rest of the design.

Schema validation already requires referenced probes to exist in the tree (`docs/design/04-schemas.md` §187). After this change, that check operates against the same registry the engine actually uses at ingest, which is what the design always meant.

This phase ships as its own PR. Even tenants who never touch `omp-builder` benefit: probes built locally with `cargo build --target wasm32-unknown-unknown --release` and uploaded via `POST /files` start working.

## Build determinism

Same-source, same-builder-image, same-`.wasm`. Three pieces:

1. **Pinned rustc.** `rust-toolchain.toml` at the repo root pins the toolchain version. The builder image inherits it. Upgrading the toolchain is a deliberate, documented event — same discipline as a wire-format bump.
2. **Pinned dependencies.** The whitelist is vendored, with a `Cargo.lock` checked in alongside. Crates.io can't move underneath us; the offline cargo build either uses the pinned versions or fails fast.
3. **Reproducibility flags.** Inherited verbatim from `probes-src/Cargo.toml`:

   ```toml
   [profile.release]
   opt-level = "s"
   lto = "thin"
   codegen-units = 1
   panic = "abort"
   strip = "symbols"
   ```

   `codegen-units = 1` removes the single largest source of nondeterminism in modern rustc.

The reproducibility property is observable — the test suite hashes the produced `.wasm` and compares it across two consecutive builds of the same source. A regression here is a release-blocker.

## Sandbox of the compiler

rustc is computation on user-supplied input. The wasmtime sandbox covers the *output*; the *input pipeline* needs its own perimeter.

- **Network**: the build container has zero outbound egress (Helm `NetworkPolicy` denies all). Cargo runs `--offline` against a pre-vendored registry. A user adding a non-whitelisted dep gets a clear error at validation time, before rustc starts.
- **Dependencies**: `whitelist.rs` lists the four crates above by name. `Cargo.toml` parsing rejects anything else with `compile_failed: disallowed_dep`. The whitelist is documented and surfaced via `GET /probes/build/whitelist`; expanding it is a code change with review.
- **CPU time**: hard `tokio::time::timeout` of 60s per build. The cargo subprocess is killed via SIGKILL on timeout; no graceful shutdown attempt (rustc doesn't deserve the courtesy).
- **Memory**: AS rlimit of 1 GiB on the cargo subprocess. rustc's allocator hits the wall and aborts cleanly; the job transitions to `failed` with an OOM marker.
- **Disk**: `<state>/builds/<job-id>/` is the scratch root. Periodic `du` during the build enforces a 200 MiB quota; exceed and the job is killed. After 30 min TTL (whether ok, failed, or cancelled) the directory is removed.
- **Source size**: 8 MiB per request, well under the gateway's 32 MiB body cap. The user has to actively try to exceed this (a single Rust file is rarely over a few KiB).

The compiler isn't trustworthy in the sense of "perfectly safe to run on any input" — it's trustworthy in the sense of "well-known failure modes, all of which we've capped". When the sandbox triggers, the job fails cleanly, the scratch dir is cleaned up, and the next request lands on a fresh slate.

## Resource caps

| Cap | Limit | Where enforced |
|---|---|---|
| Source upload size | 8 MiB | Builder request handler |
| Wall-clock per build | 60s | `tokio::time::timeout` around cargo subprocess |
| Memory per build | 1 GiB AS | `prlimit` on the spawned subprocess |
| Scratch disk per build | 200 MiB | Periodic `du` poll, kill on overrun |
| Concurrent builds per tenant | 1 | In-process `Semaphore` keyed on tenant_id |
| Builds per tenant per hour | 10 | In-memory token bucket |
| Pod-wide concurrent builds | tunable, default 4 | `Semaphore` on the builder state |
| Job retention TTL | 30 min | Background sweeper |
| Gateway request body | 32 MiB (existing) | Gateway, `crates/omp-gateway/src/lib.rs:175` |

`429 quota_exceeded` matches the existing tenant-quota error vocabulary from [`11-multi-tenancy.md`](./11-multi-tenancy.md).

## Why a separate `omp-builder` service

Considered alternatives:

- **Subprocess inside `omp-server`.** Rejected: rustc holds onto a gigabyte of memory mid-build. A runaway compile starves the file-read path on the same shard. The microservices project rubric also wants seams that follow load shape; "compile" and "serve" have wildly different shapes.
- **In-process compilation via a `cargo`-as-library API.** Rejected: there is no stable such API. `cargo-c`-style approaches exist but require unstable Cargo features.
- **Out-of-process job queue (Redis/Postgres-backed).** Deferred: correct for production scale, overkill for a course project. The in-memory job table is a known-temporary v1.

The chosen design — a separate `omp-builder` Deployment, gateway-routed by path prefix — is the smallest split that gives independent scaling, independent failure domains, and a clear story for the course's "extra credit" categories.

## Per-tenant isolation

Each build runs in its own scratch dir (`<state>/builds/<job-id>/`), with the job-id being a UUID. Different tenants' builds cannot share files, source, or `target/` artifacts. The cargo cache (`CARGO_HOME`) is shared across tenants but is read-only — the vendored registry — so there is no cross-tenant leak through it.

The job table indexes by `(tenant_id, job_id)` and rejects cross-tenant lookups. Even if a malicious client guesses a job_id from another tenant, the auth check on the SSE endpoint refuses to stream it.

In a multi-pod deployment, the gateway routes `/probes/build*` to one builder pod via a stable hash on tenant_id (the same scheme as `omp-server` shards in [`14-microservice-decomposition.md`](./14-microservice-decomposition.md)). This makes the per-tenant concurrency cap a property of the system, not of one pod.

## Risks and sharp edges

- **rustc image bloat.** ~400 MiB for the toolchain + vendored registry. The Helm chart documents the size; the deployment's `resources.requests.storage` accounts for it. A slim toolchain (`rustup component remove rust-docs rustfmt clippy`) trims maybe 100 MiB. Acceptable.
- **Cold cargo is slow.** Even with `--offline`, a cold compile is 20–40s on a modest pod. The async + SSE log model exists precisely for this. Sccache would mitigate; deferred.
- **Toolchain upgrades change every probe's hash.** rustc 1.94 → 1.95 produces different output bytes. Probes are content-addressed; a hash change ripples into manifests. Treat toolchain bumps as a coordinated event with a release note.
- **Whitelist is a maintenance ask.** Every additional vendored dep is a security review. Document the criteria for expansion in the design (small surface, no native deps, audited maintainers). Tempting to grow this; resist.
- **In-memory job state lost on restart.** Acceptable v1 trade-off. UI handles the case.
- **Pathological rustc input.** Rare but possible. The wall-clock cap is the safety net; the test suite exercises a known-pathological case.
- **Builder is a new attack surface.** Even with offline cargo and a whitelist, the compiler is processing untrusted source. If a future bug in rustc allows arbitrary code execution at compile time, the network policy + rlimit + SIGKILL chain is the defense in depth. Don't run the builder with privilege escalation enabled in the pod spec.

## Implementation status

Nothing implemented yet. This is the design step.

- ⏸ Phase 1: dynamic probe loading from the tree in `omp-core`.
- ⏸ Phase 2: `crates/omp-builder/` with the cargo-subprocess core + job table + SSE log + whitelist.
- ⏸ Phase 3: gateway path-prefix routing to the builder.
- ⏸ Phase 4: frontend `/probes/build` page with build form, live log, stage-artifacts step.
- ⏸ Phase 5: Dockerfile builder stage, Helm chart, CI builder-test job, smoke checks.

Phase 1 is independently shippable; Phases 2–5 wire up the build feature on top of it.

## What's deferred

- **Other compiler languages.** AssemblyScript is the obvious next target — small toolchain, browser-friendly tooling, can target the same WASM ABI. TinyGo a distant third.
- **Persistent build state.** A Redis-backed or Postgres-backed job queue would survive pod restarts and let multiple builders share work. Useful at scale, not for v1.
- **Build cache.** Sccache or a shared `target/` would make repeat builds near-instant. Deploy-time optimization.
- **In-app schema authoring.** Closing the loop end-to-end (compile probe → declare schema field → ingest file → see field) is mostly a UI sweep on top of `04-schemas.md`. Worthwhile, deferred.
- **In-app source editor.** The current plan uses textareas; a real editor (Monaco) would help. Cosmetic, deferred.
- **Multi-file Rust projects beyond `Cargo.toml + src/lib.rs`.** The build accepts `extra_files[<path>]` already, so the protocol supports it; the UI doesn't expose more than one source file in v1.
- **Probe versioning beyond hash.** Each tree commit is a snapshot; a probe at HEAD vs HEAD~5 is recoverable today. Semver-style versioning (`text.contains v2`) is not designed; arguable whether it's needed given content addressing.
- **Cross-tenant probe sharing.** A "marketplace" or registry where one tenant publishes and another adopts. Trivially implementable on top of object storage + signing, but well outside v1.

# 19 — Web frontend

OMP today is API-only. The CLI is the sole client; demos involve `curl`, jq, and a fair amount of squinting. This doc designs a web UI that exposes full read+write parity with the HTTP API from [`06-api-surface.md`](./06-api-surface.md), shipped as a static SvelteKit bundle embedded inside the gateway binary.

The UI is a thin client over the existing API. It does not introduce new endpoints, new auth modes, new object types, or a second deployment artifact. Every page is a projection of operations that already exist in `omp_core::api`. If the UI ever needs something the API can't answer, the API is wrong, not the UI.

This doc commits to a stack, a serving boundary, an auth flow, and a list of pages. Component-level structure is implementation detail.

## Goals

- A demoable web UI for the microservices-course grading rubric: tree browse, file inspect, upload, commit, branch, query, log, audit, and a live `/watch` feed — visible in one URL.
- Same minimalist aesthetic as `~/personal-website` (Astro, system fonts, single accent color, no shadows, monospace metadata). Lifted directly so the two surfaces look like they came from the same hand.
- One binary, one port. The gateway already terminates external traffic; the UI rides along on `/ui/*`. No CORS, no second origin in the Helm chart, no second TLS cert.
- Backend-only contributors can still build the gateway without Node installed (`cargo build -p omp-gateway --no-default-features`).

## Non-goals

- Replacing the CLI. The CLI remains the canonical write surface for scripted ingestion, schema iteration, and CI workflows. The UI is for humans inspecting state.
- Multi-user real-time collaboration. The single-writer-per-tenant invariant from [`11-multi-tenancy.md`](./11-multi-tenancy.md) holds; conflicting writes surface as `409 conflict` and the UI re-fetches.
- Dark mode, themable color systems, mobile-optimized layouts beyond responsive containers. Light mode only, light-mode forever, until a reason appears.
- An in-app schema editor. Editing schemas is a tree commit; do it from the CLI where the diff is visible.
- OAuth, SSO, magic-link login. Bearer tokens go in `Authorization`, exactly as today. The UI is a paste-the-token surface.
- Server-side rendering. The bundle is fully static; every page is a fetch from the browser. No SSR proxy, no Node at runtime.

## What does *not* change

- The HTTP API contract from [`06-api-surface.md`](./06-api-surface.md). The UI consumes existing endpoints and existing JSON shapes; no new routes are added for it.
- The auth boundary from [`11-multi-tenancy.md`](./11-multi-tenancy.md). One bearer token per tenant, validated by the gateway, no per-user identity in the UI.
- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). The UI is a client; clients can't move fixed points.
- The deployment story from [`17-deployment-k8s.md`](./17-deployment-k8s.md). Same gateway Service, same port, same `Deployment`. The image is heavier by one Node build stage; the runtime profile is unchanged.

If a UI requirement would force any of the above to change, the requirement is wrong, not the constraint.

## Architecture

The UI is a SvelteKit project under `frontend/`, configured with `@sveltejs/adapter-static` to produce a plain `build/` directory. That directory is embedded into the `omp-gateway` binary at compile time via the `rust-embed` macro and served at `/ui/*`. The existing API routes (`/files`, `/bytes`, `/tree`, `/query`, `/watch`, `/commit`, `/log`, `/branches`, `/audit`, `/uploads`, `/diff`, `/checkout`, `/status`, `/init`, `/healthz`) keep their root paths untouched.

```
   ┌───────────┐
   │  browser  │
   └─────┬─────┘
         │ HTTPS
   ┌─────▼──────────────────────────────────┐
   │             gateway                    │
   │  ┌──────────────┐   ┌────────────────┐ │
   │  │ GET /ui/*    │   │ everything     │ │
   │  │ embedded     │   │ else: existing │ │
   │  │ static asset │   │ proxy to shard │ │
   │  └──────────────┘   └────────┬───────┘ │
   └─────────────────────────────┬┴─────────┘
                                 │  HTTP/JSON, gRPC
                          ┌──────▼──────┐
                          │ omp-server  │  (sharded)
                          └─────────────┘
```

`/ui/*` is matched *before* the catch-all proxy fallback, so the UI handler wins for its prefix and every other request flows through `proxy()` exactly as today. `/` returns a 302 to `/ui/`. Unknown paths under `/ui/*` (deep links into client-side routes) fall back to the SvelteKit SPA fallback HTML so client routing can take over.

The bundle is hand-written; there is no codegen of TypeScript types from Rust. The `Manifest`, `FieldValue`, `TreeEntry`, `FileListing`, `CommitView`, `DiffEntry`, `BranchInfo`, and `AuditEntry` shapes from `omp-core` are mirrored manually in `frontend/src/lib/types.ts`. Codegen is on the deferred list; cost-benefit isn't there for ~8 stable shapes.

## Why embedded, not a separate origin

The first draft of this doc had the UI on Vercel (matching `~/personal-website`) calling the gateway over CORS. That didn't survive. Reasons:

- **One artifact** — the Helm chart already ships the gateway image. Adding a second deployable for a course-project demo is more moving parts than the demo justifies. Embedding makes the UI a property of the gateway, not a coordinated release.
- **No CORS** — same-origin `fetch` works without preflight, without `Access-Control-Allow-*` headers in the gateway, without an allow-list of UI origins to maintain. Bearer tokens travel in `Authorization` as today; SSE travels through the same connection.
- **No second TLS terminator** — whatever TLS story the gateway has (cert-manager in the chart, plain HTTP in dev) covers the UI for free.
- **Token-paste UX is awful across origins** — pasting a bearer token into a Vercel-hosted page that calls a different domain is a phishing-shaped experience. Same-origin makes it boring.

The trade-off is that the gateway's Docker image now requires Node at build time (one multi-stage layer added). Backend-only contributors can opt out via `--no-default-features`; the `embed-ui` Cargo feature is on by default but not load-bearing for the API.

For production at scale (not the v1 demo), the static bundle is trivially relocatable to a CDN: drop the `embed-ui` feature, add a CORS layer, change the deploy target. That's a swap, not a rewrite.

## Stack

SvelteKit with `@sveltejs/adapter-static`. Rationale, briefly: SSE, forms, file uploads, and dynamic routing are all first-class; the static adapter produces an output the `rust-embed` macro can consume directly; the bundle is small (no React-tree); the prose-and-monospace aesthetic from `~/personal-website` translates without a component library.

Astro was the obvious first choice (it's what the personal site uses) but the OMP UI is more *app* than *site* — query editor, live feed, multi-step uploads — and SvelteKit is closer to the right tool for that shape. The styling rules port cleanly either way; it's the same CSS variables and the same class names.

No Tailwind, no UI kit, no CSS-in-JS. Plain CSS in `app.css`, lifted from `~/personal-website/src/layouts/Base.astro`:

```css
:root {
  --bg: #fafafa;
  --bg-elevated: #ffffff;
  --fg: #111111;
  --fg-muted: #555555;
  --fg-soft: #888888;
  --border: rgba(17, 17, 17, 0.1);
  --accent: #2747d4;
  --max-width: 760px;
  --nav-height: 64px;
  --font-body: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
  --font-mono: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
```

Two container widths: `--max-width` (760px) for prose and forms, and a wide variant (~1200px) for tables — tree, log, audit, query results.

## API integration

The UI uses four interaction patterns, each mapped to existing API behavior:

- **Request/response JSON** — every path-and-tree read, every commit, every branch op, every query. A single `request<T>(method, path, opts)` helper in `$lib/api.ts` wraps `fetch`, attaches `Authorization`, parses `{error: {code, message, details}}`, throws a typed `ApiError`.
- **Multipart upload** — `POST /files` for objects under ~20 MiB. The gateway caps request bodies at 32 MiB (`crates/omp-gateway/src/lib.rs:175`); the UI routes anything bigger through the resumable path rather than bumping the cap.
- **Resumable upload session** — `POST /uploads` → chunked `PATCH /uploads/{id}?offset=` → `POST /uploads/{id}/commit`, exactly as designed in [`12-large-files.md`](./12-large-files.md). Cancel button hits `DELETE /uploads/{id}`. Chunk size matches the size returned by the create-session call.
- **SSE stream** — `GET /watch` for the live event feed. Implemented in the UI as a `fetch`-based reader (not `EventSource`), because `EventSource` cannot send custom headers and the UI needs `Authorization` on the request. Auto-reconnects with capped exponential backoff.

The UI does not call any internal route. Everything goes through the public gateway path, exactly as a CLI client would.

## Auth flow

The UI does not implement login. It implements *token possession*.

1. On layout mount, call `GET /healthz` to confirm reachability.
2. Call `GET /status` *without* an `Authorization` header (a probe).
3. `200` → the gateway is in `--no-auth` mode (single-tenant local dev). Set `authStore = { mode: 'no-auth' }` and never render token UI.
4. `401` → the gateway requires a bearer token. Read `localStorage.getItem('omp.token')`. If present, retry; if absent, render a blocking `<TokenPrompt>` modal that stores the token on submit and re-probes.
5. Every API call in `$lib/api.ts` attaches the stored token when the store is in `token-required` mode. A 401 mid-session clears the token and re-prompts.
6. The `/watch` SSE reader sends the same header naturally because it's `fetch`-streaming.

This is the same auth flow the CLI uses, expressed through a paste-in modal. There is no session, no cookie, no refresh token, no JWT issued by the UI.

## Pages

Every page is a thin `+page.svelte` that delegates to components in `$lib/`. The layout-level `<WatchFeed>` panel is present on every page (collapsible on narrow viewports).

- **`/`** — tree browser. Branch dropdown + `?at=` ref input. Folders are listed before files (alphabetical within each group), each row carries a folder/file SVG icon and a short hash. Current path is encoded as `?path=` so breadcrumbs and back-navigation deep-link cleanly. Click a folder row to descend; click a file row to open `/file/...`. Per-row "last commit" metadata is deferred — the design supports it but doing it client-side is N parallel `log()` calls per directory load and not worth the cost without a backend-enriched tree endpoint.
- **`/file/[...path]`** — file viewer in a GitLab-shaped layout: breadcrumb + thin metadata bar at the top, then the rendered file content as the visual hero, then collapsible `<details>` sections for `Manifest`, `Fields`, `History`, and `Danger zone`. Sections are closed by default and open-on-hash (`#fields` etc.) for deep linking. The hero renderer dispatches on the schema's render hint (see [`04-schemas.md`](./04-schemas.md#render-hints)): `text` → monospace with line numbers; `markdown` → sanitized HTML via `marked` + `dompurify`; `image` → `<img>` backed by an auth-aware `blob:` URL (because `<img src>` does not carry the `Authorization` header); `hex` → hex+ASCII dump; `binary` → download button only; `none` → metadata only. Fetched payloads honor `max_inline_bytes` from the render hint (default 64 KiB) and surface a "Showing first N · download full file" banner when truncated. Inline field editor (`PATCH /files/{path}`) lives inside the `Fields` section.
- **`/upload`** — drag-and-drop or file picker. Single-shot multipart for small files, automatic switch to resumable for large. Progress bar from `?offset=` accounting; cancel button.
- **`/commit`** — staged-diff viewer + commit form. After success, navigate to `/log`.
- **`/branches`** — list, create, checkout. Disable destructive ops while a request is in flight.
- **`/query`** (wide layout) — predicate editor with the grammar from [`15-query-and-discovery.md`](./15-query-and-discovery.md), prefix and `at` inputs, paginated result table via `cursor`. Recent queries persisted in `localStorage`.
- **`/log`** (wide layout) — commit timeline; click a commit to open its `/diff`.
- **`/audit`** (wide layout) — hash-chain log from [`18-observability.md`](./18-observability.md). Rows where `verified === false` are colored red; the chain breakage is the alert.
- **layout sidebar `<WatchFeed>`** — collapsible panel of incoming `commit.created` / `ref.updated` / `manifest.staged` / `quota.exceeded` events from `/watch`. Click an event to jump to its target.

That's it. Nine routes, each thin enough that the work is in `$lib/api.ts` and the components, not the pages.

## Build & deployment

Two new build-time concerns and zero new runtime concerns.

### Compile-time

`rust-embed` reads the `frontend/build/` directory at *compile* time. A fresh checkout running `cargo build` (default features include `embed-ui`) requires that directory to exist. Three escape hatches:

1. The new `crates/omp-gateway/build.rs` runs `npm ci && npm run build` in `../../frontend` if `frontend/build/index.html` is missing and `OMP_SKIP_UI_BUILD` is unset. This is the default path for a developer with Node installed.
2. `OMP_SKIP_UI_BUILD=1` skips the build script and trusts that the directory was pre-staged. This is what the Dockerfile sets after the Node stage runs.
3. `cargo build --no-default-features` drops the `embed-ui` feature entirely. The gateway compiles with no UI; the API still works. This is the path for backend-only contributors who don't want Node.

The `build.rs` failure message names all three, so a confused developer hitting this for the first time has a clear next step.

### Dockerfile

A new Node build stage feeds the Rust builder:

```dockerfile
FROM node:20-bookworm-slim AS web
WORKDIR /web
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build
```

The Rust builder copies the result and disables the build script:

```dockerfile
COPY --from=web /web/build /src/frontend/build
ENV OMP_SKIP_UI_BUILD=1
```

`cargo build --release` then proceeds with default features (`embed-ui` on). The final runtime image is unchanged — no Node, no `frontend/`.

### CI

`.github/workflows/ci.yml` adds `actions/setup-node@v4` (Node 20) and runs `npm ci && npm run build` in `frontend/` before `cargo build` in any job that exercises the gateway. An optional `web-check` job runs `npm run check` (svelte-check) for typing.

### Helm

No chart changes. The image now contains the UI; the existing `Service` on the gateway port serves both `/ui/*` and the API. `values.yaml` could grow a `gateway.uiEnabled` knob purely for documentation, but it's not load-bearing.

## Risks and pre-existing issues

### SSE buffering in the gateway proxy (must fix)

`crates/omp-gateway/src/lib.rs:201` reads the entire upstream response body before forwarding:

```rust
let resp_bytes = upstream_resp.bytes().await;
```

This is fine for JSON responses but kills SSE — `/watch` clients see no events until the upstream closes the stream, which is forever. The fix is to detect `text/event-stream` in the upstream `Content-Type` and forward via `Body::from_stream(upstream_resp.bytes_stream())` with `X-Accel-Buffering: no` and `Cache-Control: no-cache` added to the response. Buffer everything else as today (the 412→409 translation at line 215 stays exactly where it is). `reqwest` already has the `stream` feature on (`Cargo.toml:26`), so no dependency change.

This bug is not caused by the UI, but the UI is the first client that needs `/watch` to actually stream. The fix lands as part of this work.

### Compile-time directory dependency

Documented above. The risk is that someone clones the repo, runs `cargo build`, doesn't have Node, and gets a confusing error. The `build.rs` failure message and the `README.md` note mitigate it. A backend-only contributor's escape hatch is `--no-default-features`.

### Adapter-static SPA fallback filename

SvelteKit's static adapter emits a fallback HTML file (configurable; default `200.html`). The gateway's `/ui/*` handler must serve that file (not `index.html`) when an asset isn't found and the path has no extension. The exact filename is verified by the `embed-ui` build process and asserted in a `cargo test` so a future SvelteKit version change can't silently break SPA routing.

### Upload size

Gateway request-body cap is 32 MiB. The UI routes anything over ~20 MiB through the resumable path; the cap stays put. Bumping it would make small-multipart uploads more permissive, but the resumable path is the right tool for large files anyway and it's already implemented.

### Token storage

`localStorage` is the choice. It's not the most secure (an XSS in the UI exposes the token), but the alternatives — cookies need same-site policy and a server-set path; sessionStorage doesn't survive reloads — are worse for the demo shape. The XSS surface is a static SvelteKit bundle with no third-party scripts, so the attack vector is small. If E2E encryption from [`13-end-to-end-encryption.md`](./13-end-to-end-encryption.md) is added later, key material gets the same treatment with the same caveats.

## Implementation status

Nothing implemented yet. This is the design step.

Status checklist that this doc commits to:

- ⏸ `frontend/` SvelteKit project with `adapter-static` and the lifted personal-website CSS.
- ⏸ `$lib/api.ts`, `$lib/sse.ts`, `$lib/auth.ts`, `$lib/types.ts`.
- ⏸ Nine routes (`/`, `/file/[...path]`, `/upload`, `/commit`, `/branches`, `/query`, `/log`, `/audit`, layout `<WatchFeed>`).
- ⏸ `crates/omp-gateway` `embed-ui` Cargo feature, `build.rs`, `src/ui.rs`.
- ⏸ Gateway SSE streaming fix (independent of UI; landed alongside).
- ⏸ Multi-stage Dockerfile, `.dockerignore` updates, CI Node setup.

## What's deferred

- Codegen of TypeScript types from Rust (e.g. via `ts-rs` or schemars). Hand-typed for v1; revisit if the shape count grows.
- OAuth / SSO / magic-link login. Bearer tokens are sufficient for the v1 demo and for the multi-tenant story in [`11-multi-tenancy.md`](./11-multi-tenancy.md).
- Multi-user real-time collaboration (live cursors, presence). Out of scope; the writer model is single-writer-per-tenant.
- Dark mode. Add when there's a reason; for now, no.
- An in-app schema editor. Schema changes are tree commits; do them in the CLI where the diff is reviewable.
- Mobile-optimized layouts beyond the responsive containers in `app.css`.
- Deep-link sharing of query results (URL-encoding the predicate + `at`). Easy to add; not v1.
- A CDN-served variant of the bundle. The bundle is statically buildable already; switching from embedded to CDN is a deploy-time choice, not a code change of consequence.

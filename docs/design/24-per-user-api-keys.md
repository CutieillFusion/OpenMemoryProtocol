# 24 — Per-user API keys via WorkOS

[`22-workos-auth.md`](./22-workos-auth.md) added a browser auth path on top of the existing Bearer-token registry, then drew a hard line: **"Not WorkOS API Keys / M2M. WorkOS's machine-token product is a separate paid surface. Service accounts keep using OMP's TOML-registered Bearer tokens."**

That non-goal is now stale. WorkOS shipped an API Keys product (October 2025) that is included on the same AuthKit plan we are already using — no separate billing surface, no SDK adoption needed, and the entire management UI ships as an embeddable widget. The validation endpoint plugs into the existing Bearer path with one new branch.

This doc retracts that single non-goal. Everything else from doc 22 stands. Specifically: the `tenant = organization_id` mapping, the signed-cookie session model, the unchanged `TenantContext` envelope, the unchanged CLI, and the unchanged five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md).

## Why this is needed

Today, a user who wants to call the OMP API from a script has two options:

1. **Sign in via WorkOS in a browser**, copy `document.cookie` out of devtools, and paste the cookie into a `curl -H "Cookie: omp_session=…"`. The cookie is HttpOnly so this only works in the browser console. Not viable.
2. **Ask an operator** to add the SHA256 of a manually-generated token to `omp.toml` and deploy. Viable, doesn't scale, and isn't a path anyone outside the operator can take.

Every SaaS the user has used has a third option: a settings page that mints a key, shown once, copy-pasted into a `.env`. That option does not exist today. It should.

The course-project demo amplifies this: a grader who wants to verify the API works should be able to sign in, mint a key, and `curl` against the gateway in under a minute. Asking them to edit a TOML file in the operator's environment doesn't fit the demo loop.

## Why WorkOS API Keys (vs. rolling our own)

If we built this ourselves we would need: a `user_api_keys` table, prefix conventions, hashing (Argon2 vs. SHA256 debate), a "show once" UX pattern, last-used tracking, soft-delete on revoke, a management screen, and rotation flows. A small but real chunk of code that has nothing to do with what OMP is about.

WorkOS API Keys hands us all of that:

- **Storage, hashing, prefixes, rotation, last-used tracking** — owned by WorkOS.
- **Embeddable widget for create/list/revoke** — drop into a settings page; zero management UI to build.
- **Validation endpoint** — gateway POSTs the presented bearer; gets back `{ organization_id, permissions, … }` or null.
- **Already on AuthKit**, which we already use. No new vendor, no new contract, no new dashboard surface to administer.
- **Standards-shaped on the wire.** A WorkOS-issued key arrives as `Authorization: Bearer <opaque-string>` — exactly the same shape as today's static tokens. If WorkOS becomes the wrong choice later, we swap the server-side resolver, not any caller.

The cost is one new gateway code path, one new frontend route, and one new auth-flow endpoint to mint widget session tokens.

## Goals

- A signed-in user can mint, list, and revoke API keys for their org from the OMP frontend.
- Keys present as `Authorization: Bearer <key>` on every API call — same shape, same client code as the static-token path.
- Existing operator-provisioned tokens keep working unchanged. Both paths coexist; the resolver tries the local registry first, then falls back to WorkOS validation.
- The signed `TenantContext` envelope downstream services see is unchanged — `tenant`, `sub`, no end-user credential.
- WorkOS remains optional. With WorkOS config unset, the gateway behaves identically to the doc-22 design and the v1 Bearer-only design before it.

## Non-goals

- **Per-individual-user attribution within an org.** WorkOS API Keys are organization-scoped today. A key minted by user A is indistinguishable at validation time from one minted by user B in the same org. WorkOS has signaled user-scoped keys are on the roadmap; we adopt them when shipped. Until then, the audit log records `auth: "api-key"` with the WorkOS key id (not the secret) as the principal — coarser than `sub` from a session, but better than the current "the tenant did it."
- **Permission scoping inside a tenant.** Doc 22 deferred per-user roles inside a tenant. We keep deferring. The validation response includes a `permissions` array; we plumb it into `TenantContext` as an optional field for forward compatibility but do not branch on it. Enforcement is a future doc.
- **Replacing operator-provisioned Bearer tokens.** The TOML registry stays. Service accounts (`ci-prod`, `backfill-bot`) that have no human owner stay on the registry; that path is faster (no network call to WorkOS) and survives a WorkOS outage. WorkOS keys are the *user-facing* path.
- **WorkOS as the source of truth for non-WorkOS deployments.** Self-hosted single-tenant installs run without ever talking to WorkOS, exactly as in doc 22. The widget route 404s; the validation fallback is skipped; only the registry path exists.
- **A WorkOS SDK dependency.** Doc 22 chose hand-rolled OIDC over `workos-rust`. Same posture here: the validate call and widget-token mint are two `reqwest` POSTs. Vendor neutrality is preserved.

## What does *not* change

- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). API keys live above the object model.
- The `TenantContext` envelope from [`14-microservice-decomposition.md`](./14-microservice-decomposition.md). Downstream services see exactly what they see today.
- The HTTP API contract from [`06-api-surface.md`](./06-api-surface.md). One new auth-flow route on the gateway (`/auth/widget-token`); the data API is untouched.
- The CLI from [`11-multi-tenancy.md`](./11-multi-tenancy.md). Bearer tokens, TOML registry, SHA256 lookup. Untouched. A user who wants their CLI on a WorkOS-issued key drops it into `~/.omp/local.toml` exactly the way they would drop in any other Bearer.
- The session-cookie design from doc 22. API keys are a parallel path, not a replacement.
- The deployment story from [`17-deployment-k8s.md`](./17-deployment-k8s.md). No new env vars (`WORKOS_CLIENT_ID` / `WORKOS_CLIENT_SECRET` cover both validation and widget-token mint). No new secrets to bind.

If a WorkOS API Keys requirement would force any of the above to change, the requirement is wrong, not the constraint.

## Architecture

### Mint flow

```
   ┌───────────┐                                       ┌──────────┐
   │  browser  │                                       │  WorkOS  │
   └─────┬─────┘                                       └────┬─────┘
         │  GET /ui/settings/api-keys   (cookie)            │
         │ ──────────────────────────►                      │
         │  200 page (mounts widget)                        │
         │ ◄──────────────────────────                      │
         │  GET /auth/widget-token      (cookie)            │
         │ ──────────────────────────►                      │
         │              POST mint widget session            │
         │              ───────────────────────────────────►│
         │              { token, expires_at }               │
         │              ◄───────────────────────────────────│
         │  { token }                                       │
         │ ◄──────────────────────────                      │
         │  widget calls WorkOS directly with token         │
         │ ────────────────────────────────────────────────►│
         │              create / list / revoke              │
         │ ◄────────────────────────────────────────────────│
         │  user copies the new key once                    │
```

The widget talks directly to WorkOS once it has a session token. The gateway is not on that path — it does not see the key material at mint time, only at validation time.

### Validate flow

```
   ┌────────────┐                                      ┌──────────┐
   │  ext.client│                                      │  WorkOS  │
   └─────┬──────┘                                      └────┬─────┘
         │  GET /v1/...   Authorization: Bearer omp_…       │
         │ ──────────────────────────►                      │
         │              [resolve_tenant: cookie → miss]     │
         │              [resolve_bearer_token]              │
         │              [registry SHA256 lookup → miss]     │
         │              [check validation cache → miss]     │
         │              POST /api_keys/validate             │
         │              ───────────────────────────────────►│
         │              { organization_id, permissions }    │
         │              ◄───────────────────────────────────│
         │              [insert into cache, TTL 60s]        │
         │              [build TenantContext]               │
         │              [proxy to shard]                    │
         │  200                                             │
         │ ◄──────────────────────────                      │
```

The validation cache is keyed by `sha256(token)` so we never hold key material in memory between requests. TTL is the revocation SLA — accepted trade-off, see "Risks" below.

## Resolver order

Doc 22 set the order: cookie first, then Bearer registry. This change inserts WorkOS validation as a tail branch on the Bearer path, not a new top-level case:

```
1. Session cookie present and valid?            → tenant from cookie       (doc 22)
2. Bearer in registry (SHA256 lookup)?          → tenant from registry     (today)
3. Bearer in validation cache (sha256 key)?     → tenant from cache entry  (new)
4. Bearer validates against WorkOS?             → cache + tenant from resp (new)
5. Else                                         → 401
```

A WorkOS-issued key never appears in the static registry, so the registry lookup is wasted on it; that lookup is a hashmap probe, fast enough not to matter. The validation cache shields the WorkOS round-trip from the per-request hot path.

## Tenant identity mapping

Identical to doc 22 — `organization_id` is `tenant_id`, `user_<sub>` for unaffiliated users. WorkOS API Keys are scoped to organizations, so:

- Org-scoped key → `organization_id` from the validation response → `tenant_id`. Direct.
- Unaffiliated user (no org) → cannot mint a WorkOS API Key today (org-scoped product). Documented limitation; users in a personal org get keys, users in no org do not.

For a course-project demo this is fine; every demoable scenario already runs inside an org. If unaffiliated-user keys become necessary later, we either wait for WorkOS user-scoped keys or fall back to the registry path for those users specifically.

## Gateway changes

Concrete edits, anchored to the file at `crates/omp-gateway/src/lib.rs` as it stands today.

### `WorkOsConfig` (extend doc 22's struct)

No new top-level config block. The validation endpoint shares `client_id` / `client_secret` with the OIDC flow. One new optional knob:

```rust
#[serde(default = "default_validation_cache_ttl")]
pub validation_cache_ttl_seconds: u64,   // default 60
```

### `resolve_bearer_token` extension

The function today (lines 144–160) does a SHA256 lookup against the registered tokens. It grows two tail branches:

```rust
fn resolve_bearer_token(&self, headers: &HeaderMap) -> Option<String> {
    let token = extract_bearer(headers)?;
    let hash = sha256(&token);

    // 1. existing registry path
    if let Some(t) = self.registry.lookup(&hash) { return Some(t); }

    // 2. cache (skipped if WorkOS not configured)
    let workos = self.config.workos.as_ref()?;
    if let Some(t) = self.validation_cache.get(&hash) { return Some(t); }

    // 3. WorkOS validation
    let resp = workos_validate(&self.http, workos, &token).ok()?;
    let tenant = resp.organization_id?;
    self.validation_cache.insert(hash, tenant.clone(), workos.validation_cache_ttl_seconds);
    Some(tenant)
}
```

The cache is a `moka::sync::Cache<[u8; 32], CachedValidation>` with size cap (`10_000` entries) and the configured TTL. Keys are byte arrays of `sha256(token)`; the token itself never enters the map. `CachedValidation` carries `tenant`, `permissions`, `expires_at`.

`workos_validate` is a single `POST https://api.workos.com/user_management/api_keys/validate` with the bearer in the body — same `reqwest::Client` and HTTP-2 connection pool that `auth.rs` already uses for OIDC.

### New route: `/auth/widget-token`

Sits next to the doc-22 routes (`/auth/login`, `/auth/callback`, `/auth/refresh`, `/auth/logout`). Implementation in `crates/omp-gateway/src/auth.rs`:

```
GET /auth/widget-token
  Cookie: omp_session=<signed>
  → 200 { token: "...", expires_at: 1730000000 }
  → 401 if no session cookie or signature invalid
```

Mints a short-lived WorkOS widget session by calling the WorkOS widget-session endpoint with the user's `sub` and `organization_id` (read from the session cookie). The mint call uses `WORKOS_CLIENT_SECRET`; the response token goes straight to the frontend, where the widget consumes it.

The endpoint is gated identically to `/auth/me` — valid `omp_session` cookie required. No CSRF concern because nothing about the endpoint mutates state on our side.

### `TenantContext` field (forward-compatible, unenforced)

Add `permissions: Vec<String>` (default empty) to the `TenantContext` envelope in `crates/omp-tenant-ctx/src/lib.rs`. The session-cookie path leaves it empty. The WorkOS-key path fills it from the validation response. No service consumes it yet; this is purely so the field exists when a future doc defines per-tenant authorization.

The signed envelope's wire format already supports additive fields without breaking older verifiers, per doc 14.

## Frontend changes

### New route

`frontend/src/routes/settings/api-keys/+page.svelte`. The route is gated client-side on `auth_mode === 'session'` (doc 22 mode); the existing `<LoginGate>` redirects unauthenticated visitors through `/auth/login`.

The page does three things:

1. Fetches `/auth/widget-token` via a new `getWidgetToken()` in `frontend/src/lib/api.ts`.
2. Loads the WorkOS API Keys widget script (deferred, from WorkOS's CDN).
3. Mounts the widget into a `<div id="workos-api-keys-widget">` with the fetched token.

The widget renders inside our chrome but talks directly to WorkOS for create/list/revoke. We do not proxy those calls.

### Profile menu link

`frontend/src/lib/components/ProfileMenu.svelte` gains one entry: "API keys" → `/settings/api-keys`. The link is hidden when `auth_mode !== 'session'` (no widget without a WorkOS-authenticated user).

### Embedding posture

The WorkOS widget is shipped as a web component / framework-agnostic embed. SvelteKit's static export mounts it as a child of a Svelte component using `onMount` — no React island required. If WorkOS only ships a React-coupled widget at the time of implementation, we mount a small React tree just for that route via `react-dom/client`; the rest of the app stays React-free. This branch lives in the open-questions section because it depends on what WorkOS actually ships.

## CLI: still unchanged

A user who wants their CLI on a WorkOS-issued key:

1. Browser → `/settings/api-keys` → mint key → copy.
2. Paste into `~/.omp/local.toml` as the bearer for that profile.
3. CLI uses it exactly as it uses any other Bearer.

The CLI never talks to WorkOS. It does not know its bearer was issued by WorkOS rather than the registry. The gateway's validation flow handles the distinction transparently.

## Audit log

The audit log ([`18-observability.md`](./18-observability.md)) gains a third `auth` value:

- `auth: "session"` → `sub` from cookie. (doc 22)
- `auth: "bearer"` → registered label or first 8 hex of `sha256(token)`. (today)
- `auth: "api-key"` → WorkOS API key id from the validation response (e.g., `key_01H8R…`). The key id is not the key secret; it is safe to log.

This finally answers "which key was used?" rather than just "which tenant." User-level attribution arrives later when WorkOS ships user-scoped keys; the field is forward-compatible.

## Build & deployment

Nothing new. The validation call and widget-token mint share the OIDC `reqwest::Client`. No new env vars. No new Helm values. CI gains one integration test in `crates/omp-gateway/tests/proxy_routing.rs` that mocks the WorkOS validate endpoint via `wiremock` and asserts the resolver returns the expected tenant.

## Risks and open questions

This section is the *intentionally living* part of the doc. Update it as implementation surfaces real answers.

1. **Revocation latency = cache TTL.** A revoked key keeps working for up to `validation_cache_ttl_seconds` after the user clicks "Revoke" in the widget. Default 60s; trade-off between WorkOS round-trip cost and revocation freshness. Mitigations to consider: a WorkOS webhook on revoke that punches the cache (if WorkOS exposes one); shorter TTL in production. Document the SLA in the user-facing "Revoke" confirmation.

2. **WorkOS rate limits on the validate endpoint.** Unknown at design time. The 60s cache should keep the steady-state rate proportional to *unique active keys*, not request volume. Verify against WorkOS's published quota during implementation; if quotas are tight, raise the cache TTL or add a per-tenant token bucket on the validate path.

3. **Exact validate-endpoint shape.** The WorkOS docs page is the source of truth; treat the request/response struct sketches in this doc as illustrative, not authoritative. Pin the actual shapes when the implementation PR opens.

4. **Widget framework coupling.** If WorkOS ships only a React-coupled widget, mount a tiny React subtree on the `/settings/api-keys` route only. If they ship a web component, mount it directly in Svelte. Decide at implementation time; the design does not depend on either.

5. **Permissions strings.** We pass `permissions` through `TenantContext` but do not enforce. Open question: do we define them in WorkOS now (forward-compat) or wait until the enforcement doc lands? Default position: do not define them yet — defining unused permission strings invites cargo-culting and a later rename.

6. **Unaffiliated users cannot mint keys.** WorkOS API Keys are org-scoped. A user logged in via WorkOS but with no organization gets the `user_<sub>` tenant id (doc 22) and no path to mint keys. For now, document the gap; require demo users to live inside an org. If this becomes a real blocker, the fallback is to issue them a registry-path token via an admin command.

7. **Restart amnesia of the validation cache.** A gateway restart drops the cache; the next request for each in-flight key pays one WorkOS round-trip. Acceptable; the cache is opportunistic. No persistence needed.

8. **Auditability of mint events.** The widget mints keys directly against WorkOS, bypassing the gateway. We don't see the mint in our audit log. WorkOS provides its own audit trail; document that "who minted this key" is a question for the WorkOS dashboard, not the OMP audit log. If users complain, add a WorkOS webhook → OMP audit-stream bridge later.

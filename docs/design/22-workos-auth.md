# 22 — WorkOS user authentication

[`11-multi-tenancy.md`](./11-multi-tenancy.md) deferred user authentication: tokens are minted by an admin CLI command and handed out manually. [`19-web-frontend.md`](./19-web-frontend.md) restated the same position as a non-goal: "OAuth, SSO, magic-link login. Bearer tokens go in `Authorization`, exactly as today. The UI is a paste-the-token surface."

That position was right for the v1 demo and survives for machine clients. It does not survive for human clients. Pasting a bearer token into a login modal is a phishing-shaped UX and gives no path to social login, MFA, password reset, or enterprise SSO — all of which the multi-tenant story implies eventually.

This doc adds a second auth path **alongside** the Bearer-token path: WorkOS AuthKit for browser users, static Bearer tokens for everything else. It does not replace anything. It does not move any of the five fixed points. The UI's "paste a token" modal goes away; the CLI does not change.

## Why WorkOS

The job is "outsource the parts of auth we do not want to own": password storage, social-login OAuth dances, MFA enrollment, email verification, password reset, and the eventual SAML/OIDC plumbing for enterprise customers. WorkOS AuthKit covers all of those behind one OIDC endpoint and a hosted login page.

Three concrete reasons over the alternatives:

- **Free for this project's scale.** AuthKit is free up to 1M MAUs with no card on file until production. As a course project that lands at zero. Auth0 and Clerk both gate social login + MFA behind paid tiers at much lower MAU counts.
- **Forward path to enterprise SSO.** The same dashboard that signs up a Google-OAuth user signs up a customer's Okta tenant later. Enterprise SSO and Directory Sync are paid per-connection, but they are the same product surface — no migration when a customer asks for SAML.
- **Standards under the hood.** AuthKit is plain OIDC + PKCE. We integrate via the well-maintained `openidconnect` crate, not a vendor SDK. If WorkOS becomes the wrong choice later, the swap is to a different OIDC issuer URL and client id, not a rewrite.

The custom-domain feature ($99/mo) is unnecessary. `auth.workos.com` is a fine redirect target for an OSS course project.

## Goals

- A real login experience for browser users: email + password, Google, GitHub, magic link, MFA — without OMP implementing any of them.
- One natural mapping from external identity to OMP tenants: WorkOS `organization_id` *is* the OMP `tenant_id`.
- The CLI and every other machine client keep working unchanged. Bearer tokens go in `Authorization` exactly as today.
- WorkOS is optional. With WorkOS config unset, the gateway behaves identically to the v1/iteration-2 design.
- One artifact, one port, one origin. The gateway already terminates external traffic; OIDC callbacks land there. No second deployable, no CORS, no separate auth service.

## Non-goals

- **Not replacing Bearer tokens.** The CLI, CI jobs, and any non-browser caller keep using `Authorization: Bearer <token>` resolved through the existing tenant registry. The two paths coexist.
- **Not a WorkOS dependency for the OSS install.** A self-hosted single-tenant deployment runs without ever talking to WorkOS. The feature is opt-in via config.
- **Not a vendor SDK adoption.** We integrate via generic OIDC (`openidconnect` crate), not `workos-rust`. Switching to a different OIDC issuer is a config change, not a code change.
- **Not session-state in a database.** Sessions are stateless signed cookies, signed by the gateway's existing Ed25519 key from `omp-tenant-ctx`. No new persistence, no Redis.
- **Not user-level authorization inside a tenant.** A WorkOS user authenticated for org `acme` becomes "an authenticated client of tenant `acme`" — full single-writer-per-tenant access. Per-user roles inside a tenant are deferred (see below).
- **Not WorkOS API Keys / M2M.** WorkOS's machine-token product is a separate paid surface. Service accounts keep using OMP's TOML-registered Bearer tokens.

## What does *not* change

- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). Auth is above the object model.
- The signed `TenantContext` envelope from [`14-microservice-decomposition.md`](./14-microservice-decomposition.md). Downstream services see exactly what they see today: `X-OMP-Tenant-Context` issued by the gateway, no end-user credentials.
- The HTTP API contract from [`06-api-surface.md`](./06-api-surface.md). No new endpoints for the API; only three new auth-flow routes (`/auth/login`, `/auth/callback`, `/auth/logout`) on the gateway.
- The CLI's auth model from [`11-multi-tenancy.md`](./11-multi-tenancy.md). Bearer tokens, TOML registry, sha256 lookup. Untouched.
- The deployment story from [`17-deployment-k8s.md`](./17-deployment-k8s.md). Same gateway image, same port. Two new env vars (`WORKOS_CLIENT_ID`, `WORKOS_CLIENT_SECRET`) when the feature is on.

If a WorkOS requirement would force any of the above to change, the requirement is wrong, not the constraint.

## Architecture

```
   ┌───────────┐                                      ┌──────────┐
   │  browser  │                                      │  WorkOS  │
   └─────┬─────┘                                      └────┬─────┘
         │  GET /ui/  (no cookie)                          │
         │ ─────────────────────────►                      │
         │  302 /auth/login                                │
         │ ◄─────────────────────────                      │
         │  GET /auth/login                                │
         │ ─────────────────────────►                      │
         │  302 to WorkOS authorize URL  (PKCE)            │
         │ ───────────────────────────────────────────────►│
         │              hosted login UI                    │
         │ ◄───────────────────────────────────────────────│
         │  302 /auth/callback?code=…                      │
         │ ─────────────────────────►                      │
         │              code → tokens (PKCE verifier)      │
         │              ───────────────────────────────────►
         │              tokens + userinfo (org_id, sub)    │
         │              ◄───────────────────────────────────
         │  Set-Cookie: omp_session=<Ed25519-signed CBOR>  │
         │  302 /ui/                                       │
         │ ◄─────────────────────────                      │
         │  GET /files/...   Cookie: omp_session=…         │
         │ ─────────────────────────►                      │
         │              [resolve_tenant: cookie path]      │
         │              [proxy to shard with TenantContext]│
         │  200 JSON                                       │
         │ ◄─────────────────────────                      │
```

Two auth paths converge in `GatewayState::resolve_tenant()`:

```
1. If cookie `omp_session` present, signature valid, not expired
       → return tenant_id from cookie payload
2. Else if Authorization: Bearer <token> present and registered
       → return tenant_id from registry            (today's path)
3. Else 401
```

The cookie path is *first*. A browser sending both a cookie and a stale Bearer token gets resolved by cookie. Machine clients never set the cookie.

## Tenant identity mapping

WorkOS organizations exist for exactly the same reason OMP tenants do: a unit of isolation. We collapse the mapping to identity:

```
omp.tenant_id := workos.organization_id        (when the user belongs to an org)
omp.tenant_id := "user_" + workos.user_id      (when the user is unaffiliated)
```

This sidesteps the "registry of orgs" problem that [`11-multi-tenancy.md`](./11-multi-tenancy.md) left hand-wavy. New org logs in for the first time → new tenant id appears in `resolve_tenant`'s output → the rest of OMP behaves as it always did when a new tenant id shows up. No provisioning RPC, no admin tool to coordinate.

The unaffiliated-user fallback prefix (`user_…`) is reserved so that `organization_id` and `user_id` namespaces cannot collide, in case WorkOS's id formats ever overlap.

**First-touch tenant provisioning.** When a tenant id appears that has no shard assignment or quota record, the gateway:

1. Issues a `tenant.created` event ([`16-event-streaming.md`](./16-event-streaming.md)) with the WorkOS metadata as event details.
2. Selects a shard via the existing consistent-hash routing (no special case).
3. Applies the default quota tier from gateway config.

This is a write-on-first-use pattern, not a separate signup endpoint. It works because OMP's per-tenant state is lazily-initialized on the shard already — the shard creates `/tenants/<id>/` on first write.

## Sessions: signed cookies, not server state

The session cookie is an Ed25519-signed CBOR envelope, signed by the same `GatewaySigner` (`crates/omp-tenant-ctx/src/lib.rs`) that already signs `X-OMP-Tenant-Context`. We are not adding a JWT library. We are reusing the envelope format we built for inter-service trust, with a different payload type:

```rust
// crates/omp-gateway/src/auth.rs (new)
struct SessionClaims {
    tenant: String,            // org_id or "user_<workos_user_id>"
    sub: String,               // workos user id
    workos_session_id: String, // returned by the token exchange; needed for refresh
    issued_at: u64,            // unix seconds
    expires_at: u64,           // unix seconds; default issued_at + 3600
}
```

`workos_session_id` is opaque to OMP's parsing (~30 bytes) and lives in the cookie so refresh stays stateless on our side. The cookie is already signed and HttpOnly; adding it does not weaken either property.

The cookie deliberately does **not** carry `email`. Cookies travel on every request and end up in proxy access logs and any header-capturing telemetry; putting PII there means scrubbing discipline at every hop. The audit UI's "show me the email behind this `sub`" path looks the user up via the WorkOS userinfo endpoint on demand, cached briefly in memory by the gateway. `sub` (the WorkOS user id) is the stable identifier the audit log persists; email is presentation-only.

**Vendor coupling, called out.** `workos_session_id` is opaque to OMP's *parsing*, but not to OMP's *integration*: the refresh handler calls `POST /sessions/{workos_session_id}/authenticate`, which is WorkOS-shaped. Swapping to a different OIDC issuer (Auth0, Keycloak, Okta) keeps the rest of this design generic but changes the refresh strategy — the typical replacement is `prompt=none` re-authorization at `/auth/refresh` rather than a session-id endpoint call. The "swap issuer by config" story therefore has one vendor-specific branch in `auth.rs::refresh`. Acceptable; flagged so it isn't a surprise.

The encoded envelope goes into `Set-Cookie: omp_session=<base64>; HttpOnly; Secure; SameSite=Lax; Path=/`. The cookie is verified on every request by `resolve_tenant` before the proxy fallback runs.

Properties this gives us:

- **Stateless.** No session table, no Redis, no replication concern.
- **Self-revoking on rollover.** When the gateway's signing key rotates (an existing operational concern from doc 14), every session cookie becomes invalid. Forced re-login is the price of key rotation.
- **Short TTL by default.** 1 hour. A stolen cookie is valid until expiry; mitigate with HSTS, Secure, HttpOnly, and a short TTL rather than with revocation lists.

A refresh path (`GET /auth/refresh?return_to=...`) re-validates the WorkOS session by calling `POST /sessions/{workos_session_id}/authenticate` (id read from the expired cookie, which is still signature-valid — we accept "expired but well-formed" cookies on this single endpoint), and re-issues a fresh OMP cookie before redirecting back to `return_to`.

The "expired but signature-valid" allowance is bounded: `/auth/refresh` rejects cookies whose `expires_at` is older than `now - refresh_grace_seconds` (default: 86400, i.e., 24 hours). Without this bound, a cookie lifted from a six-month-old backup or an abandoned browser profile is a valid refresh oracle as long as the signing key hasn't rotated. The grace window is configurable per deployment; a tighter window (e.g., one hour) is appropriate for production, and a looser one for laptops that sleep over weekends.

This refresh is **not** transparent to a `fetch()` call. A browser `fetch` cannot follow a 302→WorkOS→302 chain on its own, and `/auth/refresh` may need to bounce through `api.workos.com` if WorkOS's own session has rolled. The frontend handles a 401 from any API call by storing the in-flight URL and doing `window.location = '/auth/refresh?return_to=' + encodeURIComponent(currentPath)` — a top-level navigation. Open SSE streams on `/watch` are dropped and re-established after the redirect. The UX is "page flickers, you're back where you were" rather than "API call retries silently"; the latter is not achievable without a service worker, which is not worth it for v1.

We deliberately do **not** store the WorkOS access token or refresh token in the cookie. The cookie carries OMP-internal claims plus the opaque `workos_session_id`; the access token stays in WorkOS's domain and is re-issued on each refresh. This keeps the cookie small and our tenant model independent of WorkOS's token lifetime.

## Gateway changes

Concrete edits, anchored to the file at `crates/omp-gateway/src/lib.rs` as it stands today.

### `GatewayConfig` (lines 38–55)

Add an optional WorkOS section:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WorkOsConfig {
    pub client_id: String,
    pub client_secret: String,           // also accepted via env var
    pub issuer_url: String,              // e.g. https://api.workos.com
    pub redirect_uri: String,            // https://gw.example.com/auth/callback
    #[serde(default = "default_session_ttl")]
    pub session_ttl_seconds: u64,
}
```

`GatewayConfig` grows `pub workos: Option<WorkOsConfig>`. When `None`, the gateway behaves exactly as today.

### `resolve_tenant` (lines 85–101)

Reordered to check the cookie first, with the existing Bearer path as the second branch. Implementation lives in `crates/omp-gateway/src/auth.rs` (new); the call site in `lib.rs` collapses to:

```rust
pub fn resolve_tenant(&self, headers: &HeaderMap) -> Option<String> {
    if let Some(t) = self.resolve_session_cookie(headers) { return Some(t); }
    self.resolve_bearer_token(headers)            // existing logic, unchanged
}
```

`resolve_bearer_token` is the current body of `resolve_tenant`, lifted verbatim.

### `router` (lines 117–128)

Four new routes mounted **before** the proxy fallback, **after** the embedded UI routes:

```rust
let r = r
    .route("/auth/login",    get(auth::login))
    .route("/auth/callback", get(auth::callback))
    .route("/auth/refresh",  get(auth::refresh))
    .route("/auth/logout",   get(auth::logout).post(auth::logout));
```

`/auth/logout` accepts both methods deliberately. The frontend invokes it as a **top-level navigation** (a `<form method="POST" action="/auth/logout">` on the sign-out button, or `window.location = '/auth/logout'` for the GET path) so the browser can follow the 302 chain through `end_session_endpoint` and back. A `fetch('POST /auth/logout')` would clear the OMP cookie but never reach WorkOS's logout endpoint, leaving the WorkOS session live — the same trap as the refresh case.

Mounted only when `state.config.workos.is_some()`; otherwise these routes return 404 and the UI's login button is hidden (see frontend section).

The auth mode (`workos`, `token`, `no-auth`) is folded into the existing `/status` response rather than added as a second probe — the frontend already calls `/status` on mount per [`19-web-frontend.md`](./19-web-frontend.md). New `auth_mode` field on the `/status` JSON; old clients ignore it.

### New module: `crates/omp-gateway/src/auth.rs`

Owns the OIDC client, PKCE state, callback exchange, and cookie minting. The `openidconnect` crate's `CoreClient` is constructed once at startup from the WorkOS issuer URL (discovery); per-request state (PKCE verifier, nonce, return URL) lives in a short-lived signed state cookie set on `/auth/login` and consumed on `/auth/callback`.

State cookie payload:

```rust
struct OidcState { id: String, pkce_verifier: String, nonce: String, return_to: String, expires_at: u64 }
```

Same signing primitive as the session cookie. TTL: 5 minutes. Single use.

**Concurrent login tabs.** A single fixed cookie name at `Path=/` would let tab B's `/auth/login` overwrite tab A's PKCE verifier, breaking A's callback. Instead, `/auth/login` mints a random `id` (16 bytes, base64url), names the cookie `omp_oidc_state_<id>`, and embeds the same `id` in the OIDC `state` query parameter sent to WorkOS. `/auth/callback` reads `state` from the query string, looks up the matching cookie by name, and verifies the cookie's signed `id` payload equals the `state` value.

This binding does double duty: it is **both** the tab disambiguator **and** the standard OIDC CSRF defense. The callback rejects when (a) no cookie matches the `state`-derived name, (b) the cookie's signature doesn't verify, or (c) the cookie's signed `id` field doesn't equal the `state` query parameter. Any of those means the callback wasn't initiated by this browser's earlier `/auth/login`. Tabs no longer collide; the gateway holds zero per-flow server state; CSRF on the callback is closed.

## Frontend changes

The UI today (`frontend/src/lib/auth.ts`, ~67 lines) probes `/status` and either runs in `no-auth` mode or prompts for a token. We add a third mode:

```ts
export type AuthMode = 'unknown' | 'no-auth' | 'token-required' | 'session';
```

`probeAuth()` gains one preliminary check: if `document.cookie` contains `omp_session`, set mode to `'session'` and skip the `/status` probe. Otherwise it behaves as today.

`probeAuth()` reads `auth_mode` off the existing `/status` response (no second probe) to decide what gate to render. `<TokenPrompt>` becomes `<LoginGate>`:

- `auth_mode === 'workos'` → render a single button: "Sign in with WorkOS" → `window.location = '/auth/login?return_to=' + encodeURIComponent(currentPath)`.
- `auth_mode === 'token'` → existing token-paste modal (preserved verbatim for self-hosted single-tenant deployments).
- `auth_mode === 'no-auth'` → no gate, exactly as today.

API calls in `$lib/api.ts` change in one place: when `mode === 'session'`, do not attach `Authorization`. The browser sends the cookie automatically. SSE on `/watch` works the same way — `fetch`-streaming carries cookies natively.

A 401 from any API call in `'session'` mode triggers `window.location = '/auth/refresh?return_to=' + encodeURIComponent(location.pathname + location.search)`. The redirect is top-level because a `fetch`-driven retry can't follow the OIDC chain. SSE consumers re-subscribe on page load.

The static-export adapter is unchanged. Callbacks happen on the Rust side; SvelteKit never sees them. There is no need to switch to a Node adapter.

## CLI: unchanged

The CLI uses in-process transport for local dev and Bearer tokens against a remote gateway. Neither changes. A user who wants their CLI to use their WorkOS identity goes through `omp admin token create` (existing path) and pastes the static token into `~/.omp/local.toml`. WorkOS-issued machine tokens are out of scope; OMP's own token registry remains the M2M surface.

The audit log ([`18-observability.md`](./18-observability.md)) carries the auth path that produced the request and the principal:

- `auth: "session"` → `sub` is the WorkOS user id from the cookie.
- `auth: "bearer"` → `sub` is the token's registered label from the registry (e.g., `"ci-prod"`, `"backfill-bot"`), or the first 8 hex chars of `sha256(token)` if no label was registered. Never null, never the token itself.

Tenants can finally answer "which user committed this?" instead of just "the tenant did."

## Build & deployment

No new build-time concerns. `openidconnect` and `cookie` are pure Rust crates; the `openidconnect` discovery call happens once at startup, not at compile time.

Helm:

- Two new optional values: `gateway.workos.clientId`, `gateway.workos.clientSecret` (the secret bound through `secretKeyRef` to a Kubernetes `Secret`). When unset, the gateway runs in legacy Bearer-only mode.
- One new env var on the gateway pod: `WORKOS_CLIENT_SECRET`.

CI: a new test target hits `/auth/login` with WorkOS config pointing at a stub OIDC server (the `openidconnect` crate ships test helpers). End-to-end against real WorkOS happens manually in a staging environment, not in CI.

## Risks and trade-offs

### Stolen-cookie window and revocation

Stateless cookies cannot be revoked by their content alone. A 1-hour TTL is the baseline mitigation; HSTS + Secure + HttpOnly + SameSite=Lax close the obvious vectors. Two escalation paths sit on top:

1. **Per-user revocation** via an in-memory denylist of `(sub, until_epoch)` tuples in the gateway. `resolve_session_cookie` rejects any cookie whose `sub` appears with `until_epoch >= cookie.issued_at`. Entries auto-expire when `until_epoch < now - max_session_ttl` and are dropped. The denylist is bounded by `active_users × TTL` — small enough to fit in memory for any realistic tenant population, and not "session state" because it stores *anti-sessions*, not sessions.

   **Restart amnesia and how to live with it.** A restarted gateway forgets the denylist; a revoked user gets up to one more TTL of access until the entry would have been re-applied. Fine for routine offboarding, bad for active-compromise response. Two mitigations layer on top:
   - **Audit every denylist insertion** as a `session.revoked` event in the audit stream ([`18-observability.md`](./18-observability.md)). After an unplanned restart, the gateway replays unexpired entries from the audit stream on startup, reconstructing the denylist without persistent storage of its own.
   - **A `deny_floor_epoch` config knob.** Cookies whose `issued_at < deny_floor_epoch` are rejected wholesale. Bumping it to `now()` is "log out everyone with a cookie older than this moment" — the per-tenant equivalent of key rotation, with no effect on cookies issued after the bump. The right tool for "we just learned of a compromise; bounce everyone, then triage individuals."

   v1 lands the denylist; the audit-replay reconstruction and `deny_floor_epoch` ship together with the production-readiness pass. The course-project demo runs without either and relies on TTL alone.

2. **Mass revocation via key rotation.** Rotating the gateway's Ed25519 signing key invalidates every cookie issued under the old key. This is the right hammer for "we suspect signing-key compromise" or "wipe all sessions before a security audit," not for "fire one employee." It logs out every user across every tenant; flag it as an incident-response tool, not an offboarding tool.

For a course-project demo, neither path is wired up; the TTL alone is enough. For anything resembling production multi-tenant use, path (1) lands before launch.

### WorkOS as an outage dependency

If `api.workos.com` is down, no new browser sessions can be established. **Existing sessions keep working** until their TTL expires, because the cookie is self-validating. Bearer-token clients are unaffected. The blast radius of a WorkOS outage is "no new logins for up to TTL minutes," not "all auth fails." Acceptable; a hard requirement on a third-party identity provider for browser auth is the price of not building it ourselves.

### Org admins vs. org members — and what this means for B2B

WorkOS exposes `role` claims (admin/member) in its userinfo. v1 of this design ignores them: **every authenticated member of WorkOS org `acme` is a fully-privileged client of OMP tenant `acme`**. They can write, commit, delete refs, change schemas, and burn quota. The single-writer-per-tenant invariant from doc 11 is unaffected; concurrent writes serialize through the same per-tenant lock.

This is a deliberate deferral, but the consequence deserves to be loud:

> ⚠ **Any employee of an organization can corrupt that organization's tenant data.** Suitable for a course-project demo, single-team internal use, or trusted-collaborator setups. **Not** suitable for B2B GA, regulated workloads, or any deployment where org members shouldn't fully trust each other. Per-user roles inside a tenant must land before that.

Per-user roles within a tenant — read-only members, branch-restricted writers, commit approvers — are listed under deferrals.

### Multi-org users

A WorkOS user can belong to multiple organizations. AuthKit's authorization flow returns one `organization_id` corresponding to the org the user picked at the WorkOS-hosted "choose organization" prompt during login. v1 of this design **pins the user to that org for the lifetime of the session cookie**: switching organizations requires logout-then-login, which re-runs the OIDC flow and lets the user pick a different org.

The cookie's `tenant` field is set once at issuance from the `organization_id` claim and never reinterpreted. We deliberately do **not** offer a same-session "switch org" mechanism — that path needs a careful UI for "you have unsaved staged data in tenant A, switching to B will discard it" and isn't worth building until a multi-org user complains. The simpler model also makes the audit log unambiguous: every entry's `tenant` is exactly the org the user authenticated against, with no mid-session rebinding.

An org switcher in the UI is a deferred item; when it lands, it's "logout + login with `organization_id` hint" not a same-session swap.

### Cookie origin in dev

When the SvelteKit dev server runs on port 5173 and the gateway on 8080, the OIDC callback lands on 8080 and the cookie does not reach 5173. Two supported workflows:

1. **Embedded mode** (canonical) — `cargo run -p omp-gateway` with `embed-ui` on. UI and gateway share an origin; cookies just work. Use this for any task involving auth.
2. **Vite dev with proxy** — add to `frontend/vite.config.ts`:
   ```ts
   // Keep this regex in sync with the gateway router; anything not matched here
   // falls through to vite's static handler and never reaches the gateway.
   server: {
     proxy: {
       '^/(auth|status|healthz|files|bytes|tree|query|watch|commit|log|branches|audit|uploads|diff|checkout|init|probes)(/|$)':
         { target: 'http://localhost:8080', changeOrigin: true, ws: true },
     },
   }
   ```
   Now `vite dev` on 5173 is a same-origin proxy in front of the gateway; cookies set by `/auth/callback` are scoped to 5173 and the OIDC flow completes. SSE on `/watch` and any future routes are covered by the regex without further edits — use this for fast UI iteration when you also need auth.

The token-paste fallback (`auth_mode === 'token'`) is the third option for environments that can't reach WorkOS at all.

### Logout must terminate the WorkOS session, not just the OMP cookie

Clearing only the `omp_session` cookie on `/auth/logout` is half-done: the user clicks "Sign out," clicks "Sign in," and is silently signed back in because `api.workos.com` still holds its own session. That's a surprising sign-out and a real security hole on shared devices.

`/auth/logout` performs RP-initiated logout per OIDC:

1. Clear the `omp_session` cookie (`Set-Cookie: omp_session=; Max-Age=0`).
2. Redirect the browser to WorkOS's `end_session_endpoint` (discovered from the issuer metadata) with the appropriate `id_token_hint` and a `post_logout_redirect_uri` pointing back at `/ui/`.
3. WorkOS terminates its own session and redirects the user back to the OMP UI, now fully signed out.

The id-token-hint requires keeping the OIDC id token long enough to use it — we hold it in memory for the duration of the request handler (it arrives in the same response as the access token during the callback exchange) but do not persist it. If the id-token-hint isn't available (e.g., long-running session where we discarded it), fall back to the `client_id` parameter, which WorkOS accepts.

### Email is metadata, not identity

Email never appears in the cookie or the audit log. The *identity* used for tenant mapping is `organization_id`; the user identity persisted in audit entries is the WorkOS `user_id` (`sub`). A user changing email addresses in WorkOS does not change their OMP tenant or break their audit trail. The audit UI looks up `sub → email` via the WorkOS userinfo endpoint on demand, with a short in-memory cache, when an operator wants to see human-readable names. This avoids both the "I changed my email and now I can't log in" class of bugs and the "PII in every request log" class of leaks.

## Relationship to other docs

- [`11-multi-tenancy.md`](./11-multi-tenancy.md) — supersedes the "Token issuance is explicitly out of scope" line for browser users. Bearer-token issuance for machine clients stays exactly as that doc describes. The tenant registry's role narrows to "machine credentials only."
- [`14-microservice-decomposition.md`](./14-microservice-decomposition.md) — unchanged. Downstream services see the same signed `TenantContext`. WorkOS is invisible past the gateway.
- [`18-observability.md`](./18-observability.md) — extended: audit entries gain an `auth` field (`"session" | "bearer"`) and, for session-auth, a `sub` field naming the WorkOS user. Tenants can finally answer "which user committed this?" instead of just "the tenant did."
- [`19-web-frontend.md`](./19-web-frontend.md) — supersedes "OAuth, SSO, magic-link login" non-goal and the "paste-the-token surface" stance for WorkOS-enabled deployments. The `<TokenPrompt>` modal survives as the fallback for self-hosted single-tenant installs.

## What's deferred

- **Per-user roles inside a tenant.** Read-only members, branch-restricted writers, commit approvers. WorkOS exposes the role claim; OMP doesn't model it yet. Lands when there's a concrete enforcement target.
- **Enterprise SSO connections.** Free in WorkOS staging, paid per-connection in production. The integration is the same OIDC flow — flipping it on per-customer is a dashboard config change, not a code change. Defer until a customer asks.
- **Directory Sync (SCIM).** Same story: paid, additive, no code change required to start.
- **WorkOS-managed API keys for machine clients.** The OMP tenant registry remains the M2M surface. Revisit if WorkOS opens an M2M product on the free tier.
- **Custom domain (`auth.yourapp.com`).** $99/mo and unnecessary for a course project. The default `auth.workos.com` redirect target works.
- **Active-session revocation.** Stateless cookies do not support it; key rotation is the bulk hammer. Per-session revocation requires a session table — defer until there's a reason.
- **Per-user audit drilldowns.** The audit log gets the `sub` field in v1 of this design; building a "show me everything user X did" UI on top is deferred to a v2 audit page.

## Implementation status

Nothing implemented yet. This is the design step. Status checklist this doc commits to:

- ⏸ `crates/omp-gateway/src/auth.rs` — OIDC client, per-flow PKCE state cookie (named `omp_oidc_state_<id>`, id echoed in OIDC `state` param), session cookie carrying `workos_session_id`, four route handlers (`login`, `callback`, `refresh`, `logout`).
- ⏸ `GatewayConfig.workos: Option<WorkOsConfig>`; env-var override for the secret.
- ⏸ `resolve_tenant` reordered: cookie first, Bearer second; expired-but-signature-valid cookies accepted only on `/auth/refresh`.
- ⏸ `/status` extended with `auth_mode` field — no second probe endpoint.
- ⏸ RP-initiated logout via WorkOS `end_session_endpoint`; OMP cookie cleared and WorkOS session terminated.
- ⏸ In-memory `(sub, until_epoch)` revocation denylist with TTL-bounded eviction; `session.revoked` audit event for restart-replay; `deny_floor_epoch` config knob (production-readiness pass, not v1 demo).
- ⏸ `refresh_grace_seconds` bound on `/auth/refresh`; cookies expired beyond the grace window are rejected.
- ⏸ `/auth/logout` accepts both GET and POST; frontend invokes it as a top-level navigation so the OIDC `end_session_endpoint` redirect chain completes.
- ⏸ Audit-UI `sub → email` lookup via WorkOS userinfo with short in-memory cache (no email in cookie, no email in request logs).
- ⏸ Frontend `auth.ts` extended with `'session'` mode; `<LoginGate>` replaces `<TokenPrompt>` when `auth_mode === 'workos'`. 401-handling does top-level redirect to `/auth/refresh`.
- ⏸ Vite dev proxy entries for `/auth`, `/status`, and the API prefixes.
- ⏸ Audit log carries `auth` and `sub` fields ([`18-observability.md`](./18-observability.md)); Bearer path uses token label or `sha256(token)[..8]`.
- ⏸ Helm values + secret wiring; no chart structural change.
- ⏸ Stub-OIDC integration test in CI.

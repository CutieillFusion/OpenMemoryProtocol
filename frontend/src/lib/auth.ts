import { writable, get, type Writable } from 'svelte/store';

/**
 * Auth modes the UI gates on.
 *
 *   - `unknown`        — pre-probe.
 *   - `no-auth`        — single-tenant dev; no gate, no Authorization header.
 *   - `token-required` — gateway fronts the shard with bearer-token auth
 *                        registered in TOML; show the token-paste modal.
 *   - `workos`         — gateway is configured with WorkOS; show the
 *                        "Sign in with WorkOS" button.
 *   - `session`        — a signed `omp_session` cookie is present in this
 *                        browser; treat the user as logged in. The mode is
 *                        client-side; the server side is `workos` either way.
 */
export type AuthMode = 'unknown' | 'no-auth' | 'token-required' | 'workos' | 'session';

export interface AuthState {
  mode: AuthMode;
  token: string | null;
}

const TOKEN_KEY = 'omp.token';
/**
 * The actual session cookie (`omp_session`) is HttpOnly and not visible to
 * JavaScript. The gateway sets a non-HttpOnly companion `omp_signed_in=1`
 * for the same lifetime so the frontend can detect "the user is signed in"
 * without a network round trip and without ever seeing the session bytes.
 */
const SESSION_PRESENCE_COOKIE = 'omp_signed_in';

function hasSessionCookie(): boolean {
  if (typeof document === 'undefined') return false;
  return document.cookie
    .split(';')
    .some((p) => p.trim().startsWith(`${SESSION_PRESENCE_COOKIE}=`));
}

const initial: AuthState = {
  mode: 'unknown',
  token: typeof localStorage !== 'undefined' ? localStorage.getItem(TOKEN_KEY) : null
};

export const auth: Writable<AuthState> = writable(initial);

export function getToken(): string | null {
  return get(auth).token;
}

export function setToken(token: string): void {
  if (typeof localStorage !== 'undefined') {
    localStorage.setItem(TOKEN_KEY, token);
  }
  auth.update((s) => ({ ...s, token, mode: 'token-required' }));
}

export function clearToken(): void {
  if (typeof localStorage !== 'undefined') {
    localStorage.removeItem(TOKEN_KEY);
  }
  auth.update((s) => ({ ...s, token: null }));
}

/**
 * Probe the gateway to detect auth mode.
 *
 * Order:
 *  1. If an `omp_session` cookie is present, mode = `session` (browser is
 *     already logged in via WorkOS). Skip the network probe.
 *  2. Otherwise GET `/status`:
 *     - 200 with `auth_mode === "workos"` → show WorkOS sign-in button.
 *     - 200 with `auth_mode === "token"` (or no field) → token-paste modal.
 *     - 200 with `auth_mode === "no-auth"` → no gate (single-tenant dev).
 *     - 401 → token-required (legacy gateway without `auth_mode` field).
 */
export async function probeAuth(): Promise<AuthMode> {
  if (hasSessionCookie()) {
    auth.update((s) => ({ ...s, mode: 'session' }));
    return 'session';
  }
  try {
    const resp = await fetch('/status', { method: 'GET' });
    if (resp.status === 200) {
      let mode: AuthMode = 'no-auth';
      try {
        const body = (await resp.json()) as { auth_mode?: string };
        if (body && typeof body.auth_mode === 'string') {
          if (body.auth_mode === 'workos') mode = 'workos';
          else if (body.auth_mode === 'token') mode = 'token-required';
          else mode = 'no-auth';
        }
      } catch {
        // No JSON body — treat as no-auth (matches pre-WorkOS gateway).
      }
      auth.update((s) => ({ ...s, mode }));
      return mode;
    }
    if (resp.status === 401) {
      auth.update((s) => ({ ...s, mode: 'token-required' }));
      return 'token-required';
    }
    // 5xx / unexpected status: we couldn't learn the mode. Stay in
    // `unknown` (no gate) rather than demote to `token-required`, which
    // would pop a bearer-paste modal in a WorkOS deployment whose shard is
    // momentarily unreachable.
    auth.update((s) => ({ ...s, mode: 'unknown' }));
    return 'unknown';
  } catch {
    auth.update((s) => ({ ...s, mode: 'unknown' }));
    return 'unknown';
  }
}

/** Top-level navigation to start the WorkOS login flow. */
export function startWorkosLogin(returnTo: string = '/ui/'): void {
  if (typeof window === 'undefined') return;
  window.location.href = `/auth/login?return_to=${encodeURIComponent(returnTo)}`;
}

/** Top-level navigation to refresh the session cookie. */
export function startWorkosRefresh(returnTo: string): void {
  if (typeof window === 'undefined') return;
  window.location.href = `/auth/refresh?return_to=${encodeURIComponent(returnTo)}`;
}

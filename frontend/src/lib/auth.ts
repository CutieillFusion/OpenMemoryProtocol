import { writable, get, type Writable } from 'svelte/store';

export type AuthMode = 'unknown' | 'no-auth' | 'token-required';

export interface AuthState {
  mode: AuthMode;
  token: string | null;
}

const TOKEN_KEY = 'omp.token';

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
 * Calls `GET /status` without an Authorization header:
 *  - 200 → gateway is in `--no-auth` (single-tenant dev) mode.
 *  - 401 → gateway requires a bearer token.
 *
 * The `/status` endpoint is chosen because it's cheap, safe to call without
 * a token, and exists in both single- and multi-tenant deployments.
 */
export async function probeAuth(): Promise<AuthMode> {
  try {
    const resp = await fetch('/status', { method: 'GET' });
    if (resp.status === 200) {
      auth.update((s) => ({ ...s, mode: 'no-auth' }));
      return 'no-auth';
    }
    if (resp.status === 401) {
      auth.update((s) => ({ ...s, mode: 'token-required' }));
      return 'token-required';
    }
    // Anything else (502, 503, network error) — surface as token-required;
    // the user can retry once the backend recovers.
    auth.update((s) => ({ ...s, mode: 'token-required' }));
    return 'token-required';
  } catch {
    auth.update((s) => ({ ...s, mode: 'token-required' }));
    return 'token-required';
  }
}

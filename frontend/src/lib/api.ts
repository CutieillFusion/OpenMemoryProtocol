import { getToken, clearToken, auth, startWorkosRefresh } from './auth';
import { get as storeGet } from 'svelte/store';
import type {
  AddResult,
  AuditResponse,
  BlobInfo,
  BranchInfo,
  BuildArtifact,
  BuildRequestBody,
  BuildView,
  CommitView,
  DiffEntry,
  FieldValue,
  FileListing,
  Manifest,
  QueryResult,
  RepoStatus,
  Schema,
  TreeEntry,
  UploadHandle
} from './types';

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
    public details?: Record<string, unknown>
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

interface RequestOptions {
  method?: string;
  body?: BodyInit | null;
  headers?: Record<string, string>;
  query?: Record<string, string | number | boolean | undefined | null>;
  signal?: AbortSignal;
  /** When true, skip the bearer header even if a token is set. Used for `probeAuth`. */
  noAuth?: boolean;
}

function buildUrl(path: string, query?: RequestOptions['query']): string {
  if (!query) return path;
  const sp = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v === undefined || v === null) continue;
    sp.append(k, String(v));
  }
  const qs = sp.toString();
  return qs ? `${path}?${qs}` : path;
}

export async function request<T>(path: string, opts: RequestOptions = {}): Promise<T> {
  const headers: Record<string, string> = { ...(opts.headers ?? {}) };
  // In session mode the browser sends the `omp_session` cookie automatically;
  // attaching a stale `Authorization` header would just be noise (and the
  // gateway resolves cookies first anyway).
  const inSessionMode = storeGet(auth).mode === 'session';
  if (!opts.noAuth && !inSessionMode) {
    const tok = getToken();
    if (tok) headers['Authorization'] = `Bearer ${tok}`;
  }
  const resp = await fetch(buildUrl(path, opts.query), {
    method: opts.method ?? 'GET',
    headers,
    body: opts.body ?? null,
    signal: opts.signal,
    credentials: 'same-origin'
  });

  if (resp.status === 401) {
    const currentMode = storeGet(auth).mode;
    if (currentMode === 'session' || currentMode === 'workos') {
      // WorkOS-deployment 401: cookie expired/rotated, or this is the
      // first call after sign-out. Bounce through `/auth/refresh` as a
      // top-level navigation; a `fetch` retry can't follow the OIDC chain.
      // We never fall back to the token-paste modal in a WorkOS deployment.
      const here = typeof location !== 'undefined' ? location.pathname + location.search : '/ui/';
      startWorkosRefresh(here);
    } else if (!opts.noAuth && getToken()) {
      // Token-mode deployment, token-bearing call came back 401 → token is
      // bad. Clear and re-prompt for a fresh paste.
      clearToken();
      auth.update((s) => ({ ...s, mode: 'token-required' }));
    }
  }

  // Bytes endpoint: caller handles raw response.
  if (path.startsWith('/bytes/')) {
    if (!resp.ok) await throwFromResponse(resp);
    return resp as unknown as T;
  }

  // 204 No Content — common for upload chunk PATCH/DELETE.
  if (resp.status === 204) {
    return undefined as T;
  }

  if (!resp.ok) {
    await throwFromResponse(resp);
  }

  // Parse JSON. Some endpoints return empty bodies on 200 (rare); guard.
  const text = await resp.text();
  if (!text) return undefined as T;
  return JSON.parse(text) as T;
}

async function throwFromResponse(resp: Response): Promise<never> {
  let code = 'unknown';
  let message = `${resp.status} ${resp.statusText}`;
  let details: Record<string, unknown> | undefined;
  try {
    const body = await resp.json();
    if (body && typeof body === 'object' && 'error' in body && body.error) {
      const err = body.error as { code?: string; message?: string; details?: Record<string, unknown> };
      code = err.code ?? code;
      message = err.message ?? message;
      details = err.details;
    }
  } catch {
    // body wasn't JSON — keep defaults.
  }
  throw new ApiError(resp.status, code, message, details);
}

// ---- Endpoint wrappers ----

export const status = (signal?: AbortSignal) =>
  request<RepoStatus>('/status', { signal });

export const health = (signal?: AbortSignal) =>
  request<{ ok: boolean; service?: string }>('/healthz', { signal, noAuth: true });

export interface MeResponse {
  tenant: string;
  sub: string;
  email: string | null;
  email_verified: boolean | null;
  first_name: string | null;
  last_name: string | null;
  profile_picture_url: string | null;
}

/**
 * Fetch the WorkOS profile of the current session. Only meaningful in
 * `'session'` mode; returns 401 / 404 in other modes.
 */
export const getMe = (signal?: AbortSignal) =>
  request<MeResponse>('/auth/me', { signal });

export interface WidgetTokenResponse {
  token: string;
  organization_id: string;
}

/**
 * Mint a short-lived WorkOS widget session token bound to the current
 * user + organization, scoped to API-key management. Feed `token` into
 * the WorkOS API Keys widget; the widget then talks directly to WorkOS
 * for create / list / revoke.
 */
export const getWidgetToken = (signal?: AbortSignal) =>
  request<WidgetTokenResponse>('/auth/widget-token', { signal });

export const listFiles = (params: { at?: string; prefix?: string; verbose?: boolean } = {}, signal?: AbortSignal) =>
  request<FileListing[]>('/files', { query: params, signal });

export const getFile = (
  path: string,
  params: { at?: string; verbose?: boolean; staged?: boolean } = {},
  signal?: AbortSignal
) =>
  request<Manifest | TreeEntry[] | BlobInfo>(
    `/files/${encodePath(path)}`,
    { query: params as Record<string, string | number | boolean | undefined | null>, signal }
  );

export const getBytesUrl = (path: string, params: { at?: string; staged?: boolean } = {}): string => {
  const base = `/bytes/${encodePath(path)}`;
  const sp = new URLSearchParams();
  if (params.staged) sp.append('staged', 'true');
  else if (params.at) sp.append('at', params.at);
  const qs = sp.toString();
  return qs ? `${base}?${qs}` : base;
};

export const fetchBytes = async (
  path: string,
  params: { at?: string; staged?: boolean } = {},
  signal?: AbortSignal
): Promise<Response> => {
  const headers: Record<string, string> = {};
  if (storeGet(auth).mode !== 'session') {
    const tok = getToken();
    if (tok) headers['Authorization'] = `Bearer ${tok}`;
  }
  const resp = await fetch(getBytesUrl(path, params), {
    headers,
    signal,
    credentials: 'same-origin'
  });
  if (!resp.ok) await throwFromResponse(resp);
  return resp;
};

/**
 * Fetch bytes through the auth-aware path and expose them as a `blob:` URL
 * for `<img src>` / `<iframe src>` consumers. Returns the URL plus a
 * `revoke` callback the caller MUST invoke on cleanup (component unmount,
 * subsequent fetch) to release the underlying memory.
 *
 * Why not `<img src={getBytesUrl(...)}>`? Image element requests don't
 * carry the `Authorization` header, so they 401 silently when auth is on.
 */
export const fetchBytesAsBlobUrl = async (
  path: string,
  params: { at?: string; staged?: boolean } = {},
  signal?: AbortSignal
): Promise<{ url: string; revoke: () => void }> => {
  const resp = await fetchBytes(path, params, signal);
  const blob = await resp.blob();
  const url = URL.createObjectURL(blob);
  return { url, revoke: () => URL.revokeObjectURL(url) };
};

export const patchFields = (path: string, fields: Record<string, FieldValue>, signal?: AbortSignal) =>
  request<Manifest>(`/files/${encodePath(path)}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(fields),
    signal
  });

export const deleteFile = (path: string, signal?: AbortSignal) =>
  request<{ ok: boolean }>(`/files/${encodePath(path)}`, {
    method: 'DELETE',
    signal
  });

export const getTree = (
  path: string = '',
  params: { at?: string; recursive?: boolean; verbose?: boolean; staged?: boolean } = {},
  signal?: AbortSignal
) => {
  const url = path ? `/tree/${encodePath(path)}` : '/tree';
  return request<TreeEntry[]>(url, {
    query: params as Record<string, string | number | boolean | undefined | null>,
    signal
  });
};

export const commit = (
  body: { message: string; author?: { name?: string; email?: string; timestamp?: string } },
  signal?: AbortSignal
) =>
  request<import('./types').CommitResponse>('/commit', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal
  });

export const log = (params: { path?: string; max?: number; verbose?: boolean } = {}, signal?: AbortSignal) =>
  request<CommitView[]>('/log', { query: params, signal });

export const diff = (params: { from: string; to: string; path?: string }, signal?: AbortSignal) =>
  request<DiffEntry[]>('/diff', { query: params, signal });

export const listBranches = (signal?: AbortSignal) =>
  request<BranchInfo[]>('/branches', { signal });

export const createBranch = (body: { name: string; start?: string }, signal?: AbortSignal) =>
  request<{ ok: boolean }>('/branches', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal
  });

export const checkout = (ref: string, signal?: AbortSignal) =>
  request<{ ok: boolean }>('/checkout', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ ref }),
    signal
  });

export const query = (
  params: { where?: string; prefix?: string; at?: string; cursor?: string; limit?: number } = {},
  signal?: AbortSignal
) => request<QueryResult>('/query', { query: params, signal });

export const getSchemas = (params: { at?: string } = {}, signal?: AbortSignal) =>
  request<Schema[]>('/schemas', { query: params, signal });

export const audit = (params: { limit?: number } = {}, signal?: AbortSignal) =>
  request<AuditResponse>('/audit', { query: params, signal });

// ---- Upload session API (resumable) ----

export const startUpload = (declared_size: number, signal?: AbortSignal) =>
  request<UploadHandle>('/uploads', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ declared_size }),
    signal
  });

export const appendUpload = (id: string, offset: number, chunk: ArrayBuffer | Blob, signal?: AbortSignal) =>
  request<void>(`/uploads/${encodeURIComponent(id)}`, {
    method: 'PATCH',
    query: { offset },
    headers: { 'Content-Type': 'application/octet-stream' },
    body: chunk as BodyInit,
    signal
  });

export const cancelUpload = (id: string, signal?: AbortSignal) =>
  request<void>(`/uploads/${encodeURIComponent(id)}`, {
    method: 'DELETE',
    signal
  });

export const commitUpload = (
  id: string,
  body: { path: string; file_type?: string; fields?: Record<string, FieldValue> },
  signal?: AbortSignal
) =>
  request<AddResult>(`/uploads/${encodeURIComponent(id)}/commit`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal
  });

// ---- Single-shot upload (multipart) ----

export const uploadFile = async (
  body: { path: string; file: Blob; file_type?: string; fields?: Record<string, string> },
  signal?: AbortSignal
): Promise<AddResult> => {
  const fd = new FormData();
  fd.append('path', body.path);
  fd.append('file', body.file, body.path.split('/').pop() ?? 'file');
  if (body.file_type) fd.append('file_type', body.file_type);
  if (body.fields) {
    for (const [k, v] of Object.entries(body.fields)) {
      fd.append(`fields[${k}]`, v);
    }
  }
  return request<AddResult>('/files', { method: 'POST', body: fd, signal });
};

// ---- Server-side probe build (omp-builder) ----

export const startBuild = (body: BuildRequestBody, signal?: AbortSignal) =>
  request<{ job_id: string }>('/probes/build', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal
  });

export const getBuild = (jobId: string, signal?: AbortSignal) =>
  request<BuildView>(`/probes/build/${encodeURIComponent(jobId)}`, { signal });

export const cancelBuild = (jobId: string, signal?: AbortSignal) =>
  request<void>(`/probes/build/${encodeURIComponent(jobId)}`, {
    method: 'DELETE',
    signal
  });

/**
 * Stage every artifact returned by a successful build through the existing
 * `POST /files` multipart endpoint. Each artifact lands as a raw blob at
 * its `path`. After the loop returns, the user can navigate to /commit.
 */
export const stageBuildArtifacts = async (
  artifacts: BuildArtifact[],
  signal?: AbortSignal
): Promise<void> => {
  for (const art of artifacts) {
    const buf = base64ToArrayBuffer(art.bytes_b64);
    const blob = new Blob([buf], { type: 'application/octet-stream' });
    const fd = new FormData();
    fd.append('path', art.path);
    fd.append('file', blob, art.path.split('/').pop() ?? 'file');
    await request<unknown>('/files', { method: 'POST', body: fd, signal });
  }
};

function base64ToArrayBuffer(b64: string): ArrayBuffer {
  const bin = atob(b64);
  const buf = new ArrayBuffer(bin.length);
  const view = new Uint8Array(buf);
  for (let i = 0; i < bin.length; i++) view[i] = bin.charCodeAt(i);
  return buf;
}

// ---- Marketplace ----

export interface MarketplaceProbe {
  id: string;
  publisher_sub: string;
  namespace: string;
  name: string;
  version: string;
  description: string | null;
  wasm_hash: string;
  manifest_hash: string;
  readme_hash: string | null;
  source_hash: string | null;
  published_at: number;
  yanked_at: number | null;
  downloads: number;
}

export const listMarketplaceProbes = (
  params: { namespace?: string; name?: string; q?: string; limit?: number } = {},
  signal?: AbortSignal
) =>
  request<{ probes: MarketplaceProbe[] }>('/marketplace/probes', {
    query: params,
    signal
  });

export const getMarketplaceProbe = (id: string, signal?: AbortSignal) =>
  request<{ probe: MarketplaceProbe; manifest_preview: string | null }>(
    `/marketplace/probes/${encodeURIComponent(id)}`,
    { signal }
  );

export const installMarketplaceProbe = (id: string, signal?: AbortSignal) =>
  request<{ ok: boolean; namespace: string; name: string; staged: unknown[] }>(
    `/marketplace/install/${encodeURIComponent(id)}`,
    { method: 'POST', signal }
  );

export const yankMarketplaceProbe = (id: string, signal?: AbortSignal) =>
  request<{ ok: boolean; already_yanked?: boolean }>(
    `/marketplace/probes/${encodeURIComponent(id)}`,
    { method: 'DELETE', signal }
  );

export const publishMarketplaceProbe = async (
  body: {
    namespace: string;
    name: string;
    version: string;
    description?: string;
    /** Rust `lib.rs` source. The marketplace builds the wasm server-side. */
    source: Blob;
    /** Probe manifest TOML (`probe.toml`). */
    manifest: Blob;
    readme?: Blob;
  },
  signal?: AbortSignal
): Promise<{ probe: MarketplaceProbe; build_log: string }> => {
  const fd = new FormData();
  fd.append('namespace', body.namespace);
  fd.append('name', body.name);
  fd.append('version', body.version);
  if (body.description) fd.append('description', body.description);
  fd.append('source', body.source, 'lib.rs');
  fd.append('manifest', body.manifest, 'probe.toml');
  if (body.readme) fd.append('readme', body.readme, 'README.md');
  return request<{ probe: MarketplaceProbe; build_log: string }>(
    '/marketplace/probes',
    { method: 'POST', body: fd, signal }
  );
};

export const patchMarketplaceProbe = (
  id: string,
  patch: { description?: string; readme?: string },
  signal?: AbortSignal
) =>
  request<{ probe: MarketplaceProbe }>(
    `/marketplace/probes/${encodeURIComponent(id)}`,
    {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(patch),
      signal
    }
  );

// ---- Schema marketplace ----

export interface MarketplaceSchema {
  id: string;
  publisher_sub: string;
  file_type: string;
  version: string;
  description: string | null;
  schema_hash: string;
  readme_hash: string | null;
  published_at: number;
  yanked_at: number | null;
}

export const listMarketplaceSchemas = (
  params: { file_type?: string; q?: string; limit?: number } = {},
  signal?: AbortSignal
) =>
  request<{ schemas: MarketplaceSchema[] }>('/marketplace/schemas', {
    query: params,
    signal
  });

export const getMarketplaceSchema = (id: string, signal?: AbortSignal) =>
  request<{ schema: MarketplaceSchema; schema_preview: string | null }>(
    `/marketplace/schemas/${encodeURIComponent(id)}`,
    { signal }
  );

export const publishMarketplaceSchema = async (
  body: {
    version: string;
    description?: string;
    /** `schema.toml` body. file_type is taken from the TOML itself. */
    schema: Blob;
    readme?: Blob;
  },
  signal?: AbortSignal
): Promise<{ schema: MarketplaceSchema }> => {
  const fd = new FormData();
  fd.append('version', body.version);
  if (body.description) fd.append('description', body.description);
  fd.append('schema', body.schema, 'schema.toml');
  if (body.readme) fd.append('readme', body.readme, 'README.md');
  return request<{ schema: MarketplaceSchema }>('/marketplace/schemas', {
    method: 'POST',
    body: fd,
    signal
  });
};

export const patchMarketplaceSchema = (
  id: string,
  patch: { description?: string; readme?: string },
  signal?: AbortSignal
) =>
  request<{ schema: MarketplaceSchema }>(
    `/marketplace/schemas/${encodeURIComponent(id)}`,
    {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(patch),
      signal
    }
  );

export const yankMarketplaceSchema = (id: string, signal?: AbortSignal) =>
  request<{ ok: boolean; already_yanked?: boolean }>(
    `/marketplace/schemas/${encodeURIComponent(id)}`,
    { method: 'DELETE', signal }
  );

export const fetchMarketplaceSchemaBlob = async (
  id: string,
  hash: string,
  signal?: AbortSignal
): Promise<Response> => {
  const path = `/marketplace/schemas/${encodeURIComponent(id)}/blobs/${encodeURIComponent(hash)}`;
  const resp = await fetch(path, { signal, credentials: 'same-origin' });
  if (!resp.ok) {
    throw new ApiError(resp.status, 'blob_fetch', `${resp.status} ${resp.statusText}`);
  }
  return resp;
};

/**
 * Fetch a single blob (probe.wasm, probe.toml, README.md, or source) by its
 * sha256 hash. The marketplace returns raw bytes; callers that want text
 * should call `.text()` on the Response. Used by the probe detail page to
 * render manifest / README / source.
 */
export const fetchMarketplaceBlob = async (
  id: string,
  hash: string,
  signal?: AbortSignal
): Promise<Response> => {
  const path = `/marketplace/probes/${encodeURIComponent(id)}/blobs/${encodeURIComponent(hash)}`;
  const resp = await fetch(path, { signal, credentials: 'same-origin' });
  if (!resp.ok) {
    throw new ApiError(resp.status, 'blob_fetch', `${resp.status} ${resp.statusText}`);
  }
  return resp;
};

// ---- helpers ----

/** Encode a path so each segment is percent-encoded but slashes are kept. */
function encodePath(p: string): string {
  return p
    .split('/')
    .filter((s) => s.length > 0)
    .map(encodeURIComponent)
    .join('/');
}

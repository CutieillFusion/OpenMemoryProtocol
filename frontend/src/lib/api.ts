import { getToken, clearToken, auth } from './auth';
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
  if (!opts.noAuth) {
    const tok = getToken();
    if (tok) headers['Authorization'] = `Bearer ${tok}`;
  }
  const resp = await fetch(buildUrl(path, opts.query), {
    method: opts.method ?? 'GET',
    headers,
    body: opts.body ?? null,
    signal: opts.signal
  });

  if (resp.status === 401) {
    // A token-bearing call that came back 401 means the token is bad
    // (revoked, expired, or wrong tenant). Clear and re-prompt.
    if (!opts.noAuth && getToken()) {
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

export const listFiles = (params: { at?: string; prefix?: string; verbose?: boolean } = {}, signal?: AbortSignal) =>
  request<FileListing[]>('/files', { query: params, signal });

export const getFile = (path: string, params: { at?: string; verbose?: boolean } = {}, signal?: AbortSignal) =>
  request<Manifest | TreeEntry[] | BlobInfo>(
    `/files/${encodePath(path)}`,
    { query: params, signal }
  );

export const getBytesUrl = (path: string, params: { at?: string } = {}): string => {
  const base = `/bytes/${encodePath(path)}`;
  if (!params.at) return base;
  const sp = new URLSearchParams({ at: params.at });
  return `${base}?${sp.toString()}`;
};

export const fetchBytes = async (path: string, params: { at?: string } = {}, signal?: AbortSignal): Promise<Response> => {
  const headers: Record<string, string> = {};
  const tok = getToken();
  if (tok) headers['Authorization'] = `Bearer ${tok}`;
  const resp = await fetch(getBytesUrl(path, params), { headers, signal });
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
  params: { at?: string } = {},
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
  params: { at?: string; recursive?: boolean; verbose?: boolean } = {},
  signal?: AbortSignal
) => {
  const url = path ? `/tree/${encodePath(path)}` : '/tree';
  return request<TreeEntry[]>(url, { query: params, signal });
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

// ---- helpers ----

/** Encode a path so each segment is percent-encoded but slashes are kept. */
function encodePath(p: string): string {
  return p
    .split('/')
    .filter((s) => s.length > 0)
    .map(encodeURIComponent)
    .join('/');
}

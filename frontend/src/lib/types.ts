// Hand-typed mirrors of the Rust serde shapes. Source of truth lives in
// crates/omp-core/src/{api,manifest,query,uploads,audit}.rs and
// crates/omp-server/src/routes.rs.

// Hash is hex-encoded (see omp-core/src/hash.rs::Serialize for Hash).
export type Hash = string;

// FieldValue is `#[serde(untagged)]` so it serializes as plain JSON.
// Datetime is indistinguishable from String at the wire level — callers
// detect ISO-8601-shaped strings if they want to render them differently.
export type FieldValue =
  | null
  | string
  | number
  | boolean
  | FieldValue[]
  | { [key: string]: FieldValue };

export type RenderKind = 'text' | 'hex' | 'image' | 'markdown' | 'binary' | 'none';

export interface RenderHint {
  kind: RenderKind;
  max_inline_bytes?: number;
}

export interface Manifest {
  source_hash?: Hash; // present only with ?verbose=true
  file_type: string;
  schema_hash?: Hash; // verbose-only
  ingested_at: string;
  ingester_version?: string; // verbose-only
  probe_hashes?: Record<string, Hash>; // verbose-only
  fields: Record<string, FieldValue>;
  /** Schema-driven render hint — present on `GET /files/{path}` responses. */
  render?: RenderHint;
}

/** Blob-shape body returned by `GET /files/{path}` when the path resolves to a raw blob. */
export interface BlobInfo {
  kind: 'blob';
  hash: Hash;
  size: number;
  render?: RenderHint;
}

export interface TreeEntry {
  name: string;
  mode: 'blob' | 'manifest' | 'tree';
  hash?: Hash; // verbose-only
}

export interface FileListing {
  path: string;
  manifest_hash?: Hash; // verbose-only
  source_hash?: Hash; // verbose-only
  file_type: string;
}

export interface CommitView {
  hash: Hash;
  tree?: Hash; // verbose-only
  parents?: Hash[]; // verbose-only
  author: string;
  email: string;
  timestamp: string;
  message: string;
}

export type DiffStatus = 'added' | 'removed' | 'modified' | 'unchanged';

export interface DiffEntry {
  path: string;
  status: DiffStatus;
  before?: Hash | null;
  after?: Hash | null;
}

export interface BranchInfo {
  name: string;
  head: Hash | null;
  is_current: boolean;
}

export type StagedKind = 'upsert' | 'remove';

export interface StagedChange {
  path: string;
  kind: StagedKind;
  hash?: Hash | null;
}

export interface RepoStatus {
  branch: string | null;
  head: Hash | null;
  staged: StagedChange[];
}

export interface QueryMatch {
  path: string;
  manifest_hash: Hash;
  source_hash: Hash;
  file_type: string;
  fields: Record<string, FieldValue>;
}

export interface QueryResult {
  matches: QueryMatch[];
  next_cursor: string | null;
}

// `GET /schemas` — wire format from omp-core/src/schema.rs::SchemaSummary.
// Drives query-editor autocomplete: each schema's fields surface as
// completions.
export type SchemaFieldType =
  | 'string'
  | 'int'
  | 'float'
  | 'bool'
  | 'datetime'
  | 'list[string]'
  | 'list[int]'
  | 'list[float]'
  | 'list[bool]'
  | 'list[datetime]'
  | 'object';

export interface SchemaField {
  name: string;
  type: SchemaFieldType | string;
  required: boolean;
  description?: string;
}

export interface Schema {
  file_type: string;
  mime_patterns: string[];
  fields: SchemaField[];
}

export type AuditValue =
  | null
  | string
  | number
  | boolean
  | AuditValue[]
  | { [key: string]: AuditValue };

export interface AuditEntry {
  version: number;
  parent: Hash | null;
  at: string;
  tenant: string;
  event: string;
  actor: string;
  details: Record<string, AuditValue>;
}

export interface AuditResponse {
  entries: AuditEntry[];
  verified: boolean;
}

export interface UploadHandle {
  upload_id: string;
  chunk_size_bytes: number;
}

// Upload-commit response shape: AddResult from omp-core/src/api.rs.
// Either a new manifest entry or a raw blob (for files in schemas/, probes/, omp.toml).
export type AddResult =
  | { kind: 'manifest'; path: string; manifest_hash: Hash; source_hash: Hash; file_type: string }
  | { kind: 'blob'; path: string; hash: Hash; size: number };

// Error envelope from the gateway/server: {"error": {"code", "message", "details?"}}
export interface ErrorEnvelope {
  error: {
    code: string;
    message: string;
    details?: Record<string, unknown>;
  };
}

// Watch envelope (SSE event payload from /watch).
export interface WatchEvent {
  type: string;
  tenant: string;
  occurred_at: string;
  trace_id: string;
}

// ---- Server-side probe build (omp-builder) ----

export type BuildState = 'queued' | 'building' | 'ok' | 'failed' | 'cancelled';

export interface BuildArtifact {
  path: string;
  bytes_b64: string;
}

export interface BuildView {
  id: string;
  tenant: string;
  state: BuildState;
  namespace: string;
  name: string;
  artifacts?: BuildArtifact[] | null;
  error?: string | null;
  created_at: string;
  updated_at: string;
}

export interface BuildRequestBody {
  namespace: string;
  name: string;
  lib_rs: string;
  probe_toml: string;
}

// ---- Reprobe summary (returned alongside `/commit` when a schema change
// triggered the auto-rebuild). ----

export interface ReprobeSkip {
  path: string;
  reason: string;
}

export interface ReprobeSummary {
  file_type: string;
  count: number;
  skipped: ReprobeSkip[];
}

export interface CommitResponse {
  hash: string;
  reprobed?: ReprobeSummary[];
}

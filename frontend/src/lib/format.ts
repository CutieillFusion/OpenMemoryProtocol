import type { FieldValue } from './types';

const ISO_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/;

export function shortHash(h: string | null | undefined, n = 8): string {
  if (!h) return '';
  return h.slice(0, n);
}

export function formatTimestamp(ts: string | null | undefined): string {
  if (!ts) return '';
  if (!ISO_RE.test(ts)) return ts;
  try {
    const d = new Date(ts);
    return d.toLocaleString();
  } catch {
    return ts;
  }
}

export function relativeTime(ts: string | null | undefined): string {
  if (!ts) return '';
  try {
    const d = new Date(ts);
    const diff = Date.now() - d.getTime();
    const s = Math.round(diff / 1000);
    if (s < 60) return `${s}s ago`;
    const m = Math.round(s / 60);
    if (m < 60) return `${m}m ago`;
    const h = Math.round(m / 60);
    if (h < 24) return `${h}h ago`;
    const days = Math.round(h / 24);
    return `${days}d ago`;
  } catch {
    return ts;
  }
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const kb = n / 1024;
  if (kb < 1024) return `${kb.toFixed(1)} KiB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb.toFixed(1)} MiB`;
  return `${(mb / 1024).toFixed(2)} GiB`;
}

export function describeField(v: FieldValue): string {
  if (v === null) return 'null';
  if (typeof v === 'string') {
    if (ISO_RE.test(v)) return formatTimestamp(v);
    return v;
  }
  if (typeof v === 'number' || typeof v === 'boolean') return String(v);
  if (Array.isArray(v)) return `[${v.length} items]`;
  return `{${Object.keys(v).length} keys}`;
}

export function fieldKind(v: FieldValue): string {
  if (v === null) return 'null';
  if (typeof v === 'string') return ISO_RE.test(v) ? 'datetime' : 'string';
  if (typeof v === 'number') return Number.isInteger(v) ? 'int' : 'float';
  if (typeof v === 'boolean') return 'bool';
  if (Array.isArray(v)) return 'list';
  return 'object';
}

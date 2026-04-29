<script lang="ts">
  import { onDestroy } from 'svelte';
  import { fetchBytes, fetchBytesAsBlobUrl, getBytesUrl, ApiError } from '$lib/api';
  import { formatBytes } from '$lib/format';
  import type { RenderHint } from '$lib/types';
  import { marked } from 'marked';
  import DOMPurify from 'dompurify';

  /** File path inside the repo (without leading slash). */
  export let path: string;
  /** Optional `?at=` ref (branch / hash / HEAD~n). */
  export let at: string = '';
  /** Schema-driven render hint from `GET /files/{path}`. */
  export let render: RenderHint;
  /** When true, fetch bytes from the staging index (not committed yet). */
  export let staged: boolean = false;

  const DEFAULT_CAP = 64 * 1024;

  $: cap = render.max_inline_bytes ?? DEFAULT_CAP;

  let loading = false;
  let error: string | null = null;

  // Text/markdown state.
  let text: string | null = null;
  let textTruncated = false;
  let actualSize = 0;

  // Hex state (raw bytes captured during fetch so we can render hex too).
  let hexLines: string | null = null;

  // Image state.
  let imageUrl: string | null = null;
  let imageRevoke: (() => void) | null = null;

  $: byteParams = staged ? { staged: true } : at ? { at } : {};

  // Re-fetch whenever path/at/kind changes.
  $: void load(path, at, render.kind, cap);

  onDestroy(() => {
    revokeImage();
  });

  function revokeImage() {
    if (imageRevoke) {
      imageRevoke();
      imageRevoke = null;
    }
    imageUrl = null;
  }

  function reset() {
    error = null;
    text = null;
    textTruncated = false;
    actualSize = 0;
    hexLines = null;
    revokeImage();
  }

  async function load(_path: string, _at: string, kind: string, _cap: number) {
    reset();
    if (kind === 'none' || kind === 'binary') {
      // Nothing to fetch — these kinds render metadata-only views.
      return;
    }
    loading = true;
    try {
      if (kind === 'image') {
        const r = await fetchBytesAsBlobUrl(path, byteParams);
        imageUrl = r.url;
        imageRevoke = r.revoke;
      } else if (kind === 'hex') {
        const resp = await fetchBytes(path, byteParams);
        const ab = await resp.arrayBuffer();
        actualSize = ab.byteLength;
        const slice = ab.byteLength > cap ? ab.slice(0, cap) : ab;
        textTruncated = ab.byteLength > cap;
        hexLines = renderHex(new Uint8Array(slice));
      } else {
        // text + markdown both need a UTF-8 decode.
        const resp = await fetchBytes(path, byteParams);
        const ab = await resp.arrayBuffer();
        actualSize = ab.byteLength;
        const slice = ab.byteLength > cap ? ab.slice(0, cap) : ab;
        textTruncated = ab.byteLength > cap;
        text = new TextDecoder('utf-8', { fatal: false }).decode(slice);
      }
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  function renderHex(bytes: Uint8Array): string {
    const lines: string[] = [];
    for (let i = 0; i < bytes.length; i += 16) {
      const slice = bytes.slice(i, i + 16);
      const hex = Array.from(slice)
        .map((b) => b.toString(16).padStart(2, '0'))
        .join(' ');
      const ascii = Array.from(slice)
        .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.'))
        .join('');
      lines.push(`${i.toString(16).padStart(8, '0')}  ${hex.padEnd(48, ' ')}  ${ascii}`);
    }
    return lines.join('\n');
  }

  $: textLines = text ? text.split('\n') : [];

  $: markdownHtml = (() => {
    if (render.kind !== 'markdown' || text === null) return '';
    const raw = marked.parse(text, { async: false }) as string;
    return DOMPurify.sanitize(raw);
  })();
</script>

<section class="file-renderer">
  {#if error}
    <div class="error-banner">{error}</div>
  {:else if loading}
    <div class="muted small-pad">loading file…</div>
  {:else if render.kind === 'none'}
    <div class="muted small-pad">no preview (schema declares <code>render.kind = "none"</code>).</div>
  {:else if render.kind === 'binary'}
    <div class="binary-cta">
      <div class="muted">Binary file — no inline preview.</div>
      <a class="btn" href={getBytesUrl(path, byteParams)} target="_blank" rel="noopener">
        download bytes
      </a>
    </div>
  {:else if render.kind === 'image' && imageUrl}
    <div class="image-frame">
      <img src={imageUrl} alt={path} />
    </div>
  {:else if render.kind === 'hex' && hexLines !== null}
    {#if textTruncated}
      <div class="trunc-banner">
        Showing first {formatBytes(cap)} of {formatBytes(actualSize)} —
        <a href={getBytesUrl(path, byteParams)} target="_blank" rel="noopener">download full file</a>
      </div>
    {/if}
    <pre class="hex-pre mono">{hexLines}</pre>
  {:else if render.kind === 'markdown' && text !== null}
    {#if textTruncated}
      <div class="trunc-banner">
        Showing first {formatBytes(cap)} of {formatBytes(actualSize)} —
        <a href={getBytesUrl(path, byteParams)} target="_blank" rel="noopener">download full file</a>
      </div>
    {/if}
    <article class="markdown-body">
      {@html markdownHtml}
    </article>
  {:else if render.kind === 'text' && text !== null}
    {#if textTruncated}
      <div class="trunc-banner">
        Showing first {formatBytes(cap)} of {formatBytes(actualSize)} —
        <a href={getBytesUrl(path, byteParams)} target="_blank" rel="noopener">download full file</a>
      </div>
    {/if}
    <pre class="code-pre mono"><ol class="code-lines">
        {#each textLines as line}<li>{line || ' '}</li>{/each}
      </ol></pre>
  {/if}
</section>

<style>
  .file-renderer {
    margin: 16px 0 24px;
  }
  .small-pad {
    padding: 12px 0;
  }
  .binary-cta {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 16px 18px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-soft);
  }
  .image-frame {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-soft);
    padding: 16px;
    display: flex;
    justify-content: center;
  }
  .image-frame img {
    max-width: 100%;
    max-height: 80vh;
    object-fit: contain;
  }
  .trunc-banner {
    background: var(--bg-soft);
    border: 1px solid var(--border);
    border-bottom: 0;
    padding: 8px 14px;
    font-size: 0.85rem;
    color: var(--fg-muted);
    border-radius: 8px 8px 0 0;
  }
  .hex-pre {
    margin: 0;
    padding: 14px 16px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-soft);
    overflow-x: auto;
    font-size: 0.8rem;
    line-height: 1.5;
    max-height: 70vh;
    overflow-y: auto;
    white-space: pre;
  }
  .code-pre {
    margin: 0;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-soft);
    overflow: auto;
    font-size: 0.85rem;
    line-height: 1.5;
    max-height: 70vh;
  }
  .code-lines {
    list-style: none;
    counter-reset: ln;
    margin: 0;
    padding: 12px 0;
  }
  .code-lines li {
    counter-increment: ln;
    display: grid;
    grid-template-columns: 56px 1fr;
    column-gap: 12px;
    padding: 0;
    white-space: pre;
  }
  .code-lines li::before {
    content: counter(ln);
    color: var(--fg-soft);
    text-align: right;
    padding-right: 8px;
    border-right: 1px solid var(--border);
    user-select: none;
  }
  .markdown-body {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elevated);
    padding: 24px 28px;
    line-height: 1.6;
  }
  .markdown-body :global(h1),
  .markdown-body :global(h2),
  .markdown-body :global(h3) {
    margin-top: 1.2em;
    margin-bottom: 0.4em;
  }
  .markdown-body :global(pre) {
    background: var(--bg-soft);
    border: 1px solid var(--border);
    padding: 12px 14px;
    border-radius: 6px;
    overflow-x: auto;
    font-size: 0.85rem;
  }
  .markdown-body :global(code) {
    font-family: var(--font-mono);
    font-size: 0.9em;
  }
  .markdown-body :global(a) {
    color: var(--accent);
  }
  .trunc-banner + .hex-pre,
  .trunc-banner + .code-pre {
    border-top-left-radius: 0;
    border-top-right-radius: 0;
  }
</style>

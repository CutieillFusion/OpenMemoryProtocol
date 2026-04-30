<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { page } from '$app/stores';
  import { base } from '$app/paths';
  import { getFile, patchFields, deleteFile, log } from '$lib/api';
  import { ApiError } from '$lib/api';
  import type { Manifest, FieldValue, CommitView, RenderHint, TreeEntry, BlobInfo } from '$lib/types';
  import {
    fieldKind,
    formatTimestamp,
    formatBytes,
    shortHash,
    relativeTime
  } from '$lib/format';
  import FileRenderer from '$lib/components/FileRenderer.svelte';
  import FileSidebar from '$lib/components/FileSidebar.svelte';

  $: filePath = $page.params.path ?? '';
  $: at = $page.url.searchParams.get('at') ?? '';

  let resp: Manifest | null = null;
  let blobInfo: { hash: string; size: number; render: RenderHint } | null = null;
  let renderHint: RenderHint = { kind: 'binary' };
  let loading = false;
  let error: string | null = null;
  let notFoundInTenant = false;
  let history: CommitView[] = [];
  /// True when the rendered content came from the staging index, not from
  /// HEAD. Surfaces a "staged" tag in the UI so the user knows the file
  /// hasn't been committed yet.
  let isStaged = false;

  // Field editor state.
  let editing: string | null = null;
  let editValue = '';
  let saving = false;

  async function load() {
    loading = true;
    error = null;
    resp = null;
    blobInfo = null;
    history = [];
    isStaged = false;
    try {
      let r: Manifest | TreeEntry[] | BlobInfo;
      try {
        r = await getFile(filePath, { at: at || undefined, verbose: true });
      } catch (e) {
        // If the file isn't committed yet but is sitting in the index
        // (e.g., just-uploaded or just-installed-from-marketplace), retry
        // against the staging index. `at` is meaningless when reading
        // staged, so drop it.
        if (e instanceof ApiError && e.status === 404 && !at) {
          r = await getFile(filePath, { verbose: true, staged: true });
          isStaged = true;
        } else {
          throw e;
        }
      }
      if (Array.isArray(r)) {
        // Path resolved to a tree — bounce the user back.
        resp = null;
      } else if ('kind' in r && r.kind === 'blob') {
        blobInfo = { hash: r.hash, size: r.size, render: r.render ?? { kind: 'binary' } };
        renderHint = blobInfo.render;
      } else {
        resp = r as Manifest;
        renderHint = resp.render ?? { kind: 'binary' };
      }
      try {
        history = await log({ path: filePath, max: 25, verbose: true });
      } catch {
        history = [];
      }
      await tick();
      openSectionFromHash();
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
      notFoundInTenant =
        e instanceof ApiError &&
        (e.status === 404 || e.code === 'not_found') &&
        !at;
    } finally {
      loading = false;
    }
  }

  // Path category for the friendly empty-state. `schemas/<file_type>/...`
  // and `probes/<ns>/<name>/...` are tenant-scoped — a fresh tenant won't
  // have them committed yet, and the file viewer should suggest the
  // marketplace flow rather than just surfacing the raw 404.
  $: pathKind = filePath.startsWith('schemas/')
    ? 'schema'
    : filePath.startsWith('probes/')
      ? 'probe'
      : 'file';

  function openSectionFromHash() {
    if (typeof window === 'undefined') return;
    const id = window.location.hash.replace(/^#/, '');
    if (!id) return;
    const el = document.getElementById(id);
    if (el && el.tagName.toLowerCase() === 'details') {
      (el as HTMLDetailsElement).open = true;
      el.scrollIntoView({ block: 'start' });
    }
  }

  function startEdit(name: string, v: FieldValue) {
    editing = name;
    if (v === null) editValue = 'null';
    else if (typeof v === 'string') editValue = v;
    else editValue = JSON.stringify(v);
  }

  async function saveEdit(name: string) {
    saving = true;
    try {
      let parsed: FieldValue;
      try {
        parsed = JSON.parse(editValue);
      } catch {
        parsed = editValue;
      }
      const m = await patchFields(filePath, { [name]: parsed });
      resp = { ...resp, ...m, fields: m.fields, render: resp?.render ?? m.render };
      editing = null;
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      saving = false;
    }
  }

  // Two-click delete: first click arms the button (visual flip), second
  // click within the timeout actually stages the deletion. Auto-disarms
  // after 3s of inactivity so a stray click doesn't sit primed.
  let deleteArmed = false;
  let deletePending = false;
  let disarmTimer: ReturnType<typeof setTimeout> | null = null;

  function disarmDelete() {
    deleteArmed = false;
    if (disarmTimer) {
      clearTimeout(disarmTimer);
      disarmTimer = null;
    }
  }

  async function onDeleteClick() {
    if (!deleteArmed) {
      deleteArmed = true;
      if (disarmTimer) clearTimeout(disarmTimer);
      disarmTimer = setTimeout(disarmDelete, 3000);
      return;
    }
    if (deletePending) return;
    deletePending = true;
    try {
      await deleteFile(filePath);
      alert(`Deletion of ${filePath} staged. Commit at /commit to make it durable.`);
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      deletePending = false;
      disarmDelete();
    }
  }

  function pathCrumbs(p: string): { label: string; path: string }[] {
    if (!p) return [{ label: 'root', path: '' }];
    const parts = p.split('/').filter(Boolean);
    const out: { label: string; path: string }[] = [{ label: 'root', path: '' }];
    let acc = '';
    for (const part of parts) {
      acc = acc ? `${acc}/${part}` : part;
      out.push({ label: part, path: acc });
    }
    return out;
  }

  $: crumbs = pathCrumbs(filePath);
  $: filePath, at, load();
  onMount(load);
</script>

<div class="file-page-grid" class:meta-open={!!resp}>
  <FileSidebar currentPath={filePath} {at} />

  <main class="file-main">
  <nav class="breadcrumb mono">
    {#each crumbs as c, i}
      {#if i > 0}<span class="bc-sep">/</span>{/if}
      {#if i === 0}
        <a href="{base}/" class="bc-link">{c.label}</a>
      {:else if i === crumbs.length - 1}
        <span class="bc-current">{c.label}</span>
      {:else}
        <a href="{base}/?path={encodeURIComponent(c.path)}" class="bc-link">{c.label}</a>
      {/if}
    {/each}
  </nav>

  {#if at}
    <div class="page-sub mono">at {at}</div>
  {/if}

  {#if loading}
    <div class="muted">loading…</div>
  {/if}

  {#if error}
    {#if notFoundInTenant && pathKind === 'schema'}
      <div class="empty-state">
        <h2>Schema not in this tenant</h2>
        <p class="muted">
          <code class="mono">{filePath}</code> isn't committed in your
          tenant's tree yet. Each tenant has its own schemas — what you saw
          on a different account doesn't carry over.
        </p>
        <p class="muted">
          Browse available schemas on the
          <a class="link" href="{base}/schema-marketplace">schema marketplace</a>,
          or commit one yourself by uploading TOML at
          <code class="mono">schemas/&lt;file_type&gt;/schema.toml</code>.
        </p>
      </div>
    {:else if notFoundInTenant && pathKind === 'probe'}
      <div class="empty-state">
        <h2>Probe not in this tenant</h2>
        <p class="muted">
          <code class="mono">{filePath}</code> isn't committed in your
          tenant's tree yet. Probes are tenant-scoped — install one from the
          <a class="link" href="{base}/marketplace">probe marketplace</a> or
          build your own at
          <a class="link" href="{base}/probes/build">/probes/build</a>.
        </p>
      </div>
    {:else}
      <div class="error-banner">{error}</div>
    {/if}
  {/if}

  {#if resp || blobInfo}
    <div class="meta-bar mono">
      {#if isStaged}
        <span class="tag tag--accent" title="This file is in the staging index but has not been committed yet. Visit /commit to make it durable.">staged</span>
        <span class="meta-dot">·</span>
      {/if}
      {#if resp}
        <span class="tag tag--accent">{resp.file_type}</span>
        <span class="meta-dot">·</span>
        <span title="ingested {formatTimestamp(resp.ingested_at)}">ingested {relativeTime(resp.ingested_at)}</span>
        {#if resp.source_hash}
          <span class="meta-dot">·</span>
          <span class="soft">blob {shortHash(resp.source_hash, 10)}</span>
        {/if}
        {#if resp.schema_hash}
          <span class="meta-dot">·</span>
          <span class="soft">schema {shortHash(resp.schema_hash, 10)}</span>
        {/if}
      {/if}
      {#if blobInfo}
        <span class="tag">blob</span>
        <span class="meta-dot">·</span>
        <span>{formatBytes(blobInfo.size)}</span>
        <span class="meta-dot">·</span>
        <span class="soft">{shortHash(blobInfo.hash, 12)}</span>
      {/if}
      <span class="meta-spacer"></span>
      <span class="soft small">render: {renderHint.kind}</span>
    </div>
  {/if}

  {#if history.length > 0}
    {@const c = history[0]}
    <div class="commit-banner">
      <div class="cb-main">
        <span class="cb-message">{c.message}</span>
        <span class="cb-meta soft">
          <span class="cb-author">{c.author}</span>
          authored
          <span title={formatTimestamp(c.timestamp)}>{relativeTime(c.timestamp)}</span>
        </span>
      </div>
      <div class="cb-actions">
        <a href="{base}/log#{c.hash}" class="cb-hash mono">{shortHash(c.hash, 10)}</a>
        <a href="{base}/log" class="btn btn--ghost btn--sm">History</a>
        {#if resp || blobInfo}
          <button
            type="button"
            class="trash-btn"
            class:armed={deleteArmed}
            on:click={onDeleteClick}
            disabled={deletePending}
            aria-label={deleteArmed ? 'Click again to confirm delete' : 'Stage delete'}
            title={deleteArmed ? 'Click again to confirm — auto-cancels in 3s' : 'Stage delete (click twice to confirm)'}
          >
            <svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor" aria-hidden="true">
              <path d="M6 2a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v1h3.5a.5.5 0 0 1 0 1H13l-.7 9.1A2 2 0 0 1 10.3 15H5.7a2 2 0 0 1-2-1.9L3 4h-.5a.5.5 0 0 1 0-1H6V2zm1 0v1h2V2H7zM4.5 5l.7 8.4a1 1 0 0 0 1 .9h3.6a1 1 0 0 0 1-.9L11.5 5h-7zM7 6.5a.5.5 0 0 1 .5.5v5a.5.5 0 0 1-1 0V7a.5.5 0 0 1 .5-.5zm2.5.5a.5.5 0 0 0-1 0v5a.5.5 0 0 0 1 0V7z"/>
            </svg>
          </button>
        {/if}
      </div>
    </div>
  {/if}

  {#if (resp || blobInfo) && filePath}
    <FileRenderer path={filePath} {at} render={renderHint} staged={isStaged} />
  {/if}

  </main>

  {#if resp}
    <aside class="meta-panel">
      <div class="meta-panel-header">
        <span class="meta-panel-title">Manifest</span>
      </div>

      <section class="meta-panel-section">
        <table class="table table--meta">
          <tbody>
            <tr><th>file_type</th><td class="mono">{resp.file_type}</td></tr>
            <tr><th>ingested_at</th><td>{formatTimestamp(resp.ingested_at)}</td></tr>
            {#if resp.ingester_version}
              <tr><th>ingester_version</th><td class="mono">{resp.ingester_version}</td></tr>
            {/if}
            {#if resp.source_hash}
              <tr><th>source_hash</th><td class="mono break">{resp.source_hash}</td></tr>
            {/if}
            {#if resp.schema_hash}
              <tr><th>schema_hash</th><td class="mono break">{resp.schema_hash}</td></tr>
            {/if}
          </tbody>
        </table>
      </section>

      <section class="meta-panel-section">
        <h3 class="meta-panel-subtitle">Fields ({Object.keys(resp.fields).length})</h3>
        {#if Object.keys(resp.fields).length === 0}
          <div class="soft small-pad">no fields</div>
        {:else}
          <ul class="field-list">
            {#each Object.entries(resp.fields) as [name, value]}
              <li class="field-item">
                <div class="field-row">
                  <span class="mono field-name">{name}</span>
                  <span class="tag field-tag">{fieldKind(value)}</span>
                </div>
                {#if editing === name}
                  <textarea class="textarea field-textarea" bind:value={editValue} rows="3"></textarea>
                  <div class="field-actions">
                    <button class="btn btn--sm btn--primary" on:click={() => saveEdit(name)} disabled={saving}>save</button>
                    <button class="btn btn--sm btn--ghost" on:click={() => (editing = null)} disabled={saving}>cancel</button>
                  </div>
                {:else}
                  <div class="field-value mono">
                    {fieldKind(value) === 'string' ? value : JSON.stringify(value)}
                  </div>
                  <div class="field-actions">
                    <button class="btn btn--sm btn--ghost" on:click={() => startEdit(name, value)}>edit</button>
                  </div>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
      </section>
    </aside>
  {/if}
</div>

<style>
  .empty-state {
    max-width: 640px;
    margin: 16px 0 24px;
    padding: 16px 20px;
    border: 1px dashed var(--border);
    border-radius: 8px;
    background: var(--bg-elevated);
  }
  .empty-state h2 {
    margin: 0 0 8px;
    font-size: 1rem;
  }
  .empty-state p {
    margin: 4px 0;
    font-size: 0.9rem;
  }
  .file-page-grid {
    padding: 24px 32px 64px calc(clamp(240px, 22vw, 320px) + 32px);
    transition: padding-right 0.15s ease;
  }
  .file-page-grid.meta-open {
    padding-right: calc(clamp(320px, 30vw, 420px) + 32px);
  }
  .file-main {
    min-width: 0;
    max-width: var(--max-width-wide);
  }
  @media (max-width: 900px) {
    .file-page-grid,
    .file-page-grid.meta-open {
      padding-left: 24px;
      padding-right: 24px;
    }
  }
  .meta-toggle.active {
    background: rgba(39, 71, 212, 0.08) !important;
    color: var(--accent) !important;
    border-color: rgba(39, 71, 212, 0.4) !important;
  }
  .meta-panel {
    position: fixed;
    top: 80px;
    right: 0;
    bottom: 16px;
    width: clamp(320px, 30vw, 420px);
    border: 1px solid var(--border);
    border-right: 0;
    border-radius: 8px 0 0 8px;
    background: var(--bg-elevated);
    box-shadow: -4px 0 16px rgba(17, 17, 17, 0.05);
    overflow-y: auto;
    z-index: 50;
  }
  @media (max-width: 900px) {
    .meta-panel {
      width: 100vw;
      border-radius: 0;
      border-left: 0;
    }
  }
  .meta-panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }
  .meta-panel-title {
    font-weight: 600;
    font-size: 0.95rem;
  }
  .meta-panel-close {
    background: none;
    border: 0;
    cursor: pointer;
    color: var(--fg-soft);
    font-size: 1.4rem;
    line-height: 1;
    padding: 0 4px;
  }
  .meta-panel-close:hover {
    color: var(--fg);
  }
  .meta-panel-section {
    padding: 12px 16px 16px;
    border-bottom: 1px solid var(--border);
  }
  .meta-panel-section:last-child {
    border-bottom: 0;
  }
  .meta-panel-subtitle {
    margin: 0 0 8px;
    font-size: 0.85rem;
    font-weight: 600;
    color: var(--fg-muted);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .table--meta th,
  .table--meta td {
    padding: 6px 8px;
    font-size: 0.8rem;
  }
  .table--meta th {
    background: transparent;
    text-transform: none;
    letter-spacing: 0;
    font-weight: 500;
    color: var(--fg-soft);
    width: 35%;
  }
  .table--meta .break {
    word-break: break-all;
  }
  .small-pad {
    padding: 8px 0;
    font-size: 0.85rem;
  }
  .field-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .field-item {
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 10px;
    background: var(--bg-soft);
  }
  .field-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 4px;
  }
  .field-name {
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--fg);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .field-tag {
    flex-shrink: 0;
    font-size: 0.65rem;
  }
  .field-value {
    font-size: 0.78rem;
    color: var(--fg-muted);
    word-break: break-all;
    margin-bottom: 4px;
  }
  .field-textarea {
    width: 100%;
    margin-bottom: 6px;
    font-size: 0.78rem;
  }
  .field-actions {
    display: flex;
    gap: 4px;
    justify-content: flex-end;
  }
  .commit-banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 12px 16px;
    margin: 12px 0 0;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elevated);
    font-size: 0.9rem;
  }
  .cb-main {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1;
  }
  .cb-message {
    font-weight: 500;
    color: var(--fg);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .cb-meta {
    font-size: 0.8rem;
  }
  .cb-author {
    color: var(--fg-muted);
    font-weight: 500;
  }
  .cb-actions {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-shrink: 0;
  }
  .cb-hash {
    background: var(--bg-soft);
    border: 1px solid var(--border);
    padding: 2px 8px;
    border-radius: 4px;
    font-size: 0.8rem;
    color: var(--fg-muted);
    text-decoration: none;
  }
  .cb-hash:hover {
    color: var(--accent);
    border-color: var(--accent);
  }
  .trash-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    padding: 0;
    border: 1px solid rgba(179, 38, 30, 0.25);
    border-radius: 6px;
    background: var(--bg-elevated);
    color: var(--danger);
    cursor: pointer;
    transition: background 0.12s, color 0.12s, border-color 0.12s, transform 0.05s;
  }
  .trash-btn:hover:not(:disabled) {
    background: rgba(179, 38, 30, 0.06);
    border-color: rgba(179, 38, 30, 0.5);
  }
  .trash-btn.armed {
    background: var(--danger) !important;
    color: #ffffff !important;
    border-color: var(--danger) !important;
    box-shadow: 0 0 0 2px rgba(179, 38, 30, 0.2) !important;
  }
  .trash-btn.armed:hover {
    background: #8a1d18 !important;
  }
  .trash-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .breadcrumb {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-wrap: wrap;
    margin: 8px 0 4px;
    font-size: 0.95rem;
  }
  .bc-link {
    color: var(--accent);
    text-decoration: none;
    padding: 2px 4px;
    border-radius: 4px;
  }
  .bc-link:hover {
    text-decoration: underline;
  }
  .bc-sep {
    color: var(--fg-soft);
    padding: 0 2px;
  }
  .bc-current {
    color: var(--fg);
    padding: 2px 4px;
    word-break: break-all;
  }
  .meta-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin: 12px 0 0;
    padding: 10px 14px;
    background: var(--bg-soft);
    border: 1px solid var(--border);
    border-radius: 8px;
    font-size: 0.85rem;
  }
  .meta-dot {
    color: var(--fg-soft);
  }
  .meta-spacer {
    flex: 1;
  }
  .small {
    font-size: 0.8rem;
  }
  .section {
    margin-top: 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elevated);
  }
  .section > summary {
    padding: 10px 16px;
    cursor: pointer;
    font-weight: 600;
    list-style: none;
  }
  .section > summary::-webkit-details-marker {
    display: none;
  }
  .section > summary::before {
    content: '▸ ';
    color: var(--fg-soft);
    display: inline-block;
    width: 16px;
  }
  .section[open] > summary::before {
    content: '▾ ';
  }
  .section > :global(*:not(summary)) {
    padding: 0 16px 14px;
  }
  .section > :global(table.table) {
    margin: 4px 0 14px;
  }
  .btn--sm {
    padding: 4px 10px;
    font-size: 0.8rem;
  }
</style>

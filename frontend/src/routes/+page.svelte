<script lang="ts">
  import { onMount } from 'svelte';
  import { base } from '$app/paths';
  import { page } from '$app/stores';
  import { goto } from '$app/navigation';
  import { getTree, listBranches, status, log } from '$lib/api';
  import type { TreeEntry, BranchInfo, RepoStatus, CommitView } from '$lib/types';
  import { ApiError } from '$lib/api';
  import { shortHash, relativeTime, formatTimestamp } from '$lib/format';

  let path = '';
  let at: string = '';
  let entries: TreeEntry[] = [];
  let branches: BranchInfo[] = [];
  let repoStatus: RepoStatus | null = null;
  let latestCommit: CommitView | null = null;
  let filter = '';
  let loading = false;
  let error: string | null = null;

  // TODO(last-commit): GitLab shows the most-recent commit message + age per
  // entry. Computing that here means N parallel `log({path, max:1})` calls
  // per directory load — fine on small repos, slow on big ones. Deferred
  // until the backend exposes an enriched-tree endpoint.

  // Filter by name (case-insensitive substring), then sort: trees first
  // (alphabetical), then files (alphabetical).
  $: sortedEntries = (() => {
    const f = filter.trim().toLowerCase();
    const base = f
      ? entries.filter((e) => e.name.toLowerCase().includes(f))
      : entries;
    return [...base].sort((a, b) => {
      const aIsTree = a.mode === 'tree';
      const bIsTree = b.mode === 'tree';
      if (aIsTree !== bIsTree) return aIsTree ? -1 : 1;
      return a.name.localeCompare(b.name);
    });
  })();

  async function load() {
    loading = true;
    error = null;
    try {
      const [tree, branchList, st, recent] = await Promise.all([
        getTree(path, { at: at || undefined, verbose: true }),
        listBranches().catch(() => [] as BranchInfo[]),
        status().catch(() => null),
        log({ max: 1, verbose: true }).catch(() => [] as CommitView[])
      ]);
      entries = tree;
      branches = branchList;
      repoStatus = st;
      latestCommit = recent[0] ?? null;
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
      entries = [];
    } finally {
      loading = false;
    }
  }

  function syncFromUrl() {
    const qp = $page.url.searchParams.get('path') ?? '';
    const qa = $page.url.searchParams.get('at') ?? '';
    if (qp !== path || qa !== at) {
      path = qp;
      at = qa;
    }
  }

  function navigateTo(newPath: string) {
    const sp = new URLSearchParams();
    if (newPath) sp.set('path', newPath);
    if (at) sp.set('at', at);
    const qs = sp.toString();
    goto(`${base}/${qs ? `?${qs}` : ''}`, { keepFocus: true, noScroll: true });
  }

  // React to URL changes (back/forward) and reload.
  $: $page.url, syncFromUrl();
  $: path, at, load();

  onMount(() => {
    syncFromUrl();
    load();
  });

  function gotoPath(name: string) {
    return path ? `${path}/${name}` : name;
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

  function fileHref(name: string): string {
    const sp = new URLSearchParams();
    if (at) sp.set('at', at);
    const qs = sp.toString();
    return `${base}/file/${gotoPath(name)}${qs ? `?${qs}` : ''}`;
  }

  // Inline SVGs to avoid an icon dep. Sized to inherit currentColor.
  const iconFolder = `<svg viewBox="0 0 16 16" width="16" height="16" fill="currentColor" aria-hidden="true">
    <path d="M1 3a1 1 0 0 1 1-1h4l1.5 1.5H14a1 1 0 0 1 1 1V13a1 1 0 0 1-1 1H2a1 1 0 0 1-1-1V3z"/>
  </svg>`;
  const iconFolderOpen = `<svg viewBox="0 0 16 16" width="16" height="16" fill="currentColor" aria-hidden="true">
    <path d="M1 4a1 1 0 0 1 1-1h4l1.5 1.5H14a1 1 0 0 1 1 1H1V4z"/>
    <path d="M1 5h14l-1.5 7.5a1 1 0 0 1-1 .5H2.5a1 1 0 0 1-1-.5L1 5z"/>
  </svg>`;
  const iconFile = `<svg viewBox="0 0 16 16" width="16" height="16" fill="currentColor" aria-hidden="true">
    <path d="M3 1a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1V5l-4-4H3z" opacity="0.85"/>
    <path d="M9 1v3a1 1 0 0 0 1 1h3" fill="var(--bg-elevated)"/>
  </svg>`;
</script>

<main class="page-container page-container--wide">
  <div class="repo-header">
    <div class="repo-title">
      <h1 class="page-title">Tree</h1>
      {#if repoStatus?.head}
        <div class="repo-sub mono soft">
          HEAD <span class="hash-chip">{shortHash(repoStatus.head, 12)}</span>
          {#if repoStatus.branch}<span> on </span><span class="hash-chip">{repoStatus.branch}</span>{/if}
        </div>
      {/if}
    </div>

    <div class="ref-bar">
      {#if branches.length > 0}
        <select
          class="select"
          aria-label="branch"
          bind:value={at}
          on:change={load}
        >
          <option value="">— at HEAD —</option>
          {#each branches as b}
            <option value={b.name}>{b.name}{b.is_current ? ' (current)' : ''}</option>
          {/each}
        </select>
      {/if}
      <input
        class="input mono ref-input"
        placeholder="branch / hash / HEAD~n"
        bind:value={at}
        on:change={load}
      />
      <button class="btn btn--ghost" on:click={load} disabled={loading}>
        {loading ? '…' : 'reload'}
      </button>
    </div>
  </div>

  <nav class="crumbs">
    {#each pathCrumbs(path) as c, i}
      {#if i > 0}<span class="crumb-sep">/</span>{/if}
      <button class="crumb-link" on:click={() => navigateTo(c.path)}>
        {c.label}
      </button>
    {/each}
  </nav>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}

  {#if latestCommit}
    {@const c = latestCommit}
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
      </div>
    </div>
  {/if}

  <div class="tree-toolbar">
    <input
      type="search"
      class="find-file mono"
      placeholder="Find file…"
      bind:value={filter}
      aria-label="filter entries"
    />
  </div>

  <div class="tree-card">
    <table class="tree-table">
      <tbody>
        {#if sortedEntries.length === 0 && !loading}
          <tr><td class="soft empty-row" colspan="2">empty</td></tr>
        {/if}
        {#if path !== ''}
          <tr class="tree-row">
            <td class="name-cell">
              <button class="entry-link" on:click={() => {
                const parent = path.split('/').slice(0, -1).join('/');
                navigateTo(parent);
              }}>
                <span class="icon">{@html iconFolderOpen}</span>
                <span class="up-name">..</span>
              </button>
            </td>
            <td class="hash-cell"></td>
          </tr>
        {/if}
        {#each sortedEntries as e}
          <tr class="tree-row">
            <td class="name-cell">
              {#if e.mode === 'tree'}
                <button class="entry-link" on:click={() => navigateTo(gotoPath(e.name))}>
                  <span class="icon icon--folder">{@html iconFolder}</span>
                  <span class="entry-name">{e.name}</span>
                </button>
              {:else}
                <a class="entry-link" href={fileHref(e.name)}>
                  <span class="icon icon--file">{@html iconFile}</span>
                  <span class="entry-name">{e.name}</span>
                </a>
              {/if}
            </td>
            <td class="hash-cell mono soft">{shortHash(e.hash, 10)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>

  {#if repoStatus && repoStatus.staged.length > 0}
    <section class="page-section">
      <h2>Staged ({repoStatus.staged.length})</h2>
      <div class="muted text-sm">
        Uncommitted changes. Visit <a href="{base}/commit"><span style="color: var(--accent); text-decoration: underline;">/commit</span></a> to author a commit.
      </div>
      <ul class="staged-list">
        {#each repoStatus.staged as s}
          <li>
            <span class="tag {s.kind === 'remove' ? 'tag--danger' : 'tag--accent'}">{s.kind}</span>
            <span class="mono">{s.path}</span>
          </li>
        {/each}
      </ul>
    </section>
  {/if}
</main>

<style>
  .repo-header {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 16px;
    flex-wrap: wrap;
    margin-bottom: 12px;
  }
  .repo-title h1 {
    margin: 0;
  }
  .repo-sub {
    margin-top: 4px;
    font-size: 0.85rem;
  }
  .hash-chip {
    background: var(--bg-soft);
    border: 1px solid var(--border);
    padding: 1px 6px;
    border-radius: 4px;
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--fg-muted);
  }
  .ref-bar {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .ref-input {
    width: 220px;
    padding: 6px 10px;
    font-size: 0.85rem;
  }
  .crumbs {
    display: flex;
    align-items: center;
    gap: 4px;
    margin: 8px 0 12px;
    font-family: var(--font-mono);
    font-size: 0.9rem;
    flex-wrap: wrap;
  }
  .crumb-sep {
    color: var(--fg-soft);
  }
  .crumb-link {
    border: 0;
    background: none;
    color: var(--accent);
    cursor: pointer;
    padding: 2px 4px;
    font-family: inherit;
    font-size: inherit;
  }
  .crumb-link:hover {
    text-decoration: underline;
  }
  .commit-banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 12px 16px;
    margin: 12px 0 12px;
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
  .tree-toolbar {
    margin-bottom: 8px;
  }
  .find-file {
    width: 320px;
    max-width: 100%;
    padding: 6px 12px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-elevated);
    font-size: 0.85rem;
  }
  .find-file:focus {
    outline: none;
    border-color: var(--accent);
  }
  .tree-card {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elevated);
    overflow: hidden;
  }
  .tree-table {
    width: 100%;
    border-collapse: collapse;
  }
  .tree-row {
    border-bottom: 1px solid var(--border);
  }
  .tree-row:last-child {
    border-bottom: 0;
  }
  .tree-row:hover {
    background: var(--bg-soft);
  }
  .name-cell {
    padding: 8px 14px;
  }
  .hash-cell {
    padding: 8px 14px;
    font-size: 0.8rem;
    text-align: right;
    white-space: nowrap;
    width: 1%;
  }
  .empty-row {
    padding: 24px;
    text-align: center;
  }
  .entry-link {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    border: 0;
    background: none;
    color: var(--fg);
    cursor: pointer;
    padding: 0;
    text-align: left;
    font-family: var(--font-body);
    font-size: 0.95rem;
    text-decoration: none;
  }
  .entry-link:hover .entry-name,
  .entry-link:hover .up-name {
    color: var(--accent);
    text-decoration: underline;
  }
  .icon {
    display: inline-flex;
    width: 16px;
    height: 16px;
  }
  .icon--folder {
    color: #c9a02e;
  }
  .icon--file {
    color: #5e7bc7;
  }
  .up-name {
    color: var(--fg-muted);
    font-family: var(--font-mono);
  }
  .staged-list {
    list-style: none;
    padding: 0;
    margin: 0;
  }
  .staged-list li {
    padding: 8px 0;
    border-bottom: 1px solid var(--border);
    display: flex;
    gap: 12px;
    align-items: center;
  }
  .staged-list li:last-child {
    border-bottom: 0;
  }
</style>

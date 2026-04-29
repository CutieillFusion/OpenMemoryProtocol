<script lang="ts">
  import { base } from '$app/paths';
  import { getTree, ApiError } from '$lib/api';
  import type { TreeEntry } from '$lib/types';

  /** Currently-open file path (used for highlight + auto-expanding ancestors). */
  export let currentPath: string;
  /** Optional `?at=` ref to thread through navigation links. */
  export let at: string = '';

  const MAX_ENTRIES = 1000;

  let entries: TreeEntry[] = [];
  let loading = false;
  let error: string | null = null;
  let filter = '';
  let openFolders: Set<string> = new Set();

  type FolderNode = {
    kind: 'folder';
    name: string;
    path: string;
    children: TreeNode[];
  };
  type FileNode = {
    kind: 'file';
    name: string;
    path: string;
  };
  type TreeNode = FolderNode | FileNode;

  $: void load(at);

  // Re-open ancestors of the current file whenever the route changes.
  $: openAncestorsOf(currentPath);

  /** True after a successful load that came from the staging index, not
   * from HEAD. Surfaces a "showing staged" hint at the sidebar header so
   * the user knows the listing isn't durable yet. */
  let showingStaged = false;

  async function load(_at: string) {
    loading = true;
    error = null;
    try {
      const tree = await getTree('', {
        at: _at || undefined,
        recursive: true,
        verbose: true
      });
      entries = tree.filter((e) => e.mode === 'manifest' || e.mode === 'blob');
      showingStaged = false;
    } catch (e) {
      // No commits yet → fall back to listing the staging index, so the
      // sidebar still shows files the user has uploaded or installed but
      // not committed. Same fallback for `not_found`/`no commits` shapes.
      const isNoCommits =
        e instanceof ApiError &&
        ((e.status === 404 && /no commits/i.test(e.message)) ||
          e.code === 'not_found');
      if (isNoCommits && !_at) {
        try {
          const tree = await getTree('', { staged: true, verbose: true });
          entries = tree.filter((e) => e.mode === 'manifest' || e.mode === 'blob');
          showingStaged = true;
          error = null;
        } catch (e2) {
          error = e2 instanceof ApiError ? `${e2.code}: ${e2.message}` : String(e2);
          entries = [];
          showingStaged = false;
        }
      } else {
        error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
        entries = [];
        showingStaged = false;
      }
    } finally {
      loading = false;
    }
  }

  function openAncestorsOf(p: string) {
    if (!p) return;
    const parts = p.split('/').filter(Boolean);
    if (parts.length < 2) return;
    const next = new Set(openFolders);
    let acc = '';
    for (let i = 0; i < parts.length - 1; i++) {
      acc = acc ? `${acc}/${parts[i]}` : parts[i];
      next.add(acc);
    }
    openFolders = next;
  }

  function toggleFolder(path: string) {
    const next = new Set(openFolders);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    openFolders = next;
  }

  function buildTree(paths: TreeEntry[]): TreeNode[] {
    const root: FolderNode = { kind: 'folder', name: '', path: '', children: [] };
    for (const e of paths) {
      const parts = e.name.split('/').filter(Boolean);
      let cursor = root;
      for (let i = 0; i < parts.length - 1; i++) {
        const folderName = parts[i];
        const folderPath = parts.slice(0, i + 1).join('/');
        let next = cursor.children.find(
          (c): c is FolderNode => c.kind === 'folder' && c.name === folderName
        );
        if (!next) {
          next = { kind: 'folder', name: folderName, path: folderPath, children: [] };
          cursor.children.push(next);
        }
        cursor = next;
      }
      cursor.children.push({
        kind: 'file',
        name: parts[parts.length - 1] ?? e.name,
        path: e.name
      });
    }
    sortTree(root);
    return root.children;
  }

  function sortTree(node: FolderNode): void {
    node.children.sort((a, b) => {
      if (a.kind !== b.kind) return a.kind === 'folder' ? -1 : 1;
      return a.name.localeCompare(b.name);
    });
    for (const c of node.children) {
      if (c.kind === 'folder') sortTree(c);
    }
  }

  function flatten(
    nodes: TreeNode[],
    open: Set<string>,
    depth = 0,
    out: { node: TreeNode; depth: number }[] = []
  ): { node: TreeNode; depth: number }[] {
    for (const n of nodes) {
      out.push({ node: n, depth });
      if (n.kind === 'folder' && open.has(n.path)) {
        flatten(n.children, open, depth + 1, out);
      }
    }
    return out;
  }

  function fileHref(path: string): string {
    const sp = new URLSearchParams();
    if (at) sp.set('at', at);
    const qs = sp.toString();
    return `${base}/file/${path}${qs ? `?${qs}` : ''}`;
  }

  function basename(path: string): string {
    const parts = path.split('/');
    return parts[parts.length - 1] ?? path;
  }

  function dirname(path: string): string {
    const idx = path.lastIndexOf('/');
    return idx === -1 ? '' : path.slice(0, idx);
  }

  $: tree = buildTree(entries);
  $: visible = flatten(tree, openFolders);
  $: searching = filter.trim().length > 0;

  // Filtered flat list when the user is typing.
  $: filtered = (() => {
    const f = filter.trim().toLowerCase();
    if (!f) return [] as TreeEntry[];
    return entries
      .filter((e) => e.name.toLowerCase().includes(f))
      .slice(0, MAX_ENTRIES);
  })();

  $: truncated = entries.length > MAX_ENTRIES && !searching;
</script>

<aside class="file-sidebar">
  <div class="sidebar-header">
    <div class="sidebar-title">Files</div>
    {#if showingStaged}
      <span class="tag tag--accent" title="Listing the staging index — these paths are uploaded/installed but not committed yet.">staged</span>
    {/if}
  </div>
  <div class="sidebar-search">
    <input
      type="search"
      class="search-input mono"
      placeholder="Search files…"
      bind:value={filter}
      aria-label="filter files"
    />
  </div>
  <div class="sidebar-body">
    {#if loading && entries.length === 0}
      <div class="muted small">loading…</div>
    {:else if error}
      <div class="error-banner small">{error}</div>
    {:else if searching}
      {#if filtered.length === 0}
        <div class="muted small">no matches</div>
      {:else}
        <ul class="tree-list">
          {#each filtered as e (e.name)}
            {@const active = e.name === currentPath}
            <li class="row" class:active>
              <a
                href={fileHref(e.name)}
                class="file-link"
                aria-current={active ? 'page' : undefined}
                style="padding-left: 14px;"
              >
                <span class="file-icon" aria-hidden="true">
                  <svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor">
                    <path d="M3 1a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1V5l-4-4H3z" opacity="0.85"/>
                    <path d="M9 1v3a1 1 0 0 0 1 1h3" fill="var(--bg-elevated)"/>
                  </svg>
                </span>
                <span class="file-text">
                  <span class="file-name">{basename(e.name)}</span>
                  {#if dirname(e.name)}
                    <span class="file-dir mono">{dirname(e.name)}/</span>
                  {/if}
                </span>
              </a>
            </li>
          {/each}
        </ul>
      {/if}
    {:else if visible.length === 0}
      <div class="muted small">empty</div>
    {:else}
      <ul class="tree-list">
        {#each visible as { node, depth } (node.path)}
          {#if node.kind === 'folder'}
            {@const open = openFolders.has(node.path)}
            <li class="row">
              <button
                class="folder-link"
                style="padding-left: {14 + depth * 14}px;"
                on:click={() => toggleFolder(node.path)}
                aria-expanded={open}
              >
                <span class="chevron" class:open aria-hidden="true">▸</span>
                <span class="folder-icon" aria-hidden="true">
                  <svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor">
                    <path d="M1 3a1 1 0 0 1 1-1h4l1.5 1.5H14a1 1 0 0 1 1 1V13a1 1 0 0 1-1 1H2a1 1 0 0 1-1-1V3z"/>
                  </svg>
                </span>
                <span class="folder-name">{node.name}</span>
              </button>
            </li>
          {:else}
            {@const active = node.path === currentPath}
            <li class="row" class:active>
              <a
                href={fileHref(node.path)}
                class="file-link"
                style="padding-left: {14 + depth * 14}px;"
                aria-current={active ? 'page' : undefined}
              >
                <span class="file-icon" aria-hidden="true">
                  <svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor">
                    <path d="M3 1a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1V5l-4-4H3z" opacity="0.85"/>
                    <path d="M9 1v3a1 1 0 0 0 1 1h3" fill="var(--bg-elevated)"/>
                  </svg>
                </span>
                <span class="file-name">{node.name}</span>
              </a>
            </li>
          {/if}
        {/each}
      </ul>
      {#if truncated}
        <div class="muted small trunc">
          Showing first {MAX_ENTRIES} — refine your search.
        </div>
      {/if}
    {/if}
  </div>
</aside>

<style>
  .file-sidebar {
    display: flex;
    flex-direction: column;
    border: 1px solid var(--border);
    border-left: 0;
    border-radius: 0 8px 8px 0;
    background: var(--bg-elevated);
    box-shadow: 4px 0 16px rgba(17, 17, 17, 0.05);
    overflow: hidden;
    position: fixed;
    top: 80px;
    bottom: 16px;
    left: 0;
    width: clamp(240px, 22vw, 320px);
    z-index: 40;
  }
  @media (max-width: 900px) {
    .file-sidebar {
      display: none;
    }
  }
  .sidebar-header {
    padding: 12px 14px 8px;
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .sidebar-title {
    font-weight: 600;
    font-size: 0.95rem;
  }
  .sidebar-search {
    padding: 10px 12px;
    border-bottom: 1px solid var(--border);
    background: var(--bg-soft);
  }
  .search-input {
    width: 100%;
    padding: 6px 10px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-elevated);
    font-size: 0.8rem;
  }
  .search-input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .sidebar-body {
    flex: 1;
    overflow-y: auto;
    padding: 4px 0;
  }
  .small {
    font-size: 0.85rem;
    padding: 12px 14px;
  }
  .tree-list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .row {
    margin: 0;
  }
  .file-link,
  .folder-link {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 5px 14px;
    text-decoration: none;
    color: var(--fg);
    font-size: 0.85rem;
    border-left: 2px solid transparent;
    width: 100%;
    text-align: left;
    background: none;
    border-top: 0;
    border-right: 0;
    border-bottom: 0;
    font-family: inherit;
    cursor: pointer;
  }
  .file-link:hover,
  .folder-link:hover {
    background: var(--bg-soft);
  }
  .row.active .file-link {
    background: rgba(39, 71, 212, 0.08);
    border-left-color: var(--accent);
    color: var(--accent);
  }
  .file-icon {
    display: inline-flex;
    color: #5e7bc7;
    flex-shrink: 0;
  }
  .row.active .file-icon {
    color: var(--accent);
  }
  .folder-icon {
    display: inline-flex;
    color: #c9a02e;
    flex-shrink: 0;
  }
  .chevron {
    display: inline-block;
    width: 10px;
    color: var(--fg-soft);
    transform: rotate(0deg);
    transition: transform 0.1s;
    font-size: 0.7rem;
    flex-shrink: 0;
  }
  .chevron.open {
    transform: rotate(90deg);
  }
  .folder-name {
    font-weight: 500;
  }
  .file-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .file-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
    line-height: 1.3;
  }
  .file-dir {
    font-size: 0.7rem;
    color: var(--fg-soft);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .trunc {
    border-top: 1px solid var(--border);
    color: var(--fg-soft);
    text-align: center;
    font-size: 0.75rem;
    padding: 8px 14px;
  }
</style>

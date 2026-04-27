<script lang="ts">
  import { onMount } from 'svelte';
  import { base } from '$app/paths';
  import { log, diff, ApiError } from '$lib/api';
  import type { CommitView, DiffEntry } from '$lib/types';
  import { shortHash, formatTimestamp, relativeTime } from '$lib/format';

  let commits: CommitView[] = [];
  let limit = 50;
  let loading = false;
  let error: string | null = null;
  let pathFilter = '';

  let openHash: string | null = null;
  let diffEntries: DiffEntry[] = [];
  let diffLoading = false;
  let diffError: string | null = null;

  async function load() {
    loading = true;
    error = null;
    try {
      commits = await log({
        max: limit,
        path: pathFilter.trim() || undefined,
        verbose: true
      });
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(async () => {
    await load();
    // Auto-open hash from URL fragment (e.g. linked from /commit redirect).
    const hash = window.location.hash.replace('#', '');
    if (hash && commits.find((c) => c.hash === hash)) {
      openCommit(hash);
    }
  });

  async function openCommit(hash: string) {
    if (openHash === hash) {
      openHash = null;
      diffEntries = [];
      return;
    }
    openHash = hash;
    diffEntries = [];
    diffError = null;
    const c = commits.find((x) => x.hash === hash);
    if (!c) return;
    const parent = c.parents && c.parents.length > 0 ? c.parents[0] : null;
    if (!parent) {
      // Genesis commit — no parent, can't diff. Show a stub.
      diffEntries = [];
      diffError = 'genesis commit (no parent)';
      return;
    }
    diffLoading = true;
    try {
      diffEntries = await diff({ from: parent, to: hash });
    } catch (e) {
      diffError = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      diffLoading = false;
    }
  }
</script>

<main class="page-container page-container--wide">
  <h1 class="page-title">Log</h1>
  <div class="page-sub">Commit timeline. Click a commit to expand its diff against its first parent.</div>

  {#if error}<div class="error-banner">{error}</div>{/if}

  <form on:submit|preventDefault={load} class="flex" style="gap: 12px; align-items: end; margin-bottom: 24px;">
    <div class="field" style="flex: 1; max-width: 320px; margin-bottom: 0;">
      <label class="label" for="pf">path filter (optional)</label>
      <input id="pf" class="input mono" bind:value={pathFilter} placeholder="reports/" />
    </div>
    <div class="field" style="width: 100px; margin-bottom: 0;">
      <label class="label" for="lim">max</label>
      <input id="lim" class="input" type="number" min="1" max="1000" bind:value={limit} />
    </div>
    <button class="btn" type="submit" disabled={loading}>
      {loading ? 'loading…' : 'reload'}
    </button>
  </form>

  {#if commits.length === 0 && !loading}
    <div class="soft">no commits</div>
  {/if}

  <ol class="commit-list">
    {#each commits as c}
      <li class="commit-row" id={c.hash} class:open={openHash === c.hash}>
        <button class="commit-head" on:click={() => openCommit(c.hash)}>
          <span class="mono commit-hash">{shortHash(c.hash, 12)}</span>
          <span class="commit-msg">{c.message}</span>
          <span class="commit-meta soft mono text-xs">
            {c.author} · {relativeTime(c.timestamp)}
          </span>
        </button>
        {#if openHash === c.hash}
          <div class="commit-detail">
            <div class="text-sm muted" style="margin-bottom: 12px;">
              <span class="mono">{c.email}</span> · {formatTimestamp(c.timestamp)}
              {#if c.parents}
                · parents:
                {#each c.parents as p, i}
                  {#if i > 0}, {/if}<a href="#{p}" class="mono">{shortHash(p, 10)}</a>
                {/each}
              {/if}
            </div>
            {#if diffLoading}
              <div class="muted">loading diff…</div>
            {:else if diffError}
              <div class="error-banner">{diffError}</div>
            {:else if diffEntries.length === 0}
              <div class="soft">no changes</div>
            {:else}
              <table class="table">
                <thead>
                  <tr><th>status</th><th>path</th><th>before</th><th>after</th></tr>
                </thead>
                <tbody>
                  {#each diffEntries as d}
                    <tr>
                      <td>
                        <span class="tag {d.status === 'added' ? 'tag--success' : d.status === 'removed' ? 'tag--danger' : d.status === 'modified' ? 'tag--accent' : ''}">
                          {d.status}
                        </span>
                      </td>
                      <td><a class="mono" href="{base}/file/{d.path}?at={c.hash}">{d.path}</a></td>
                      <td class="mono soft text-xs">{d.before ? shortHash(d.before, 10) : '—'}</td>
                      <td class="mono soft text-xs">{d.after ? shortHash(d.after, 10) : '—'}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            {/if}
          </div>
        {/if}
      </li>
    {/each}
  </ol>
</main>

<style>
  .commit-list {
    list-style: none;
    padding: 0;
    margin: 0;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--bg-elevated);
    overflow: hidden;
  }
  .commit-row {
    border-bottom: 1px solid var(--border);
  }
  .commit-row:last-child {
    border-bottom: 0;
  }
  .commit-head {
    width: 100%;
    border: 0;
    background: none;
    padding: 14px 18px;
    text-align: left;
    display: grid;
    grid-template-columns: auto 1fr auto;
    gap: 16px;
    align-items: center;
    cursor: pointer;
    color: inherit;
  }
  .commit-head:hover {
    background: rgba(17, 17, 17, 0.02);
  }
  .commit-row.open .commit-head {
    background: rgba(17, 17, 17, 0.03);
  }
  .commit-hash {
    color: var(--accent);
    font-size: 0.85rem;
  }
  .commit-msg {
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .commit-detail {
    padding: 16px 20px 20px;
    border-top: 1px solid var(--border);
  }
</style>

<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { status, commit, ApiError } from '$lib/api';
  import type { RepoStatus, ReprobeSummary } from '$lib/types';
  import { shortHash } from '$lib/format';

  let repoStatus: RepoStatus | null = null;
  let loading = false;
  let error: string | null = null;
  let message = '';
  let authorName = '';
  let authorEmail = '';
  let posting = false;
  let createdHash: string | null = null;
  let reprobed: ReprobeSummary[] = [];

  async function load() {
    loading = true;
    error = null;
    try {
      repoStatus = await status();
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(load);

  async function onCommit() {
    if (!message.trim()) return;
    posting = true;
    error = null;
    try {
      const author =
        authorName || authorEmail
          ? { name: authorName || undefined, email: authorEmail || undefined }
          : undefined;
      const r = await commit({ message: message.trim(), author });
      createdHash = r.hash;
      reprobed = r.reprobed ?? [];
      message = '';
      await load();
      // After a moment, jump to /log so the user sees their new commit.
      // If the commit reprobed files, give the user a beat longer to read
      // the banner before navigating away.
      const delay = reprobed.length > 0 ? 1800 : 600;
      setTimeout(() => goto(`${base}/log#${r.hash}`), delay);
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      posting = false;
    }
  }
</script>

<main class="page-container">
  <h1 class="page-title">Commit</h1>
  <div class="page-sub">
    Make staged changes durable. Single-writer-per-tenant invariant applies — a
    409 conflict means a parallel commit beat you; reload and retry.
  </div>

  {#if error}<div class="error-banner">{error}</div>{/if}
  {#if createdHash}
    <div class="success-banner">
      committed <code class="mono">{shortHash(createdHash, 16)}</code> — redirecting to /log…
    </div>
  {/if}
  {#if reprobed.length > 0}
    <div class="reprobe-banner">
      <strong>Schema change auto-rebuilt existing files.</strong>
      <ul class="reprobe-list">
        {#each reprobed as r}
          <li>
            <code>{r.file_type}</code>: {r.count} reprobed{#if r.skipped.length > 0}, <span class="danger">{r.skipped.length} skipped</span>{/if}
            {#if r.skipped.length > 0}
              <details>
                <summary class="text-xs soft">show skipped paths</summary>
                <ul class="skipped-list">
                  {#each r.skipped as s}
                    <li class="text-xs"><code>{s.path}</code> — {s.reason}</li>
                  {/each}
                </ul>
              </details>
            {/if}
          </li>
        {/each}
      </ul>
    </div>
  {/if}

  <section class="page-section">
    <h2>Staged ({repoStatus?.staged.length ?? 0})</h2>
    {#if loading}
      <div class="muted">loading…</div>
    {:else if !repoStatus || repoStatus.staged.length === 0}
      <div class="soft">nothing staged. upload a file or run <code>omp add</code> from the CLI.</div>
    {:else}
      <table class="table">
        <thead>
          <tr><th>kind</th><th>path</th><th>hash</th></tr>
        </thead>
        <tbody>
          {#each repoStatus.staged as s}
            <tr>
              <td><span class="tag {s.kind === 'remove' ? 'tag--danger' : 'tag--accent'}">{s.kind}</span></td>
              <td><a href="{base}/file/{s.path}" class="mono">{s.path}</a></td>
              <td class="mono soft">{shortHash(s.hash ?? '', 12)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </section>

  {#if repoStatus && repoStatus.staged.length > 0}
    <section class="page-section">
      <h2>Compose</h2>
      <form on:submit|preventDefault={onCommit} class="stack--lg">
        <div class="field">
          <label class="label" for="msg">message</label>
          <textarea
            id="msg"
            class="textarea"
            bind:value={message}
            placeholder="add Q3 earnings"
            required
          ></textarea>
        </div>
        <div class="flex" style="gap: 16px; flex-wrap: wrap;">
          <div class="field" style="flex: 1; min-width: 200px;">
            <label class="label" for="an">author name (optional)</label>
            <input id="an" class="input" bind:value={authorName} />
          </div>
          <div class="field" style="flex: 1; min-width: 200px;">
            <label class="label" for="ae">author email (optional)</label>
            <input id="ae" class="input" bind:value={authorEmail} />
          </div>
        </div>
        <div class="flex flex--between">
          <span class="muted text-sm">
            {#if repoStatus.branch}on branch <code>{repoStatus.branch}</code>{/if}
          </span>
          <button type="submit" class="btn btn--primary" disabled={!message.trim() || posting}>
            {posting ? 'committing…' : 'commit'}
          </button>
        </div>
      </form>
    </section>
  {/if}
</main>

<style>
  .success-banner {
    padding: 12px 16px;
    margin: 16px 0;
    border: 1px solid rgba(31, 122, 58, 0.3);
    border-radius: 8px;
    background: rgba(31, 122, 58, 0.05);
    color: var(--success);
    font-size: 0.9rem;
  }
  .reprobe-banner {
    padding: 12px 16px;
    margin: 12px 0;
    border: 1px solid rgba(39, 71, 212, 0.25);
    border-radius: 8px;
    background: rgba(39, 71, 212, 0.04);
    font-size: 0.9rem;
  }
  .reprobe-list {
    margin: 8px 0 0;
    padding-left: 18px;
  }
  .skipped-list {
    margin: 4px 0 0;
    padding-left: 16px;
    color: var(--fg-muted);
  }
</style>

<script lang="ts">
  import { onMount } from 'svelte';
  import { base } from '$app/paths';
  import {
    ApiError,
    listMarketplaceSchemas,
    type MarketplaceSchema
  } from '$lib/api';

  let schemas: MarketplaceSchema[] = [];
  let loading = true;
  let error: string | null = null;
  let query = '';

  onMount(load);

  async function load() {
    loading = true;
    error = null;
    try {
      const resp = await listMarketplaceSchemas({ limit: 100 });
      schemas = resp.schemas;
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  $: filtered = filterSchemas(schemas, query);

  function filterSchemas(all: MarketplaceSchema[], q: string): MarketplaceSchema[] {
    const needle = q.trim().toLowerCase();
    if (!needle) return all;
    return all.filter(
      (s) =>
        s.file_type.toLowerCase().includes(needle) ||
        (s.description ?? '').toLowerCase().includes(needle)
    );
  }

  function fmtDate(ts: number): string {
    return ts ? new Date(ts * 1000).toLocaleString() : '';
  }
</script>

<section class="page">
  <header class="page-header">
    <div class="page-title-row">
      <h1>Schema marketplace</h1>
      <div class="page-actions">
        <a class="btn btn--ghost" href="{base}/marketplace">Probe marketplace →</a>
        <a class="btn btn--primary" href="{base}/schema-marketplace/upload">Publish schema</a>
      </div>
    </div>
    <p class="muted">
      Community-published schemas. A schema is a <code>file_type</code> + MIME
      patterns + a set of fields (probe references and user-provided metadata).
      Browse, then click through to install into your tenant tree.
    </p>
  </header>

  <div class="search">
    <input
      class="input"
      type="text"
      placeholder="Search file_type or description"
      bind:value={query}
    />
    <button class="btn btn--ghost" on:click={() => (query = '')} disabled={!query}>clear</button>
  </div>

  {#if loading}
    <div class="muted">loading…</div>
  {:else if error}
    <div class="error-banner">{error}</div>
  {:else if filtered.length === 0}
    <div class="empty">
      {#if schemas.length === 0}
        No schemas published yet. <a class="link" href="{base}/schema-marketplace/upload">Publish one →</a>
      {:else}
        No schemas match "{query}".
      {/if}
    </div>
  {:else}
    <div class="grid">
      {#each filtered as s (s.id)}
        <a class="card" href="{base}/schema-marketplace/{s.id}">
          <header class="card-head">
            <span class="name mono">{s.file_type}</span>
            <span class="ver mono">@{s.version}</span>
          </header>
          <p class="desc">{s.description ?? '(no description)'}</p>
          <footer class="card-foot">
            <span class="soft">by</span>
            <code class="mono pub">{s.publisher_sub.slice(0, 16)}</code>
            <span class="soft">· {fmtDate(s.published_at)}</span>
          </footer>
        </a>
      {/each}
    </div>
  {/if}
</section>

<style>
  .page {
    max-width: var(--max-width-wide);
    margin: 0 auto;
    padding: 24px;
  }
  .page-title-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 8px;
  }
  .page-actions {
    display: flex;
    gap: 8px;
  }
  .search {
    display: flex;
    gap: 8px;
    margin-bottom: 16px;
  }
  .search .input {
    flex: 1;
    max-width: 480px;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
    gap: 12px;
  }
  .card {
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 14px 16px;
    background: var(--bg-elevated);
    text-decoration: none;
    color: inherit;
    display: block;
  }
  .card:hover {
    border-color: var(--accent, #2747d4);
  }
  .card-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    margin-bottom: 6px;
  }
  .name {
    font-size: 1rem;
    font-weight: 600;
  }
  .ver {
    color: var(--fg-soft);
    font-size: 0.8rem;
  }
  .desc {
    margin: 0 0 8px;
    font-size: 0.85rem;
    color: var(--fg-muted);
  }
  .card-foot {
    font-size: 0.75rem;
    color: var(--fg-soft);
  }
  .pub {
    font-size: 0.7rem;
  }
  .empty {
    padding: 32px;
    text-align: center;
    color: var(--fg-soft);
    border: 1px dashed var(--border);
    border-radius: 8px;
  }
</style>

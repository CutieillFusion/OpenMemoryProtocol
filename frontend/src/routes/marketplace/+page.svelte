<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import {
    ApiError,
    listMarketplaceProbes,
    installMarketplaceProbe,
    type MarketplaceProbe
  } from '$lib/api';

  let probes: MarketplaceProbe[] = [];
  let loading = true;
  let error: string | null = null;
  let query = '';
  let installing: string | null = null;
  let installError: string | null = null;
  let installedNotice: string | null = null;

  onMount(load);

  async function load() {
    loading = true;
    error = null;
    try {
      const resp = await listMarketplaceProbes({
        q: query.trim() || undefined,
        limit: 100
      });
      probes = resp.probes;
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  function fmtDate(ts: number): string {
    if (!ts) return '';
    return new Date(ts * 1000).toLocaleString();
  }

  function shortId(id: string): string {
    return id.slice(0, 12);
  }

  async function install(probe: MarketplaceProbe) {
    installing = probe.id;
    installError = null;
    installedNotice = null;
    try {
      await installMarketplaceProbe(probe.id);
      installedNotice = `Staged ${probe.namespace}.${probe.name}@${probe.version}. Visit /commit to make it durable.`;
    } catch (e) {
      installError = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      installing = null;
    }
  }
</script>

<section class="page">
  <header class="page-header">
    <div class="page-title-row">
      <h1>Marketplace</h1>
      <a class="btn" href="{base}/marketplace/upload">Upload existing probe</a>
    </div>
    <p class="muted">
      Install community-published probes into your tenant. Per
      <code>docs/design/23-probe-marketplace.md</code>, each probe is a folder
      (<code>probe.wasm</code> + <code>probe.toml</code> + optional
      <code>README.md</code>) addressed by content hash. Click <em>view</em> to
      inspect the manifest and source before <em>install</em>.
    </p>
  </header>

  <div class="search">
    <input
      class="input"
      type="text"
      placeholder="Search namespace, name, or description"
      bind:value={query}
      on:keydown={(e) => e.key === 'Enter' && load()}
    />
    <button class="btn" on:click={load}>Search</button>
  </div>

  {#if installedNotice}
    <div class="notice notice--ok">
      {installedNotice}
      <a class="link" href="{base}/commit">Go to Commit →</a>
    </div>
  {/if}
  {#if installError}
    <div class="notice notice--err">install failed: {installError}</div>
  {/if}

  {#if loading}
    <div class="muted">loading…</div>
  {:else if error}
    <div class="error-banner">{error}</div>
  {:else if probes.length === 0}
    <div class="empty">No probes published yet. Build one on the
      <a class="link" href="{base}/probes/build">Build probe</a> page and click
      <em>Publish</em>.
    </div>
  {:else}
    <div class="grid">
      {#each probes as p (p.id)}
        <article class="card">
          <header class="card-head">
            <div class="card-title">
              <span class="ns">{p.namespace}.</span><span class="name">{p.name}</span>
              <span class="version">@{p.version}</span>
            </div>
            <div class="card-id mono">{shortId(p.id)}</div>
          </header>
          <p class="card-desc">{p.description ?? '(no description)'}</p>
          <footer class="card-foot">
            <div class="card-meta">
              <span class="soft">by</span>
              <span class="mono">{shortId(p.publisher_sub)}</span>
              <span class="soft">· {fmtDate(p.published_at)}</span>
            </div>
            <div class="card-actions">
              <a class="btn btn--ghost btn--sm" href="{base}/marketplace/{p.id}">view</a>
              <button
                class="btn btn--primary btn--sm"
                disabled={installing !== null}
                on:click={() => install(p)}
              >
                {installing === p.id ? 'installing…' : 'install'}
              </button>
            </div>
          </footer>
        </article>
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
  .page-header h1 {
    margin: 0;
  }
  .page-title-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 8px;
  }
  .card-actions {
    display: flex;
    gap: 8px;
  }
  .page-header p {
    margin: 0 0 24px;
    font-size: 0.9rem;
  }
  .search {
    display: flex;
    gap: 8px;
    margin-bottom: 16px;
  }
  .search .input {
    flex: 1;
    max-width: 400px;
  }
  .notice {
    padding: 8px 12px;
    border-radius: 6px;
    margin-bottom: 12px;
    font-size: 0.85rem;
  }
  .notice--ok {
    background: rgba(40, 160, 80, 0.1);
    border: 1px solid rgba(40, 160, 80, 0.3);
  }
  .notice--err {
    background: rgba(220, 60, 60, 0.1);
    border: 1px solid rgba(220, 60, 60, 0.3);
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
  }
  .card-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    margin-bottom: 6px;
  }
  .card-title {
    font-family: var(--font-mono);
    font-size: 0.95rem;
  }
  .ns {
    color: var(--fg-soft);
  }
  .version {
    color: var(--fg-soft);
    margin-left: 4px;
  }
  .card-id {
    font-size: 0.75rem;
    color: var(--fg-soft);
  }
  .card-desc {
    font-size: 0.85rem;
    color: var(--fg-muted);
    margin: 6px 0 12px;
    min-height: 2.4em;
  }
  .card-foot {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.8rem;
  }
  .card-meta {
    color: var(--fg-soft);
  }
  .empty {
    padding: 32px;
    text-align: center;
    color: var(--fg-soft);
    border: 1px dashed var(--border);
    border-radius: 8px;
  }
</style>

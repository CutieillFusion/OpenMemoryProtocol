<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { base } from '$app/paths';
  import {
    ApiError,
    fetchMarketplaceSchemaBlob,
    getMarketplaceSchema,
    getMe,
    yankMarketplaceSchema,
    type MarketplaceSchema
  } from '$lib/api';

  let schema: MarketplaceSchema | null = null;
  let schemaText = '';
  let readmeText: string | null = null;
  let loading = true;
  let error: string | null = null;
  let mySub: string | null = null;
  let yanking = false;
  let yankError: string | null = null;

  $: id = $page.params.id ?? '';
  $: isOwner = !!schema && !!mySub && schema.publisher_sub === mySub;

  onMount(() => {
    load();
    getMe()
      .then((me) => (mySub = me.sub))
      .catch(() => {});
  });

  async function load() {
    loading = true;
    error = null;
    try {
      const resp = await getMarketplaceSchema(id);
      schema = resp.schema;
      schemaText = resp.schema_preview ?? '';
      if (schema.readme_hash) {
        const r = await fetchMarketplaceSchemaBlob(id, schema.readme_hash);
        readmeText = await r.text();
      }
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  function fmtDate(ts: number): string {
    return ts ? new Date(ts * 1000).toLocaleString() : '';
  }

  async function yank() {
    if (!schema) return;
    if (!confirm(`Yank schema ${schema.file_type}@${schema.version}?`)) return;
    yanking = true;
    yankError = null;
    try {
      await yankMarketplaceSchema(schema.id);
      await load();
    } catch (e) {
      yankError = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      yanking = false;
    }
  }
</script>

<section class="page">
  <a class="back" href="{base}/schema-marketplace">← back to schema marketplace</a>

  {#if loading}
    <div class="muted">loading…</div>
  {:else if error}
    <div class="error-banner">{error}</div>
  {:else if schema}
    <header class="head">
      <div>
        <h1 class="title mono">
          {schema.file_type} <span class="ver">@{schema.version}</span>
        </h1>
        <p class="muted">{schema.description ?? '(no description)'}</p>
        <div class="meta">
          <span class="soft">id</span> <code class="mono">{schema.id.slice(0, 16)}</code>
          <span class="soft">· schema</span> <code class="mono">{schema.schema_hash.slice(0, 12)}</code>
          <span class="soft">· by</span> <code class="mono">{schema.publisher_sub}</code>
          <span class="soft">· published</span> {fmtDate(schema.published_at)}
          {#if schema.yanked_at}
            <span class="tag tag--danger">yanked {fmtDate(schema.yanked_at)}</span>
          {/if}
        </div>
      </div>
      {#if isOwner}
        <div class="owner-actions">
          <a class="btn btn--ghost btn--sm" href="{base}/schema-marketplace/{schema.id}/edit"
            >edit metadata</a
          >
          <a class="btn btn--ghost btn--sm" href="{base}/schema-marketplace/upload"
            >push new version</a
          >
          {#if !schema.yanked_at}
            <button class="btn btn--ghost btn--sm danger" disabled={yanking} on:click={yank}>
              {yanking ? 'yanking…' : 'yank'}
            </button>
          {/if}
          {#if yankError}
            <div class="error-banner small">{yankError}</div>
          {/if}
        </div>
      {/if}
    </header>

    {#if readmeText !== null}
      <section class="block">
        <h2>README</h2>
        <pre class="prose">{readmeText}</pre>
      </section>
    {/if}

    <section class="block">
      <h2>schema.toml</h2>
      <pre class="code mono">{schemaText}</pre>
    </section>
  {/if}
</section>

<style>
  .page {
    max-width: var(--max-width-wide);
    margin: 0 auto;
    padding: 24px;
  }
  .back {
    display: inline-block;
    color: var(--fg-muted);
    text-decoration: none;
    font-size: 0.85rem;
    margin-bottom: 16px;
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 24px;
    border-bottom: 1px solid var(--border);
    padding-bottom: 16px;
    margin-bottom: 24px;
  }
  .title {
    margin: 0 0 4px;
    font-size: 1.4rem;
    font-weight: 600;
  }
  .ver {
    color: var(--fg-soft);
  }
  .meta {
    font-size: 0.8rem;
    color: var(--fg-muted);
    margin-top: 8px;
  }
  .owner-actions {
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-end;
  }
  .owner-actions .danger {
    color: var(--danger, #c33);
  }
  .block {
    margin-bottom: 32px;
  }
  .block h2 {
    margin: 0 0 8px;
    font-size: 1rem;
  }
  pre.prose,
  pre.code {
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 12px 14px;
    background: var(--bg-elevated);
    font-size: 0.82rem;
    line-height: 1.45;
    overflow-x: auto;
    white-space: pre-wrap;
    word-break: break-word;
  }
  pre.prose {
    font-family: var(--font-sans);
  }
  .error-banner.small {
    padding: 6px 10px;
    font-size: 0.8rem;
  }
</style>

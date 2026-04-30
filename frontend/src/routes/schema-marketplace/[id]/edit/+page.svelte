<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/stores';
  import { base } from '$app/paths';
  import {
    ApiError,
    fetchMarketplaceSchemaBlob,
    getMarketplaceSchema,
    getMe,
    patchMarketplaceSchema,
    type MarketplaceSchema
  } from '$lib/api';

  let schema: MarketplaceSchema | null = null;
  let mySub: string | null = null;
  let description = '';
  let readme = '';
  let loading = true;
  let saving = false;
  let error: string | null = null;

  $: id = $page.params.id ?? '';
  $: isOwner = !!schema && !!mySub && schema.publisher_sub === mySub;

  onMount(async () => {
    try {
      const me = await getMe();
      mySub = me.sub;
    } catch {
      // anonymous viewer; PATCH will 403 if they try to save
    }
    await load();
  });

  async function load() {
    loading = true;
    error = null;
    try {
      const resp = await getMarketplaceSchema(id);
      schema = resp.schema;
      description = schema.description ?? '';
      if (schema.readme_hash) {
        const r = await fetchMarketplaceSchemaBlob(id, schema.readme_hash);
        readme = await r.text();
      } else {
        readme = '';
      }
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  async function save() {
    if (!schema) return;
    saving = true;
    error = null;
    try {
      await patchMarketplaceSchema(schema.id, { description, readme });
      goto(`${base}/schema-marketplace/${schema.id}`);
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      saving = false;
    }
  }
</script>

<section class="page">
  <a class="back" href="{base}/schema-marketplace/{id}">← back to schema</a>
  <h1>Edit metadata</h1>
  <p class="muted">
    Description and README only. The TOML body needs a new version — use
    <em>push new version</em> on the detail page.
  </p>

  {#if loading}
    <div class="muted">loading…</div>
  {:else if error}
    <div class="error-banner">{error}</div>
  {:else if schema && !isOwner}
    <div class="error-banner">Only the publisher can edit this schema.</div>
  {:else if schema}
    <div class="field">
      <label class="label" for="desc">description</label>
      <input id="desc" class="input" bind:value={description} />
    </div>
    <div class="field">
      <label class="label" for="rd">README.md</label>
      <textarea id="rd" class="input mono" rows="14" bind:value={readme}></textarea>
    </div>
    <div class="flex flex--end" style="margin-top: 16px;">
      <button class="btn btn--primary" on:click={save} disabled={saving}>
        {saving ? 'saving…' : 'save'}
      </button>
    </div>
  {/if}
</section>

<style>
  .page {
    max-width: 720px;
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
  .field {
    margin-bottom: 16px;
  }
</style>

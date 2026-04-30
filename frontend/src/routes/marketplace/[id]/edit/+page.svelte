<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/stores';
  import { base } from '$app/paths';
  import {
    ApiError,
    fetchMarketplaceBlob,
    getMarketplaceProbe,
    getMe,
    patchMarketplaceProbe,
    type MarketplaceProbe
  } from '$lib/api';

  let probe: MarketplaceProbe | null = null;
  let mySub: string | null = null;
  let description = '';
  let readme = '';
  let loading = true;
  let saving = false;
  let error: string | null = null;

  $: id = $page.params.id ?? '';
  $: isOwner = !!probe && !!mySub && probe.publisher_sub === mySub;

  onMount(async () => {
    try {
      const me = await getMe();
      mySub = me.sub;
    } catch {
      // Anonymous viewer; the API call below will still 403 server-side
      // when they try to save.
    }
    await load();
  });

  async function load() {
    loading = true;
    error = null;
    try {
      const resp = await getMarketplaceProbe(id);
      probe = resp.probe;
      description = probe.description ?? '';
      if (probe.readme_hash) {
        const r = await fetchMarketplaceBlob(id, probe.readme_hash);
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
    if (!probe) return;
    saving = true;
    error = null;
    try {
      await patchMarketplaceProbe(probe.id, {
        description,
        readme
      });
      goto(`${base}/marketplace/${probe.id}`);
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      saving = false;
    }
  }
</script>

<section class="page">
  <a class="back" href="{base}/marketplace/{id}">← back to probe</a>
  <h1>Edit metadata</h1>
  <p class="muted">
    Description and README only. Code changes need a new version — use
    <em>push new version</em> on the detail page.
  </p>

  {#if loading}
    <div class="muted">loading…</div>
  {:else if error}
    <div class="error-banner">{error}</div>
  {:else if probe && !isOwner}
    <div class="error-banner">Only the publisher can edit this probe.</div>
  {:else if probe}
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

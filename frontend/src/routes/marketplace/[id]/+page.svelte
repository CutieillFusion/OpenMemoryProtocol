<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { base } from '$app/paths';
  import {
    ApiError,
    fetchMarketplaceBlob,
    getMarketplaceProbe,
    getMe,
    installMarketplaceProbe,
    yankMarketplaceProbe,
    type MarketplaceProbe
  } from '$lib/api';

  let probe: MarketplaceProbe | null = null;
  let manifestText = '';
  let readmeText: string | null = null;
  let sourceText: string | null = null;
  let loading = true;
  let error: string | null = null;
  let installing = false;
  let installed = false;
  let installError: string | null = null;
  let mySub: string | null = null;
  let yanking = false;
  let yankError: string | null = null;

  $: id = $page.params.id ?? '';

  onMount(() => {
    load();
    // Owner actions are conditional on viewer == publisher. getMe() may 401
    // for anonymous viewers; that's fine, we just leave mySub null and hide
    // the controls.
    getMe()
      .then((me) => (mySub = me.sub))
      .catch(() => {});
  });

  $: isOwner = !!probe && !!mySub && probe.publisher_sub === mySub;

  async function load() {
    if (!id) {
      error = 'missing probe id';
      loading = false;
      return;
    }
    loading = true;
    error = null;
    try {
      const resp = await getMarketplaceProbe(id);
      probe = resp.probe;
      // Always fetch manifest fresh (small) so the detail-page view never
      // diverges from the bytes that would actually install.
      const manifestResp = await fetchMarketplaceBlob(id, probe.manifest_hash);
      manifestText = await manifestResp.text();
      if (probe.readme_hash) {
        const r = await fetchMarketplaceBlob(id, probe.readme_hash);
        readmeText = await r.text();
      }
      if (probe.source_hash) {
        const s = await fetchMarketplaceBlob(id, probe.source_hash);
        sourceText = await s.text();
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

  async function install() {
    if (!probe) return;
    installing = true;
    installError = null;
    try {
      await installMarketplaceProbe(probe.id);
      installed = true;
    } catch (e) {
      installError = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      installing = false;
    }
  }

  async function yank() {
    if (!probe) return;
    if (!confirm(`Yank ${probe.namespace}.${probe.name}@${probe.version}? Existing installs keep working; new installs are blocked.`)) {
      return;
    }
    yanking = true;
    yankError = null;
    try {
      await yankMarketplaceProbe(probe.id);
      await load();
    } catch (e) {
      yankError = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      yanking = false;
    }
  }
</script>

<section class="page">
  <a class="back" href="{base}/marketplace">← back to marketplace</a>

  {#if loading}
    <div class="muted">loading…</div>
  {:else if error}
    <div class="error-banner">{error}</div>
  {:else if probe}
    <header class="probe-head">
      <div>
        <h1 class="probe-title mono">
          <span class="ns">{probe.namespace}.</span>{probe.name}
          <span class="version">@{probe.version}</span>
        </h1>
        <p class="muted">{probe.description ?? '(no description)'}</p>
        <div class="meta">
          <span class="soft">id</span> <code class="mono">{probe.id.slice(0, 16)}</code>
          <span class="soft">· wasm</span> <code class="mono">{probe.wasm_hash.slice(0, 12)}</code>
          <span class="soft">· by</span> <code class="mono">{probe.publisher_sub}</code>
          <span class="soft">· published</span> {fmtDate(probe.published_at)}
          {#if probe.yanked_at}
            <span class="tag tag--danger">yanked {fmtDate(probe.yanked_at)}</span>
          {/if}
        </div>
      </div>
      <div class="install-col">
        {#if installed}
          <div class="notice notice--ok">
            Staged. Visit <a class="link" href="{base}/commit">/commit</a> to make it durable.
          </div>
        {:else if !probe.yanked_at}
          <button class="btn btn--primary" disabled={installing} on:click={install}>
            {installing ? 'installing…' : 'install'}
          </button>
          {#if installError}
            <div class="error-banner small">{installError}</div>
          {/if}
        {/if}
        {#if isOwner}
          <div class="owner-actions">
            <a
              class="btn btn--ghost btn--sm"
              href="{base}/marketplace/{probe.id}/edit"
            >edit metadata</a>
            <a
              class="btn btn--ghost btn--sm"
              href="{base}/marketplace/upload?namespace={encodeURIComponent(probe.namespace)}&name={encodeURIComponent(probe.name)}"
            >push new version</a>
            {#if !probe.yanked_at}
              <button class="btn btn--ghost btn--sm danger" disabled={yanking} on:click={yank}>
                {yanking ? 'yanking…' : 'yank'}
              </button>
            {/if}
            {#if yankError}
              <div class="error-banner small">{yankError}</div>
            {/if}
          </div>
        {/if}
      </div>
    </header>

    {#if readmeText !== null}
      <section class="block">
        <h2>README</h2>
        <pre class="prose">{readmeText}</pre>
      </section>
    {/if}

    <section class="block">
      <h2>probe.toml</h2>
      <pre class="code mono">{manifestText}</pre>
    </section>

    {#if sourceText !== null}
      <section class="block">
        <h2>source / lib.rs</h2>
        <pre class="code mono">{sourceText}</pre>
      </section>
    {:else}
      <section class="block">
        <h2>source</h2>
        <p class="muted text-sm">
          The publisher did not include source. Compiled
          <code>probe.wasm</code> alone is the only artifact that gets
          installed.
        </p>
      </section>
    {/if}
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
  .back:hover {
    color: var(--fg);
  }
  .probe-head {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 24px;
    border-bottom: 1px solid var(--border);
    padding-bottom: 16px;
    margin-bottom: 24px;
  }
  .probe-title {
    margin: 0 0 4px;
    font-size: 1.4rem;
    font-weight: 600;
  }
  .ns,
  .version {
    color: var(--fg-soft);
  }
  .meta {
    font-size: 0.8rem;
    color: var(--fg-muted);
    margin-top: 8px;
  }
  .meta code {
    font-size: 0.78rem;
  }
  .install-col {
    min-width: 220px;
    text-align: right;
    display: flex;
    flex-direction: column;
    gap: 8px;
    align-items: flex-end;
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
  .notice {
    padding: 8px 12px;
    border-radius: 6px;
    font-size: 0.85rem;
  }
  .notice--ok {
    background: rgba(40, 160, 80, 0.1);
    border: 1px solid rgba(40, 160, 80, 0.3);
  }
  .error-banner.small {
    padding: 6px 10px;
    font-size: 0.8rem;
  }
  .owner-actions {
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-end;
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px dashed var(--border);
  }
  .owner-actions .danger {
    color: var(--danger, #c33);
  }
</style>

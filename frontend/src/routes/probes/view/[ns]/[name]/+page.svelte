<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { base } from '$app/paths';
  import { ApiError, fetchBytes } from '$lib/api';

  let manifestText: string | null = null;
  let sourceText: string | null = null;
  let readmeText: string | null = null;
  let loading = true;
  let error: string | null = null;
  let manifestErr: string | null = null;

  $: ns = $page.params.ns ?? '';
  $: name = $page.params.name ?? '';
  $: probeRef = ns && name ? `${ns}.${name}` : '';
  $: probeDir = ns && name ? `probes/${ns}/${name}` : '';

  onMount(load);

  async function load() {
    if (!ns || !name) {
      error = 'missing probe ns/name';
      loading = false;
      return;
    }
    loading = true;
    error = null;
    manifestErr = null;
    manifestText = null;
    sourceText = null;
    readmeText = null;
    try {
      // probe.toml is required for any installed probe; if it's missing, the
      // probe isn't in this tenant.
      manifestText = await tryFetchText(`${probeDir}/probe.toml`);
      if (manifestText === null) {
        manifestErr = `probes/${ns}/${name}/probe.toml not found in this tenant. The probe may live only in the marketplace — search there instead.`;
      }
      sourceText = await tryFetchText(`${probeDir}/source/lib.rs`);
      readmeText = await tryFetchText(`${probeDir}/README.md`);
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  async function tryFetchText(path: string): Promise<string | null> {
    try {
      const resp = await fetchBytes(path);
      return await resp.text();
    } catch (e) {
      // Treat 404 / not-found as a missing companion, not a hard error.
      if (e instanceof ApiError && (e.code === 'not_found' || e.status === 404)) {
        return null;
      }
      throw e;
    }
  }
</script>

<section class="page">
  <a class="back" href="{base}/schema-marketplace">← back to schemas</a>

  <header class="head">
    <h1 class="title mono">
      <span class="ns">{ns}.</span>{name}
    </h1>
    <div class="actions">
      <a class="btn btn--ghost" href="{base}/marketplace?q={encodeURIComponent(probeRef)}">find in marketplace →</a>
    </div>
  </header>
  <p class="muted text-sm">
    Source viewer for the locally-installed probe at
    <code class="mono">{probeDir}/</code>. This page reads
    <code>probe.toml</code>, <code>source/lib.rs</code>, and
    <code>README.md</code> from the tenant's tree via
    <code>GET /files/&lt;path&gt;</code>. If you're looking at a probe that
    your tenant hasn't installed yet, the
    <a class="link" href="{base}/marketplace">probe marketplace</a> is the
    place to inspect and install it.
  </p>

  {#if loading}
    <div class="muted">loading…</div>
  {:else if error}
    <div class="error-banner">{error}</div>
  {:else}
    {#if manifestErr}
      <div class="notice notice--info">{manifestErr}</div>
    {/if}

    {#if readmeText !== null}
      <section class="block">
        <h2>README</h2>
        <pre class="prose">{readmeText}</pre>
      </section>
    {/if}

    {#if manifestText !== null}
      <section class="block">
        <h2>probe.toml</h2>
        <pre class="code mono">{manifestText}</pre>
      </section>
    {/if}

    {#if sourceText !== null}
      <section class="block">
        <h2>source / lib.rs</h2>
        <pre class="code mono">{sourceText}</pre>
      </section>
    {:else if manifestText !== null}
      <section class="block">
        <h2>source</h2>
        <p class="muted text-sm">
          No <code>source/lib.rs</code> companion in this tenant's tree. The
          probe was installed without source — only the compiled
          <code>probe.wasm</code> is present.
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
  .head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    border-bottom: 1px solid var(--border);
    padding-bottom: 12px;
    margin-bottom: 12px;
  }
  .title {
    margin: 0;
    font-size: 1.4rem;
    font-weight: 600;
  }
  .ns {
    color: var(--fg-soft);
  }
  .actions {
    display: flex;
    gap: 8px;
  }
  .notice {
    padding: 8px 12px;
    border-radius: 6px;
    margin-bottom: 16px;
    font-size: 0.85rem;
  }
  .notice--info {
    background: rgba(40, 90, 200, 0.06);
    border: 1px solid rgba(40, 90, 200, 0.25);
  }
  .block {
    margin: 24px 0;
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
</style>

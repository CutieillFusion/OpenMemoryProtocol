<script lang="ts">
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { page } from '$app/stores';
  import { ApiError, publishMarketplaceProbe } from '$lib/api';
  import CodeEditor from '$lib/components/CodeEditor.svelte';

  let namespace = $page.url.searchParams.get('namespace') ?? '';
  let name = $page.url.searchParams.get('name') ?? '';
  let version = '0.1.0';
  let description = '';
  let readmeText = '';
  let sourceText = '';
  let manifestText = '';

  let publishing = false;
  let error: string | null = null;
  let buildLog: string | null = null;

  $: canSubmit =
    !publishing &&
    namespace.trim() !== '' &&
    name.trim() !== '' &&
    version.trim() !== '' &&
    sourceText.trim() !== '' &&
    manifestText.trim() !== '';

  async function submit() {
    if (!canSubmit) return;
    publishing = true;
    error = null;
    buildLog = null;
    try {
      const source = new Blob([sourceText], { type: 'text/plain' });
      const manifest = new Blob([manifestText], { type: 'text/plain' });
      const readme = readmeText.trim()
        ? new Blob([readmeText], { type: 'text/markdown' })
        : undefined;
      const resp = await publishMarketplaceProbe({
        namespace: namespace.trim(),
        name: name.trim(),
        version: version.trim(),
        description: description.trim() || undefined,
        source,
        manifest,
        readme
      });
      goto(`${base}/marketplace/${resp.probe.id}`);
    } catch (e) {
      if (e instanceof ApiError) {
        error = `${e.code}: ${e.message}`;
        const log = e.details?.log;
        if (typeof log === 'string') buildLog = log;
      } else {
        error = String(e);
      }
    } finally {
      publishing = false;
    }
  }
</script>

<section class="page">
  <a class="back" href="{base}/marketplace">← back to marketplace</a>
  <h1>Publish a probe</h1>
  <p class="muted">
    Paste your probe's <code class="mono">lib.rs</code> below. OMP compiles it
    server-side on every publish (and every version bump), so the on-disk
    <code class="mono">probe.wasm</code> always matches the published source.
    Pre-compiled WASM uploads are not accepted.
  </p>

  <div class="grid">
    <div class="field">
      <label class="label" for="ns">namespace</label>
      <input id="ns" class="input mono" bind:value={namespace} placeholder="text" />
    </div>
    <div class="field">
      <label class="label" for="nm">name</label>
      <input id="nm" class="input mono" bind:value={name} placeholder="word_count" />
    </div>
    <div class="field">
      <label class="label" for="vr">version</label>
      <input id="vr" class="input mono" bind:value={version} placeholder="0.1.0" />
    </div>
  </div>

  <div class="field">
    <label class="label" for="desc">description (optional)</label>
    <input id="desc" class="input" bind:value={description} placeholder="One-line summary" />
  </div>

  <div class="field">
    <span class="label">lib.rs<span class="req"> *</span></span>
    <CodeEditor
      language="rust"
      bind:value={sourceText}
      disabled={publishing}
      minHeight="320px"
      placeholder="//! Your probe's Rust source."
    />
  </div>

  <div class="field">
    <span class="label">probe.toml<span class="req"> *</span></span>
    <CodeEditor
      language="toml"
      bind:value={manifestText}
      disabled={publishing}
      minHeight="220px"
      placeholder={`name = "text.word_count"\nreturns = "int"\naccepts_kwargs = []\ndescription = "Counts whitespace-separated tokens."\n\n[limits]\nmemory_mb = 32\nfuel = 100000000\nwall_clock_s = 5`}
    />
  </div>

  <div class="field">
    <label class="label" for="rd">README.md (optional, markdown)</label>
    <textarea
      id="rd"
      class="input mono"
      rows="6"
      bind:value={readmeText}
      placeholder="## How to use this probe..."
    ></textarea>
  </div>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}
  {#if buildLog}
    <details class="build-log" open>
      <summary>build log</summary>
      <pre class="mono">{buildLog}</pre>
    </details>
  {/if}

  <div class="flex flex--between" style="margin-top: 16px;">
    <span class="muted text-sm">
      The binding key is
      <code class="mono">(publisher_sub, namespace, name, version)</code>.
      Republishing the same version returns 409 — bump the version to push an update.
    </span>
    <button class="btn btn--primary" on:click={submit} disabled={!canSubmit}>
      {publishing ? 'building & publishing…' : 'publish'}
    </button>
  </div>
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
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 12px;
  }
  .field {
    margin-bottom: 16px;
  }
  .req {
    color: var(--danger, #c33);
  }
  .build-log {
    margin-top: 12px;
    border: 1px solid var(--border, #ddd);
    padding: 8px;
    border-radius: 4px;
  }
  .build-log pre {
    max-height: 320px;
    overflow: auto;
    font-size: 0.75rem;
    white-space: pre-wrap;
  }
</style>

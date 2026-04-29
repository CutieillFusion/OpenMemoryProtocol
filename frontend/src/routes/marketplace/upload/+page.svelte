<script lang="ts">
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { ApiError, publishMarketplaceProbe } from '$lib/api';

  let namespace = '';
  let name = '';
  let version = '0.1.0';
  let description = '';
  let readmeText = '';
  let sourceText = '';

  let wasmFile: File | null = null;
  let manifestFile: File | null = null;

  let publishing = false;
  let error: string | null = null;

  function pickWasm(e: Event) {
    const f = (e.target as HTMLInputElement).files?.[0];
    wasmFile = f ?? null;
  }
  function pickManifest(e: Event) {
    const f = (e.target as HTMLInputElement).files?.[0];
    manifestFile = f ?? null;
  }

  $: canSubmit =
    !publishing &&
    namespace.trim() !== '' &&
    name.trim() !== '' &&
    version.trim() !== '' &&
    !!wasmFile &&
    !!manifestFile;

  async function submit() {
    if (!canSubmit || !wasmFile || !manifestFile) return;
    publishing = true;
    error = null;
    try {
      const wasm = new Blob([await wasmFile.arrayBuffer()], { type: 'application/wasm' });
      const manifest = new Blob([await manifestFile.arrayBuffer()], { type: 'text/plain' });
      const readme = readmeText.trim()
        ? new Blob([readmeText], { type: 'text/markdown' })
        : undefined;
      const source = sourceText.trim() ? new Blob([sourceText], { type: 'text/plain' }) : undefined;
      const resp = await publishMarketplaceProbe({
        namespace: namespace.trim(),
        name: name.trim(),
        version: version.trim(),
        description: description.trim() || undefined,
        wasm,
        manifest,
        readme,
        source
      });
      goto(`${base}/marketplace/${resp.probe.id}`);
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      publishing = false;
    }
  }
</script>

<section class="page">
  <a class="back" href="{base}/marketplace">← back to marketplace</a>
  <h1>Upload existing probe</h1>
  <p class="muted">
    For probes you built outside the <a class="link" href="{base}/probes/build">Build probe</a>
    page (e.g., compiled locally with <code class="mono">cargo build --target
    wasm32-unknown-unknown --release</code>). Uploads bypass the server-side
    builder; you provide a pre-compiled <code>.wasm</code> and its
    <code>.probe.toml</code>. Optional README and Rust source are saved
    alongside so installers can review them before clicking <em>install</em>.
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

  <div class="grid">
    <div class="field">
      <label class="label" for="wasm"
        >probe.wasm<span class="req"> *</span></label
      >
      <input id="wasm" class="input" type="file" accept=".wasm,application/wasm" on:change={pickWasm} />
      {#if wasmFile}
        <span class="hint mono">{wasmFile.name} · {wasmFile.size} bytes</span>
      {/if}
    </div>
    <div class="field">
      <label class="label" for="man"
        >probe.toml<span class="req"> *</span></label
      >
      <input id="man" class="input" type="file" accept=".toml,text/*" on:change={pickManifest} />
      {#if manifestFile}
        <span class="hint mono">{manifestFile.name} · {manifestFile.size} bytes</span>
      {/if}
    </div>
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

  <div class="field">
    <label class="label" for="src">source / lib.rs (optional, paste the Rust source)</label>
    <textarea
      id="src"
      class="input mono"
      rows="10"
      bind:value={sourceText}
      placeholder="//! Source the probe was compiled from..."
    ></textarea>
  </div>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}

  <div class="flex flex--between" style="margin-top: 16px;">
    <span class="muted text-sm">
      Per doc 23, the binding key is
      <code class="mono">(publisher_sub, namespace, name, version)</code>.
      Republishing the same version returns 409.
    </span>
    <button class="btn btn--primary" on:click={submit} disabled={!canSubmit}>
      {publishing ? 'publishing…' : 'publish'}
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
  .hint {
    display: inline-block;
    margin-top: 4px;
    font-size: 0.75rem;
    color: var(--fg-soft);
  }
</style>

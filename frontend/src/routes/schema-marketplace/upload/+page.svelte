<script lang="ts">
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { ApiError, publishMarketplaceSchema } from '$lib/api';

  let version = '0.1.0';
  let description = '';
  let schemaText = '';
  let readmeText = '';

  let publishing = false;
  let error: string | null = null;

  $: canSubmit = !publishing && version.trim() !== '' && schemaText.trim() !== '';

  async function submit() {
    if (!canSubmit) return;
    publishing = true;
    error = null;
    try {
      const schema = new Blob([schemaText], { type: 'text/plain' });
      const readme = readmeText.trim()
        ? new Blob([readmeText], { type: 'text/markdown' })
        : undefined;
      const resp = await publishMarketplaceSchema({
        version: version.trim(),
        description: description.trim() || undefined,
        schema,
        readme
      });
      goto(`${base}/schema-marketplace/${resp.schema.id}`);
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      publishing = false;
    }
  }
</script>

<section class="page">
  <a class="back" href="{base}/schema-marketplace">← back to schema marketplace</a>
  <h1>Publish a schema</h1>
  <p class="muted">
    Paste the body of <code class="mono">schema.toml</code>. The
    <code class="mono">file_type</code> in the TOML becomes the schema's
    identifier.
  </p>

  <div class="field">
    <label class="label" for="vr">version<span class="req"> *</span></label>
    <input id="vr" class="input mono" bind:value={version} placeholder="0.1.0" />
  </div>

  <div class="field">
    <label class="label" for="desc">description (optional)</label>
    <input id="desc" class="input" bind:value={description} placeholder="One-line summary" />
  </div>

  <div class="field">
    <label class="label" for="sch">schema.toml<span class="req"> *</span></label>
    <textarea
      id="sch"
      class="input mono"
      rows="16"
      bind:value={schemaText}
      placeholder={`file_type = "text"\nmime_patterns = ["text/*"]\n\n[fields.byte_size]\nsource = "probe"\nprobe = "file.size"\ntype = "int"`}
    ></textarea>
  </div>

  <div class="field">
    <label class="label" for="rd">README.md (optional, markdown)</label>
    <textarea
      id="rd"
      class="input mono"
      rows="6"
      bind:value={readmeText}
      placeholder="## What this schema captures..."
    ></textarea>
  </div>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}

  <div class="flex flex--between" style="margin-top: 16px;">
    <span class="muted text-sm">
      The binding key is
      <code class="mono">(publisher_sub, file_type, version)</code>.
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
  .field {
    margin-bottom: 16px;
  }
  .req {
    color: var(--danger, #c33);
  }
</style>

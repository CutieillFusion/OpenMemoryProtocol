<script lang="ts">
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import {
    uploadFile,
    startUpload,
    appendUpload,
    commitUpload,
    cancelUpload,
    ApiError
  } from '$lib/api';
  import type { AddResult } from '$lib/types';
  import { formatBytes } from '$lib/format';

  // Files larger than this are routed through the resumable session API to
  // stay under the gateway's 32 MiB request body cap.
  const RESUMABLE_THRESHOLD = 20 * 1024 * 1024;

  let path = '';
  let fileType = '';
  let fields: Array<{ k: string; v: string }> = [];
  let file: File | null = null;
  let result: AddResult | null = null;
  let error: string | null = null;
  let uploading = false;
  let progress = 0;
  let uploadIdState: string | null = null;
  let abortFlag = false;

  function pickFile(e: Event) {
    const t = e.currentTarget as HTMLInputElement;
    file = t.files && t.files.length > 0 ? t.files[0] : null;
    if (file && !path) path = file.name;
  }

  function addField() {
    fields = [...fields, { k: '', v: '' }];
  }
  function removeField(i: number) {
    fields = fields.filter((_, idx) => idx !== i);
  }

  function fieldsObj(): Record<string, string> {
    const o: Record<string, string> = {};
    for (const { k, v } of fields) {
      if (k.trim()) o[k.trim()] = v;
    }
    return o;
  }

  async function onSubmit() {
    if (!file || !path) return;
    error = null;
    result = null;
    uploading = true;
    progress = 0;
    abortFlag = false;
    try {
      if (file.size <= RESUMABLE_THRESHOLD) {
        result = await uploadFile({
          path,
          file,
          file_type: fileType || undefined,
          fields: fieldsObj()
        });
      } else {
        result = await resumableUpload();
      }
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      uploading = false;
      uploadIdState = null;
    }
  }

  async function resumableUpload(): Promise<AddResult> {
    if (!file) throw new Error('no file');
    const handle = await startUpload(file.size);
    uploadIdState = handle.upload_id;
    const chunkSize = Number(handle.chunk_size_bytes) || 4 * 1024 * 1024;
    let offset = 0;
    while (offset < file.size) {
      if (abortFlag) {
        await cancelUpload(handle.upload_id);
        throw new Error('upload cancelled');
      }
      const end = Math.min(offset + chunkSize, file.size);
      const chunk = file.slice(offset, end);
      await appendUpload(handle.upload_id, offset, chunk);
      offset = end;
      progress = offset / file.size;
    }
    const fieldEntries = fields.filter((f) => f.k.trim());
    const fmap: Record<string, string> = {};
    for (const { k, v } of fieldEntries) fmap[k.trim()] = v;
    return commitUpload(handle.upload_id, {
      path,
      file_type: fileType || undefined,
      fields: Object.keys(fmap).length > 0 ? fmap : undefined
    });
  }

  function cancel() {
    abortFlag = true;
  }

  function onDrop(e: DragEvent) {
    e.preventDefault();
    if (e.dataTransfer?.files && e.dataTransfer.files.length > 0) {
      file = e.dataTransfer.files[0];
      if (!path) path = file.name;
    }
  }
  function onDragOver(e: DragEvent) {
    e.preventDefault();
  }
</script>

<main class="page-container">
  <h1 class="page-title">Upload</h1>
  <div class="page-sub">
    Stage a new file. Small files (&lt; {formatBytes(RESUMABLE_THRESHOLD)}) go through
    a single multipart POST. Larger files use the resumable session API
    automatically.
  </div>

  {#if error}<div class="error-banner">{error}</div>{/if}

  <div
    class="dropzone"
    class:has-file={file !== null}
    on:drop={onDrop}
    on:dragover={onDragOver}
    role="region"
    aria-label="File drop zone"
  >
    {#if file}
      <div class="mono">{file.name}</div>
      <div class="text-sm soft">{formatBytes(file.size)}</div>
      {#if file.size > RESUMABLE_THRESHOLD}
        <span class="tag tag--accent">resumable</span>
      {/if}
    {:else}
      <div class="soft">drop a file here, or pick one below</div>
    {/if}
    <label class="btn" style="margin-top: 12px;">
      <input type="file" on:change={pickFile} hidden />
      choose file
    </label>
  </div>

  <form on:submit|preventDefault={onSubmit} class="stack--lg" style="margin-top: 24px;">
    <div class="field">
      <label class="label" for="p">repo path</label>
      <input id="p" class="input mono" bind:value={path} placeholder="reports/q3.pdf" required />
    </div>

    <div class="field">
      <label class="label" for="ft">file_type (optional, defaults to inferred)</label>
      <input id="ft" class="input mono" bind:value={fileType} placeholder="pdf" />
    </div>

    <div class="field">
      <div class="label">user fields (optional)</div>
      {#each fields as f, i (i)}
        <div class="field-row">
          <input class="input mono" placeholder="key" bind:value={fields[i].k} />
          <input class="input mono" placeholder="value" bind:value={fields[i].v} />
          <button type="button" class="btn btn--ghost" on:click={() => removeField(i)}>−</button>
        </div>
      {/each}
      <button type="button" class="btn btn--ghost text-sm" on:click={addField}>+ add field</button>
    </div>

    <div class="flex flex--between">
      <span class="muted text-sm">
        {#if uploading && file && file.size > RESUMABLE_THRESHOLD}
          uploading… {Math.round(progress * 100)}%
        {/if}
      </span>
      <div class="flex" style="gap: 8px;">
        {#if uploading && file && file.size > RESUMABLE_THRESHOLD}
          <button type="button" class="btn btn--danger" on:click={cancel}>cancel</button>
        {/if}
        <button type="submit" class="btn btn--primary" disabled={!file || !path || uploading}>
          {uploading ? 'uploading…' : 'upload'}
        </button>
      </div>
    </div>
    {#if uploading && file && file.size > RESUMABLE_THRESHOLD}
      <div class="progress"><div class="progress-bar" style="width: {progress * 100}%"></div></div>
    {/if}
  </form>

  {#if result}
    <section class="page-section">
      <h2>Result</h2>
      {#if result.kind === 'manifest'}
        <div class="muted">
          Staged manifest at <a href="{base}/file/{result.path}">/{result.path}</a>.
          Visit <a href="{base}/commit"><span style="color: var(--accent); text-decoration: underline;">/commit</span></a> to make it durable.
        </div>
      {:else}
        <div class="muted">Staged blob at <code>{result.path}</code> ({formatBytes(result.size)}).</div>
      {/if}
      <pre class="bytes-pre mono">{JSON.stringify(result, null, 2)}</pre>
    </section>
  {/if}
</main>

<style>
  .dropzone {
    border: 2px dashed var(--border);
    border-radius: 12px;
    padding: 32px;
    text-align: center;
    background: var(--bg-soft);
    transition: border-color 0.15s, background 0.15s;
  }
  .dropzone.has-file {
    border-color: var(--accent);
    background: rgba(39, 71, 212, 0.03);
  }
  .field-row {
    display: flex;
    gap: 8px;
    margin-bottom: 8px;
  }
  .progress {
    height: 4px;
    background: var(--border);
    border-radius: 2px;
    overflow: hidden;
  }
  .progress-bar {
    height: 100%;
    background: var(--accent);
    transition: width 0.2s;
  }
  .bytes-pre {
    padding: 12px 16px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-soft);
    overflow-x: auto;
    font-size: 0.8rem;
  }
</style>

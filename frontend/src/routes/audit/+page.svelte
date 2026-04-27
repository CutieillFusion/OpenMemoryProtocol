<script lang="ts">
  import { onMount } from 'svelte';
  import { audit, ApiError } from '$lib/api';
  import type { AuditEntry } from '$lib/types';
  import { shortHash, formatTimestamp, relativeTime } from '$lib/format';

  let entries: AuditEntry[] = [];
  let verified = true;
  let limit = 200;
  let loading = false;
  let error: string | null = null;

  async function load() {
    loading = true;
    error = null;
    try {
      const r = await audit({ limit });
      entries = r.entries;
      verified = r.verified;
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(load);

  function describeDetails(d: Record<string, unknown>): string {
    const keys = Object.keys(d);
    if (keys.length === 0) return '';
    return keys
      .slice(0, 4)
      .map((k) => `${k}=${typeof d[k] === 'string' ? d[k] : JSON.stringify(d[k])}`)
      .join(' · ');
  }
</script>

<main class="page-container page-container--wide">
  <h1 class="page-title">Audit</h1>
  <div class="page-sub">
    Hash-chain audit log. Each entry references its parent's hash;
    tampering breaks the chain and the response is flagged as
    <code>verified=false</code>.
  </div>

  {#if error}<div class="error-banner">{error}</div>{/if}

  <div class="flex flex--between" style="margin-bottom: 24px;">
    <div>
      {#if entries.length > 0}
        {#if verified}
          <span class="tag tag--success">chain verified</span>
        {:else}
          <span class="tag tag--danger">CHAIN BROKEN — investigate</span>
        {/if}
        <span class="muted text-sm" style="margin-left: 12px;">{entries.length} entries</span>
      {/if}
    </div>
    <div class="flex" style="gap: 12px; align-items: end;">
      <div class="field" style="width: 100px; margin-bottom: 0;">
        <label class="label" for="lim">limit</label>
        <input id="lim" class="input" type="number" min="1" max="1000" bind:value={limit} />
      </div>
      <button class="btn" on:click={load} disabled={loading}>
        {loading ? 'loading…' : 'reload'}
      </button>
    </div>
  </div>

  {#if entries.length === 0 && !loading}
    <div class="soft">no audit entries</div>
  {:else}
    <table class="table">
      <thead>
        <tr>
          <th>at</th>
          <th>event</th>
          <th>actor</th>
          <th>details</th>
          <th>parent</th>
        </tr>
      </thead>
      <tbody>
        {#each entries as e}
          <tr>
            <td class="mono text-xs" title={formatTimestamp(e.at)}>{relativeTime(e.at)}</td>
            <td><span class="tag">{e.event}</span></td>
            <td class="mono soft text-xs">{e.actor || '—'}</td>
            <td class="mono text-xs" style="max-width: 480px;">{describeDetails(e.details)}</td>
            <td class="mono soft text-xs">{e.parent ? shortHash(e.parent, 10) : '(genesis)'}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</main>

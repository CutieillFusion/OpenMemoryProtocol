<script lang="ts">
  import { onMount } from 'svelte';
  import { base } from '$app/paths';
  import { listBranches, createBranch, checkout, ApiError } from '$lib/api';
  import type { BranchInfo } from '$lib/types';
  import { shortHash } from '$lib/format';

  let branches: BranchInfo[] = [];
  let loading = false;
  let error: string | null = null;
  let newName = '';
  let newStart = '';
  let busy: string | null = null;

  async function load() {
    loading = true;
    error = null;
    try {
      branches = await listBranches();
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(load);

  async function onCreate() {
    if (!newName.trim()) return;
    busy = 'create';
    error = null;
    try {
      await createBranch({ name: newName.trim(), start: newStart.trim() || undefined });
      newName = '';
      newStart = '';
      await load();
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      busy = null;
    }
  }

  async function onCheckout(name: string) {
    if (!confirm(`Checkout '${name}'? Uncommitted staged changes will follow you.`)) return;
    busy = name;
    error = null;
    try {
      await checkout(name);
      await load();
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      busy = null;
    }
  }
</script>

<main class="page-container">
  <h1 class="page-title">Branches</h1>
  <div class="page-sub">
    Refs are content-addressable pointers at commits. Single-writer-per-tenant
    means branch ops serialize per-tenant; conflicts return 409.
  </div>

  {#if error}<div class="error-banner">{error}</div>{/if}

  <section class="page-section">
    <h2>Existing</h2>
    {#if loading}
      <div class="muted">loading…</div>
    {:else if branches.length === 0}
      <div class="soft">no branches</div>
    {:else}
      <table class="table">
        <thead>
          <tr><th>name</th><th>head</th><th></th></tr>
        </thead>
        <tbody>
          {#each branches as b}
            <tr>
              <td>
                <span class="mono">{b.name}</span>
                {#if b.is_current}<span class="tag tag--accent" style="margin-left: 8px;">current</span>{/if}
              </td>
              <td class="mono soft">{shortHash(b.head ?? '', 16)}</td>
              <td style="text-align: right;">
                <button
                  class="btn btn--sm"
                  on:click={() => onCheckout(b.name)}
                  disabled={b.is_current || busy !== null}
                >
                  {busy === b.name ? 'switching…' : 'checkout'}
                </button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </section>

  <section class="page-section">
    <h2>Create</h2>
    <form on:submit|preventDefault={onCreate} class="stack">
      <div class="field">
        <label class="label" for="bn">name</label>
        <input id="bn" class="input mono" bind:value={newName} placeholder="feature/auth" required />
      </div>
      <div class="field">
        <label class="label" for="bs">start ref (optional, defaults to current HEAD)</label>
        <input id="bs" class="input mono" bind:value={newStart} placeholder="main" />
      </div>
      <div>
        <button class="btn btn--primary" type="submit" disabled={!newName.trim() || busy !== null}>
          {busy === 'create' ? 'creating…' : 'create branch'}
        </button>
      </div>
    </form>
  </section>
</main>

<style>
  .btn--sm {
    padding: 4px 10px;
    font-size: 0.8rem;
  }
</style>

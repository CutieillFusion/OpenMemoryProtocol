<script lang="ts">
  import { onMount } from 'svelte';
  import { ApiError, getWidgetToken, type WidgetTokenResponse } from '$lib/api';

  // The WorkOS API Keys widget is loaded lazily — both the script and the
  // widget-token mint can fail independently and we want to surface each
  // failure mode to the user rather than blanking the page.

  let widgetTokenResp: WidgetTokenResponse | null = null;
  let loadError: string | null = null;
  let widgetLoadError: string | null = null;
  let mountEl: HTMLDivElement;
  let loading = true;

  onMount(async () => {
    try {
      widgetTokenResp = await getWidgetToken();
    } catch (e) {
      loadError = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
      loading = false;
      return;
    }
    loading = false;

    try {
      await loadScriptOnce('https://widgets.workos.com/widgets.umd.js');
      const widgets = (window as any).WorkOSWidgets;
      if (!widgets || typeof widgets.mount !== 'function') {
        widgetLoadError =
          'WorkOS widget script unavailable. Verify the embed snippet against https://workos.com/docs/widgets/api-keys.';
        return;
      }
      widgets.mount({
        type: 'api-keys',
        target: mountEl,
        token: widgetTokenResp.token,
        organizationId: widgetTokenResp.organization_id
      });
    } catch (e) {
      widgetLoadError = e instanceof Error ? e.message : String(e);
    }
  });

  function loadScriptOnce(src: string): Promise<void> {
    return new Promise((resolve, reject) => {
      const existing = document.querySelector(`script[data-src="${src}"]`);
      if (existing) {
        resolve();
        return;
      }
      const s = document.createElement('script');
      s.src = src;
      s.async = true;
      s.dataset.src = src;
      s.onload = () => resolve();
      s.onerror = () => reject(new Error(`failed to load ${src}`));
      document.head.appendChild(s);
    });
  }
</script>

<svelte:head>
  <title>API keys — OMP</title>
</svelte:head>

<section class="api-keys-page">
  <header class="page-head">
    <h1>API keys</h1>
    <p class="muted">
      Mint and manage API keys for your organization. Keys present as
      <code>Authorization: Bearer &lt;key&gt;</code> on every API call.
    </p>
  </header>

  {#if loading}
    <p class="muted">Loading…</p>
  {:else if loadError}
    <div class="error">
      Couldn't fetch a widget session token: {loadError}
      {#if loadError.includes('no_organization')}
        <p class="hint">
          API keys are scoped to organizations. Your account isn't
          attached to one yet — sign in via an org-bound flow first.
        </p>
      {/if}
    </div>
  {:else}
    <div bind:this={mountEl} class="widget-mount" data-token-len={widgetTokenResp?.token.length ?? 0}>
      {#if widgetLoadError}
        <div class="error">
          Widget didn't mount: {widgetLoadError}
        </div>
      {/if}
    </div>
  {/if}
</section>

<style>
  .api-keys-page {
    max-width: 720px;
    margin: 32px auto;
    padding: 0 16px;
  }
  .page-head h1 {
    margin: 0 0 4px;
    font-size: 1.4rem;
  }
  .muted {
    color: var(--fg-muted);
    font-size: 0.9rem;
  }
  .widget-mount {
    margin-top: 24px;
    min-height: 240px;
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 16px;
  }
  .error {
    color: var(--danger, #c33);
    font-size: 0.9rem;
    line-height: 1.4;
  }
  .hint {
    margin-top: 8px;
    color: var(--fg-muted);
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
    background: rgba(17, 17, 17, 0.05);
    padding: 1px 4px;
    border-radius: 3px;
  }
</style>

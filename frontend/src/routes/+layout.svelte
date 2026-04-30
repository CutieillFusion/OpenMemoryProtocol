<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { page } from '$app/stores';
  import { base } from '$app/paths';
  import { auth, probeAuth, startWorkosLogin } from '$lib/auth';
  import { watch, type WatchHandle } from '$lib/sse';
  import type { WatchEvent } from '$lib/types';
  import { relativeTime } from '$lib/format';
  import ProfileMenu from '$lib/components/ProfileMenu.svelte';
  import '../app.css';

  let watchHandle: WatchHandle | null = null;
  let events: WatchEvent[] = [];
  let panelOpen = true;
  let healthOk = true;

  // Web UI is WorkOS-only. The bearer-token "paste a token" modal has been
  // removed because it's a phishing-shaped UX; the browser surface is WorkOS
  // exclusively. The CLI and any machine client still use
  // `Authorization: Bearer <token>` against the gateway — that path is
  // server-side and untouched by these changes.
  $: showWorkosGate = $auth.mode === 'workos';
  // A deployment in token-mode (`auth_mode: "token"` from /status) means
  // the operator hasn't configured WorkOS for browser users. Render an
  // explanatory page rather than the bearer-paste modal — the browser
  // user has no way to authenticate here, and pretending otherwise was
  // the bug behind "missing or unknown bearer token".
  $: showWorkosUnconfigured = $auth.mode === 'token-required';
  $: showLoginGate = showWorkosGate || showWorkosUnconfigured;

  const navLinks = [
    { href: '/', label: 'Tree' },
    { href: '/upload', label: 'Upload' },
    { href: '/probes/build', label: 'Build probe' },
    { href: '/marketplace', label: 'Probes' },
    { href: '/schema-marketplace', label: 'Schemas' },
    { href: '/commit', label: 'Commit' },
    { href: '/branches', label: 'Branches' },
    { href: '/query', label: 'Query' },
    { href: '/log', label: 'Log' },
    { href: '/audit', label: 'Audit' }
  ];

  $: currentPath = $page.url.pathname.replace(base, '') || '/';

  function isActive(href: string): boolean {
    if (href === '/') return currentPath === '/';
    return currentPath === href || currentPath.startsWith(href + '/');
  }

  onMount(async () => {
    await probeAuth();
    startWatch();
  });

  function startWatch() {
    watchHandle?.close();
    watchHandle = watch({
      onEvent: (e) => {
        events = [e, ...events].slice(0, 25);
      },
      onOpen: () => {
        healthOk = true;
      },
      onError: () => {
        healthOk = false;
      }
    });
  }

  onDestroy(() => {
    watchHandle?.close();
  });

  // Logout is exclusively WorkOS now (the form below POSTs to /auth/logout).
</script>

<div class="app">
  <nav class="site-nav">
    <div class="nav-inner">
      <a class="nav-logo" href="{base}/">OMP</a>
      <ul class="nav-links">
        {#each navLinks as link}
          <li>
            <a
              class="nav-link"
              class:active={isActive(link.href)}
              href={base + link.href}
            >
              {link.label}
            </a>
          </li>
        {/each}
      </ul>
      <div class="nav-spacer"></div>
      <span class="auth-badge">
        {#if $auth.mode === 'no-auth'}
          <span class="tag">no-auth</span>
        {:else if $auth.mode === 'session'}
          <ProfileMenu />
        {:else if $auth.mode === 'workos'}
          <span class="tag tag--danger">signed out</span>
        {:else if $auth.mode === 'token-required'}
          <span class="tag tag--danger" title="WorkOS not configured for this deployment">no auth configured</span>
        {/if}
      </span>
    </div>
  </nav>

  <slot />

  {#if !showLoginGate}
    <aside class="watch-panel" class:closed={!panelOpen}>
      <header>
        <button
          class="watch-toggle"
          on:click={() => (panelOpen = !panelOpen)}
          aria-label="Toggle activity panel"
        >
          <span class="dot" class:off={!healthOk}></span>
          Activity
          <span class="watch-count">{events.length}</span>
          <span class="caret">{panelOpen ? '▾' : '▴'}</span>
        </button>
      </header>
      {#if panelOpen}
        <div class="watch-list">
          {#if events.length === 0}
            <div class="watch-empty">no events yet</div>
          {:else}
            {#each events as e (e.occurred_at + e.type + e.trace_id)}
              <div class="watch-item">
                <div class="watch-type">{e.type}</div>
                <div class="watch-meta">
                  <span class="mono">{e.tenant}</span>
                  <span class="soft">{relativeTime(e.occurred_at)}</span>
                </div>
              </div>
            {/each}
          {/if}
        </div>
      {/if}
    </aside>
  {/if}

  {#if showWorkosGate}
    <div class="modal-overlay">
      <div class="modal">
        <h2>Sign in</h2>
        <p class="muted text-sm">
          This deployment uses WorkOS AuthKit. Click below to authenticate
          with your organization's identity provider.
        </p>
        <div class="flex flex--between" style="margin-top: 16px">
          <span class="text-xs soft">Hosted login at <code>auth.workos.com</code>.</span>
          <button class="btn btn--primary" type="button" on:click={() => startWorkosLogin(currentPath)}>
            Sign in with WorkOS
          </button>
        </div>
      </div>
    </div>
  {/if}

  {#if showWorkosUnconfigured}
    <div class="modal-overlay">
      <div class="modal">
        <h2>Browser sign-in not configured</h2>
        <p class="muted text-sm">
          This deployment of OpenMemoryProtocol doesn't have WorkOS auth
          configured for browser users. The web UI is WorkOS-only — there's
          no way to sign in here until an operator enables it.
        </p>
        <p class="muted text-sm">
          If you're an operator: set <code>[workos]</code> in your gateway
          config and restart the gateway. Until then, machine clients (the CLI,
          CI jobs) can still hit the API directly with
          <code>Authorization: Bearer &lt;token&gt;</code> against a
          tenant token registered in <code>tenants.toml</code>.
        </p>
      </div>
    </div>
  {/if}
</div>

<style>
  .site-nav {
    position: sticky;
    top: 0;
    z-index: 100;
    height: var(--nav-height);
    background: rgba(250, 250, 250, 0.85);
    backdrop-filter: blur(12px);
    -webkit-backdrop-filter: blur(12px);
    border-bottom: 1px solid var(--border);
  }

  .nav-inner {
    max-width: var(--max-width-wide);
    height: 100%;
    margin: 0 auto;
    padding: 0 24px;
    display: flex;
    align-items: center;
    gap: 24px;
  }

  .nav-logo {
    font-family: var(--font-mono);
    font-size: 0.95rem;
    font-weight: 600;
    color: var(--fg);
    letter-spacing: -0.02em;
  }

  .nav-links {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    gap: 4px;
  }

  .nav-link {
    display: inline-block;
    font-size: 0.9rem;
    color: var(--fg-muted);
    padding: 6px 12px;
    border-radius: 6px;
    transition: color 0.15s, background 0.15s;
  }

  .nav-link:hover {
    color: var(--fg);
    background: rgba(17, 17, 17, 0.04);
  }

  .nav-link.active {
    color: var(--fg);
    background: rgba(17, 17, 17, 0.06);
  }

  .nav-spacer {
    flex: 1;
  }

  .auth-badge {
    display: inline-flex;
    align-items: center;
  }

  .btn--sm {
    padding: 4px 8px;
    font-size: 0.8rem;
  }

  .watch-panel {
    position: fixed;
    bottom: 16px;
    right: 16px;
    width: 280px;
    max-height: 60vh;
    background: var(--bg-elevated);
    border: 1px solid var(--border);
    border-radius: 10px;
    box-shadow: 0 1px 0 rgba(17, 17, 17, 0.04);
    z-index: 50;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }

  .watch-panel.closed {
    width: auto;
  }

  .watch-toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    background: none;
    border: 0;
    width: 100%;
    padding: 10px 14px;
    font-size: 0.85rem;
    color: var(--fg-muted);
    text-align: left;
  }

  .watch-toggle:hover {
    background: rgba(17, 17, 17, 0.03);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--success);
  }

  .dot.off {
    background: var(--fg-soft);
  }

  .watch-count {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--fg-soft);
  }

  .caret {
    margin-left: auto;
    color: var(--fg-soft);
  }

  .watch-list {
    flex: 1;
    overflow-y: auto;
    border-top: 1px solid var(--border);
  }

  .watch-empty {
    padding: 16px;
    color: var(--fg-soft);
    font-size: 0.85rem;
    text-align: center;
  }

  .watch-item {
    padding: 10px 14px;
    border-bottom: 1px solid var(--border);
  }

  .watch-item:last-child {
    border-bottom: 0;
  }

  .watch-type {
    font-family: var(--font-mono);
    font-size: 0.85rem;
    color: var(--fg);
    margin-bottom: 2px;
  }

  .watch-meta {
    display: flex;
    gap: 8px;
    font-size: 0.75rem;
    color: var(--fg-soft);
  }

  @media (max-width: 720px) {
    .nav-inner {
      padding: 0 16px;
      gap: 12px;
      overflow-x: auto;
    }
    .nav-links {
      flex-shrink: 0;
    }
    .watch-panel {
      width: calc(100vw - 32px);
      max-height: 40vh;
    }
  }
</style>

<script lang="ts">
  import { onMount } from 'svelte';
  import { ApiError, getMe, type MeResponse } from '$lib/api';

  let me: MeResponse | null = null;
  let loading = true;
  let loadError: string | null = null;
  let open = false;

  let menu: HTMLDivElement;

  onMount(async () => {
    try {
      me = await getMe();
    } catch (e) {
      loadError = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  });

  function toggle() {
    open = !open;
  }

  function close() {
    open = false;
  }

  function onDocClick(e: MouseEvent) {
    if (!open) return;
    if (menu && !menu.contains(e.target as Node)) {
      open = false;
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === 'Escape' && open) {
      open = false;
    }
  }

  $: displayName = (() => {
    if (!me) return '…';
    const fn = me.first_name?.trim();
    const ln = me.last_name?.trim();
    if (fn && ln) return `${fn} ${ln}`;
    if (fn) return fn;
    if (me.email) return me.email;
    return me.sub;
  })();

  $: initials = (() => {
    if (!me) return '?';
    const fn = me.first_name?.trim();
    const ln = me.last_name?.trim();
    if (fn && ln) return (fn[0] + ln[0]).toUpperCase();
    if (fn) return fn.slice(0, 2).toUpperCase();
    if (me.email) return me.email.slice(0, 2).toUpperCase();
    return me.sub.slice(0, 2).toUpperCase();
  })();
</script>

<svelte:window on:click={onDocClick} on:keydown={onKey} />

<div class="profile-menu" bind:this={menu}>
  <button
    class="profile-trigger"
    type="button"
    aria-haspopup="true"
    aria-expanded={open}
    on:click|stopPropagation={toggle}
  >
    {#if me?.profile_picture_url}
      <img class="avatar" src={me.profile_picture_url} alt="" referrerpolicy="no-referrer" />
    {:else}
      <span class="avatar avatar--initials">{initials}</span>
    {/if}
    <span class="trigger-name">{loading ? '…' : displayName}</span>
    <span class="caret">▾</span>
  </button>

  {#if open}
    <div class="dropdown" role="menu">
      {#if loadError}
        <div class="dropdown-error">couldn't load profile: {loadError}</div>
      {:else if me}
        <div class="profile-head">
          {#if me.profile_picture_url}
            <img class="avatar avatar--lg" src={me.profile_picture_url} alt="" referrerpolicy="no-referrer" />
          {:else}
            <span class="avatar avatar--lg avatar--initials">{initials}</span>
          {/if}
          <div class="profile-meta">
            <div class="profile-name">{displayName}</div>
            {#if me.email}
              <div class="profile-email">
                {me.email}
                {#if me.email_verified === false}
                  <span class="tag tag--danger" style="margin-left: 6px;">unverified</span>
                {/if}
              </div>
            {/if}
          </div>
        </div>
      {/if}

      <div class="dropdown-actions">
        <a class="dropdown-link" href="/ui/settings/api-keys" on:click={close}>
          API keys
        </a>
        <form method="POST" action="/auth/logout" style="margin: 0;">
          <button class="btn btn--danger btn--block" type="submit" on:click={close}>
            Sign out
          </button>
        </form>
      </div>
    </div>
  {/if}
</div>

<style>
  .profile-menu {
    position: relative;
    display: inline-block;
  }
  .profile-trigger {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    background: none;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 4px 10px 4px 4px;
    cursor: pointer;
    font: inherit;
    color: var(--fg);
  }
  .profile-trigger:hover {
    background: rgba(17, 17, 17, 0.04);
  }
  .avatar {
    width: 26px;
    height: 26px;
    border-radius: 50%;
    object-fit: cover;
    flex-shrink: 0;
  }
  .avatar--initials {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: var(--accent, #2747d4);
    color: #fff;
    font-size: 0.7rem;
    font-weight: 600;
    letter-spacing: 0.5px;
  }
  .avatar--lg {
    width: 44px;
    height: 44px;
    font-size: 0.95rem;
  }
  .trigger-name {
    font-size: 0.8rem;
    max-width: 160px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .caret {
    color: var(--fg-soft);
    font-size: 0.7rem;
  }

  .dropdown {
    position: absolute;
    top: calc(100% + 6px);
    right: 0;
    width: 280px;
    background: var(--bg-elevated, #fff);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: 0 8px 24px rgba(17, 17, 17, 0.08);
    z-index: 1000;
    overflow: hidden;
  }
  .profile-head {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 14px;
    border-bottom: 1px solid var(--border);
  }
  .profile-meta {
    min-width: 0;
    flex: 1;
  }
  .profile-name {
    font-weight: 600;
    font-size: 0.9rem;
    color: var(--fg);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .profile-email {
    font-size: 0.8rem;
    color: var(--fg-muted);
    margin-top: 2px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dropdown-actions {
    padding: 10px 14px;
  }
  .dropdown-link {
    display: block;
    padding: 8px 10px;
    margin-bottom: 8px;
    border-radius: 6px;
    color: var(--fg);
    text-decoration: none;
    font-size: 0.85rem;
    border: 1px solid var(--border);
  }
  .dropdown-link:hover {
    background: rgba(17, 17, 17, 0.04);
  }
  .dropdown-error {
    padding: 14px;
    font-size: 0.85rem;
    color: var(--danger, #c33);
  }
  .btn--block {
    width: 100%;
  }
</style>

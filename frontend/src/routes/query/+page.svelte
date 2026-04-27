<script lang="ts">
  import { onMount } from 'svelte';
  import { base } from '$app/paths';
  import { query as runQuery, ApiError } from '$lib/api';
  import type { QueryResult } from '$lib/types';
  import { describeField, fieldKind, shortHash } from '$lib/format';

  const HISTORY_KEY = 'omp.queryHistory';
  const HISTORY_MAX = 12;

  let where = '';
  let prefix = '';
  let at = '';
  let limit = 100;
  let cursor: string | null = null;
  let result: QueryResult | null = null;
  let loading = false;
  let error: string | null = null;
  let history: string[] = [];

  function loadHistory() {
    if (typeof localStorage === 'undefined') return;
    try {
      const raw = localStorage.getItem(HISTORY_KEY);
      if (raw) history = JSON.parse(raw) as string[];
    } catch {
      history = [];
    }
  }

  function pushHistory(q: string) {
    if (!q.trim()) return;
    history = [q, ...history.filter((h) => h !== q)].slice(0, HISTORY_MAX);
    if (typeof localStorage !== 'undefined') {
      localStorage.setItem(HISTORY_KEY, JSON.stringify(history));
    }
  }

  onMount(loadHistory);

  async function run(append = false) {
    loading = true;
    error = null;
    if (!append) {
      cursor = null;
      result = null;
    }
    try {
      const r = await runQuery({
        where: where.trim() || undefined,
        prefix: prefix.trim() || undefined,
        at: at.trim() || undefined,
        cursor: append ? cursor ?? undefined : undefined,
        limit
      });
      if (append && result) {
        result = { matches: [...result.matches, ...r.matches], next_cursor: r.next_cursor };
      } else {
        result = r;
        if (where.trim()) pushHistory(where.trim());
      }
      cursor = r.next_cursor;
    } catch (e) {
      error = e instanceof ApiError ? `${e.code}: ${e.message}` : String(e);
    } finally {
      loading = false;
    }
  }

  function pickHistory(q: string) {
    where = q;
    run();
  }

  function pickExample(q: string) {
    where = q;
    if (typeof window !== 'undefined') {
      window.scrollTo({ top: 0, behavior: 'smooth' });
    }
  }

  // Determine column set from union of all match field names. Cap at 8 cols
  // so wide fields don't blow up the table.
  $: cols = (() => {
    if (!result) return [] as string[];
    const set = new Set<string>();
    for (const m of result.matches) {
      for (const k of Object.keys(m.fields)) set.add(k);
    }
    return Array.from(set).slice(0, 8);
  })();
</script>

<main class="page-container page-container--wide">
  <h1 class="page-title">Query</h1>
  <div class="page-sub">
    Predicate filtering over manifest fields. Walks the tree at <code>HEAD</code>
    (or <code>?at</code>), parses each manifest, and returns matches.
  </div>

  <details class="docs-panel" open>
    <summary class="docs-summary">
      <span>Query language reference</span>
      <span class="docs-summary-hint">click to collapse</span>
    </summary>

    <div class="docs-grid">
      <section class="docs-section">
        <h3>Field paths</h3>
        <p class="docs-prose">
          Reference fields by name. Use dotted notation for nested objects.
        </p>
        <pre class="docs-code">file_type
pages
tags
source.mime
metadata.author.name</pre>
      </section>

      <section class="docs-section">
        <h3>Operators</h3>
        <table class="docs-table">
          <thead>
            <tr><th>Op</th><th>Works on</th><th>Example</th></tr>
          </thead>
          <tbody>
            <tr><td><code>=</code></td><td>any scalar</td><td><code>file_type = "pdf"</code></td></tr>
            <tr><td><code>!=</code></td><td>any scalar</td><td><code>file_type != "audio"</code></td></tr>
            <tr><td><code>&lt;</code> <code>&lt;=</code> <code>&gt;</code> <code>&gt;=</code></td><td>numbers, datetimes</td><td><code>pages &gt; 10</code></td></tr>
            <tr><td><code>contains</code></td><td>strings (substring) <span class="docs-or">or</span> lists (membership)</td><td><code>tags contains "policy"</code><br><code>title contains "Q3"</code></td></tr>
            <tr><td><code>starts_with</code></td><td>strings only</td><td><code>author starts_with "Alice"</code></td></tr>
            <tr><td><code>exists(field)</code></td><td>any field — true if non-null</td><td><code>exists(transcript)</code></td></tr>
          </tbody>
        </table>
      </section>

      <section class="docs-section">
        <h3>Combining</h3>
        <p class="docs-prose">
          Combine atoms with <code>AND</code>, <code>OR</code>, <code>NOT</code>. Group with parentheses.
          Operator names are case-sensitive (uppercase).
        </p>
        <pre class="docs-code">file_type = "pdf" AND pages &gt; 10
tags contains "policy" OR tags contains "draft"
NOT file_type = "audio"
file_type = "pdf" AND (pages &gt; 10 OR tags contains "draft")</pre>
      </section>

      <section class="docs-section">
        <h3>Literals</h3>
        <ul class="docs-list">
          <li><strong>Strings</strong>: double-quoted — <code>"hello"</code></li>
          <li><strong>Integers</strong>: <code>10</code>, <code>-3</code></li>
          <li><strong>Floats</strong>: <code>3.14</code></li>
          <li><strong>Booleans</strong>: <code>true</code>, <code>false</code></li>
          <li><strong>Null</strong>: <code>null</code></li>
        </ul>
      </section>

      <section class="docs-section">
        <h3>Filters &amp; pagination</h3>
        <ul class="docs-list">
          <li><code>prefix</code> — restrict the walk to a path prefix (e.g. <code>reports/2026/</code>).</li>
          <li><code>at</code> — time-travel to a branch, commit hash, or <code>HEAD~n</code>.</li>
          <li><code>limit</code> — page size, default 100, max 1000.</li>
          <li>Pages auto-cap at 1 MB serialized; click <em>load more</em> to follow the cursor.</li>
        </ul>
      </section>

      <section class="docs-section">
        <h3>Worked examples</h3>
        <ul class="docs-examples">
          <li>
            <button class="example-btn" type="button" on:click={() => pickExample('file_type = "text"')}>file_type = "text"</button>
            <span class="docs-prose">All text-typed manifests.</span>
          </li>
          <li>
            <button class="example-btn" type="button" on:click={() => pickExample('file_type = "pdf" AND pages &gt; 10')}>{`file_type = "pdf" AND pages > 10`}</button>
            <span class="docs-prose">PDFs longer than 10 pages.</span>
          </li>
          <li>
            <button class="example-btn" type="button" on:click={() => pickExample('tags contains "policy" AND author starts_with "Alice"')}>{`tags contains "policy" AND author starts_with "Alice"`}</button>
            <span class="docs-prose">Tagged "policy" and authored by someone whose name starts with "Alice".</span>
          </li>
          <li>
            <button class="example-btn" type="button" on:click={() => pickExample('exists(transcript) AND duration_seconds &gt; 600')}>{`exists(transcript) AND duration_seconds > 600`}</button>
            <span class="docs-prose">Transcribed media longer than 10 minutes.</span>
          </li>
          <li>
            <button class="example-btn" type="button" on:click={() => pickExample('NOT file_type = "audio"')}>{`NOT file_type = "audio"`}</button>
            <span class="docs-prose">Everything except audio.</span>
          </li>
        </ul>
      </section>

      <section class="docs-section docs-section--limits">
        <h3>What query does <em>not</em> do</h3>
        <ul class="docs-list">
          <li><strong>No full-text search.</strong> Predicates only see manifest fields, not file bytes. "Find documents that mention 'Argentina'" is out of scope.</li>
          <li><strong>No regex.</strong> Use <code>contains</code> / <code>starts_with</code> instead.</li>
          <li><strong>No SQL.</strong> No <code>JOIN</code>, <code>GROUP BY</code>, <code>ORDER BY</code>, <code>LIKE</code>, or sub-selects. Sort and group client-side.</li>
          <li><strong>No semantic / vector search.</strong> No embeddings.</li>
          <li><strong>No content-hash query.</strong> Walk a ref instead.</li>
          <li><strong>No cross-tenant query.</strong> Each token sees only its tenant's manifests.</li>
          <li><strong>No writes.</strong> Read-only by construction.</li>
        </ul>
      </section>
    </div>
  </details>

  {#if error}<div class="error-banner">{error}</div>{/if}

  <form on:submit|preventDefault={() => run()} class="stack--lg">
    <div class="field">
      <label class="label" for="w">where</label>
      <textarea
        id="w"
        class="textarea"
        bind:value={where}
        placeholder='file_type = "pdf" AND pages > 10'
      ></textarea>
    </div>
    <div class="flex" style="gap: 16px; flex-wrap: wrap;">
      <div class="field" style="flex: 1; min-width: 200px;">
        <label class="label" for="pf">prefix</label>
        <input id="pf" class="input mono" bind:value={prefix} placeholder="reports/" />
      </div>
      <div class="field" style="flex: 1; min-width: 200px;">
        <label class="label" for="at">at</label>
        <input id="at" class="input mono" bind:value={at} placeholder="HEAD" />
      </div>
      <div class="field" style="width: 120px;">
        <label class="label" for="lim">limit</label>
        <input
          id="lim"
          class="input"
          type="number"
          min="1"
          max="1000"
          bind:value={limit}
        />
      </div>
    </div>
    <div class="flex flex--between">
      <span class="muted text-sm">
        {#if result}{result.matches.length} matches{cursor ? ' · more' : ' · final'}{/if}
      </span>
      <button class="btn btn--primary" type="submit" disabled={loading}>
        {loading ? 'querying…' : 'run'}
      </button>
    </div>
  </form>

  {#if history.length > 0}
    <section class="page-section">
      <h2>Recent</h2>
      <div class="flex flex--wrap" style="gap: 6px;">
        {#each history as h}
          <button class="tag" style="cursor: pointer;" on:click={() => pickHistory(h)} title={h}>
            {h.length > 50 ? h.slice(0, 50) + '…' : h}
          </button>
        {/each}
      </div>
    </section>
  {/if}

  {#if result}
    <section class="page-section">
      <h2>Results</h2>
      {#if result.matches.length === 0}
        <div class="soft">no matches</div>
      {:else}
        <div style="overflow-x: auto;">
          <table class="table">
            <thead>
              <tr>
                <th>path</th>
                <th>file_type</th>
                {#each cols as c}<th class="mono">{c}</th>{/each}
                <th>hash</th>
              </tr>
            </thead>
            <tbody>
              {#each result.matches as m}
                <tr>
                  <td><a href="{base}/file/{m.path}{at ? `?at=${encodeURIComponent(at)}` : ''}" class="mono">{m.path}</a></td>
                  <td><span class="tag">{m.file_type}</span></td>
                  {#each cols as c}
                    <td class="mono text-xs">
                      {c in m.fields ? describeField(m.fields[c]) : ''}
                    </td>
                  {/each}
                  <td class="mono soft text-xs">{shortHash(m.manifest_hash, 10)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        {#if cursor}
          <div style="margin-top: 16px; text-align: center;">
            <button class="btn" on:click={() => run(true)} disabled={loading}>
              {loading ? 'loading…' : 'load more'}
            </button>
          </div>
        {/if}
      {/if}
    </section>
  {/if}
</main>

<style>
  .docs-panel {
    margin: 8px 0 24px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elevated);
    overflow: hidden;
  }
  .docs-summary {
    list-style: none;
    cursor: pointer;
    padding: 12px 16px;
    font-weight: 600;
    background: var(--bg-soft);
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
  }
  .docs-summary::-webkit-details-marker {
    display: none;
  }
  .docs-summary::before {
    content: '▾ ';
    color: var(--fg-soft);
    margin-right: 6px;
    display: inline-block;
  }
  .docs-panel:not([open]) .docs-summary::before {
    content: '▸ ';
  }
  .docs-summary-hint {
    font-weight: 400;
    font-size: 0.75rem;
    color: var(--fg-soft);
  }
  .docs-grid {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: 0;
  }
  @media (max-width: 900px) {
    .docs-grid {
      grid-template-columns: 1fr;
    }
  }
  .docs-section {
    padding: 16px 18px;
    border-top: 1px solid var(--border);
    border-right: 1px solid var(--border);
    min-width: 0;
  }
  .docs-section:nth-child(2n) {
    border-right: 0;
  }
  .docs-section h3 {
    margin: 0 0 8px;
    font-size: 0.85rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--fg-muted);
  }
  .docs-prose {
    margin: 0 0 8px;
    font-size: 0.85rem;
    color: var(--fg-muted);
    line-height: 1.55;
  }
  .docs-prose code,
  .docs-section code {
    font-family: var(--font-mono);
    font-size: 0.82em;
    background: var(--bg-soft);
    padding: 1px 5px;
    border-radius: 3px;
  }
  .docs-code {
    margin: 0;
    padding: 10px 12px;
    background: var(--bg-soft);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-family: var(--font-mono);
    font-size: 0.78rem;
    line-height: 1.55;
    overflow-x: auto;
    white-space: pre;
  }
  .docs-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.82rem;
  }
  .docs-table th {
    text-align: left;
    padding: 6px 8px;
    color: var(--fg-soft);
    font-weight: 600;
    border-bottom: 1px solid var(--border);
  }
  .docs-table td {
    padding: 6px 8px;
    border-bottom: 1px solid var(--border);
    vertical-align: top;
  }
  .docs-table tr:last-child td {
    border-bottom: 0;
  }
  .docs-or {
    color: var(--fg-soft);
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin: 0 4px;
  }
  .docs-list {
    margin: 0;
    padding-left: 18px;
    font-size: 0.85rem;
    color: var(--fg-muted);
    line-height: 1.6;
  }
  .docs-list li {
    margin-bottom: 4px;
  }
  .docs-examples {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .docs-examples li {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .example-btn {
    align-self: flex-start;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-soft);
    color: var(--accent);
    font-family: var(--font-mono);
    font-size: 0.78rem;
    padding: 4px 8px;
    cursor: pointer;
    text-align: left;
    max-width: 100%;
    overflow-x: auto;
    white-space: nowrap;
  }
  .example-btn:hover {
    background: rgba(39, 71, 212, 0.06);
    border-color: var(--accent);
  }
  .docs-section--limits {
    background: rgba(179, 38, 30, 0.025);
    grid-column: 1 / -1;
    border-right: 0;
  }
  .docs-section--limits h3 {
    color: var(--danger);
  }
</style>

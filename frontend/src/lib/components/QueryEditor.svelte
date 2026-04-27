<script lang="ts">
  import { onMount, onDestroy, createEventDispatcher } from 'svelte';
  import { EditorState, Compartment } from '@codemirror/state';
  import { EditorView, keymap, placeholder as placeholderExt } from '@codemirror/view';
  import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
  import { autocompletion, completionKeymap } from '@codemirror/autocomplete';
  import { bracketMatching } from '@codemirror/language';
  import { ompQueryLanguage, ompCompletions } from '$lib/queryLang';
  import type { Schema } from '$lib/types';

  /** Bound query text (where + trailing modifiers). */
  export let value: string = '';
  /** Schemas powering autocomplete. Update reactively when fetched. */
  export let schemas: Schema[] = [];
  /** Placeholder shown when the editor is empty. */
  export let placeholder: string = 'file_type = "pdf" AND pages > 10  prefix "reports/" limit 50';
  /** Optional Cmd/Ctrl-Enter handler. Falls through if not provided. */
  export let onSubmit: (() => void) | undefined = undefined;

  const dispatch = createEventDispatcher<{ change: { value: string } }>();
  let host: HTMLDivElement;
  let view: EditorView | null = null;
  const completionCompartment = new Compartment();

  // Track whether the most recent `value` update originated inside the editor,
  // so external assignments don't fight in-flight typing.
  let updatingFromEditor = false;

  function buildExtensions(initialSchemas: Schema[]) {
    return [
      history(),
      bracketMatching(),
      ompQueryLanguage(),
      completionCompartment.of(
        autocompletion({ override: [ompCompletions(initialSchemas)], activateOnTyping: true })
      ),
      placeholderExt(placeholder),
      EditorView.lineWrapping,
      keymap.of([
        {
          key: 'Mod-Enter',
          run: () => {
            if (onSubmit) { onSubmit(); return true; }
            return false;
          },
        },
        ...completionKeymap,
        ...historyKeymap,
        ...defaultKeymap,
      ]),
      EditorView.updateListener.of((u) => {
        if (!u.docChanged) return;
        const next = u.state.doc.toString();
        updatingFromEditor = true;
        value = next;
        dispatch('change', { value: next });
        // Defer the flag reset so a downstream reactive `value` write doesn't
        // try to round-trip into the editor.
        queueMicrotask(() => { updatingFromEditor = false; });
      }),
      EditorView.theme({
        '&': {
          fontFamily: 'var(--font-mono, ui-monospace, monospace)',
          fontSize: '0.95rem',
          backgroundColor: 'var(--bg-elevated, #fff)',
          border: '1px solid var(--border, #ddd)',
          borderRadius: '6px',
        },
        '&.cm-focused': {
          outline: '2px solid var(--accent, #2747d4)',
          outlineOffset: '-1px',
        },
        '.cm-scroller': { fontFamily: 'inherit' },
        '.cm-content': { padding: '10px 12px', minHeight: '64px' },
        '.cm-placeholder': { color: 'var(--fg-soft, #888)' },
      }),
    ];
  }

  onMount(() => {
    view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: value,
        extensions: buildExtensions(schemas),
      }),
    });
    return () => view?.destroy();
  });

  onDestroy(() => view?.destroy());

  // Push external `value` changes back into the editor (e.g. picking from history).
  $: if (view && !updatingFromEditor && value !== view.state.doc.toString()) {
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: value },
    });
  }

  // Re-bind the completion source when schemas change.
  $: if (view) {
    view.dispatch({
      effects: completionCompartment.reconfigure(
        autocompletion({ override: [ompCompletions(schemas)], activateOnTyping: true })
      ),
    });
  }

  export function focus() {
    view?.focus();
  }
</script>

<div bind:this={host} class="omp-query-editor"></div>

<style>
  .omp-query-editor {
    width: 100%;
  }
</style>

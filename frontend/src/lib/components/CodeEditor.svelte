<script lang="ts">
  import { onMount, onDestroy, createEventDispatcher } from 'svelte';
  import { EditorState, Compartment } from '@codemirror/state';
  import { EditorView, keymap, placeholder as placeholderExt } from '@codemirror/view';
  import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
  import {
    StreamLanguage,
    bracketMatching,
    syntaxHighlighting,
    HighlightStyle,
    indentUnit
  } from '@codemirror/language';
  import { tags as t } from '@lezer/highlight';
  import { rust } from '@codemirror/lang-rust';
  import { toml } from '@codemirror/legacy-modes/mode/toml';

  /**
   * Reusable CodeMirror 6 editor with Rust / TOML syntax highlighting.
   * Modeled on `QueryEditor.svelte`; the OMP query editor pattern (host
   * div + EditorView + Compartment for hot-reconfigure + reactive
   * updateListener) is the same. Differences: a `language` prop swaps the
   * language extension, a `disabled` prop drives editable+readOnly, and
   * the highlight style is generic instead of query-language-specific.
   */

  /** Bound editor text. */
  export let value: string = '';
  /** Which language pack to load. */
  export let language: 'rust' | 'toml' = 'rust';
  /** Read-only and non-editable when true. Used during in-flight builds. */
  export let disabled: boolean = false;
  /** Placeholder shown when the editor is empty. */
  export let placeholder: string = '';
  /** CSS min-height for the editor surface. */
  export let minHeight: string = '120px';
  /** Optional Cmd/Ctrl-Enter handler. Falls through if not provided. */
  export let onSubmit: (() => void) | undefined = undefined;

  const dispatch = createEventDispatcher<{ change: { value: string } }>();
  let host: HTMLDivElement;
  let view: EditorView | null = null;
  const editableCompartment = new Compartment();
  const readOnlyCompartment = new Compartment();

  // Track whether the most recent `value` update originated inside the editor,
  // so external assignments don't fight in-flight typing.
  let updatingFromEditor = false;

  // Highlight style. Tag-based (Lezer's `tags`), so it works for both the
  // Rust lezer grammar and the TOML stream-language. Colors pulled from
  // CSS custom properties so it inherits the existing design system.
  const highlightStyle = HighlightStyle.define([
    { tag: [t.keyword, t.modifier, t.controlKeyword], color: '#7548c2', fontWeight: '600' },
    { tag: [t.string, t.special(t.string), t.regexp], color: '#0a7d2e' },
    { tag: [t.number, t.bool, t.null], color: '#b5611a' },
    { tag: [t.lineComment, t.blockComment, t.docComment], color: '#7a7a7a', fontStyle: 'italic' },
    { tag: [t.heading, t.heading1, t.heading2, t.heading3], color: '#1f4ec3', fontWeight: '700' },
    { tag: [t.attributeName, t.propertyName], color: '#3a64a3' },
    { tag: t.attributeValue, color: '#0a7d2e' },
    { tag: [t.typeName, t.className, t.namespace], color: '#5a3aa3' },
    { tag: [t.function(t.variableName), t.function(t.propertyName)], color: '#1f4ec3' },
    { tag: [t.operator, t.punctuation], color: '#444' },
    { tag: [t.macroName, t.meta], color: '#a04515' },
    { tag: t.invalid, color: '#c33', textDecoration: 'underline wavy' }
  ]);

  function languageExtension() {
    return language === 'rust' ? rust() : StreamLanguage.define(toml);
  }

  function buildExtensions() {
    return [
      history(),
      bracketMatching(),
      indentUnit.of('    '),
      languageExtension(),
      syntaxHighlighting(highlightStyle),
      placeholderExt(placeholder),
      EditorView.lineWrapping,
      editableCompartment.of(EditorView.editable.of(!disabled)),
      readOnlyCompartment.of(EditorState.readOnly.of(disabled)),
      keymap.of([
        {
          key: 'Mod-Enter',
          run: () => {
            if (onSubmit) {
              onSubmit();
              return true;
            }
            return false;
          }
        },
        ...historyKeymap,
        ...defaultKeymap
      ]),
      EditorView.updateListener.of((u) => {
        if (!u.docChanged) return;
        const next = u.state.doc.toString();
        updatingFromEditor = true;
        value = next;
        dispatch('change', { value: next });
        queueMicrotask(() => {
          updatingFromEditor = false;
        });
      }),
      EditorView.theme({
        '&': {
          fontFamily: 'var(--font-mono, ui-monospace, monospace)',
          fontSize: '0.85rem',
          backgroundColor: 'var(--bg-elevated, #fff)',
          border: '1px solid var(--border, #ddd)',
          borderRadius: '6px'
        },
        '&.cm-focused': {
          outline: '2px solid var(--accent, #2747d4)',
          outlineOffset: '-1px'
        },
        '&.cm-editor.cm-readonly': {
          backgroundColor: 'var(--bg-soft, #f7f7f7)',
          opacity: '0.85'
        },
        '.cm-scroller': {
          fontFamily: 'inherit',
          minHeight,
          lineHeight: '1.5'
        },
        '.cm-content': { padding: '10px 12px' },
        '.cm-placeholder': { color: 'var(--fg-soft, #888)' },
        '.cm-gutters': {
          backgroundColor: 'transparent',
          borderRight: '1px solid var(--border, #eee)',
          color: 'var(--fg-soft, #aaa)'
        }
      })
    ];
  }

  onMount(() => {
    view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: value,
        extensions: buildExtensions()
      })
    });
    return () => view?.destroy();
  });

  onDestroy(() => view?.destroy());

  // Push external `value` changes back into the editor (e.g. when the
  // parent resets the field).
  $: if (view && !updatingFromEditor && value !== view.state.doc.toString()) {
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: value }
    });
  }

  // Reactively reconfigure editable/readOnly when `disabled` flips.
  $: if (view) {
    view.dispatch({
      effects: [
        editableCompartment.reconfigure(EditorView.editable.of(!disabled)),
        readOnlyCompartment.reconfigure(EditorState.readOnly.of(disabled))
      ]
    });
  }

  /** Imperatively focus the editor — lets a sibling `<label>` click-focus. */
  export function focus() {
    view?.focus();
  }
</script>

<div bind:this={host} class="omp-code-editor"></div>

<style>
  .omp-code-editor {
    width: 100%;
  }
  .omp-code-editor :global(.cm-editor) {
    width: 100%;
  }
</style>

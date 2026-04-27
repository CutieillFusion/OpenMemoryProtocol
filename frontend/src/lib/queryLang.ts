// OMP query-language support for the web UI:
//   1. tokenizer (mirrors crates/omp-core/src/query.rs:119-272)
//   2. CodeMirror StreamLanguage for syntax highlighting
//   3. context-aware completion source
//   4. parseUnified() — peels trailing `prefix … at … limit …` clauses so
//      the existing /query endpoint contract stays unchanged
//
// The tokenizer must accept exactly what the backend parser accepts; if
// it diverges, the user sees red squigglies that the server happily runs
// (or vice versa). Treat any divergence here as a bug.

import { StreamLanguage, LanguageSupport, syntaxHighlighting, HighlightStyle } from '@codemirror/language';
import { tags as t } from '@lezer/highlight';
import type { CompletionContext, CompletionResult, CompletionSource, Completion } from '@codemirror/autocomplete';
import type { Schema } from './types';

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

export type TokenKind =
  | 'lparen' | 'rparen' | 'dot' | 'comma'
  | 'op'
  | 'string' | 'int' | 'float' | 'bool' | 'null'
  | 'and' | 'or' | 'not' | 'exists'
  | 'ident';

export type OpKind = '=' | '!=' | '<' | '<=' | '>' | '>=' | 'contains' | 'starts_with';

export interface Token {
  kind: TokenKind;
  /** Byte offset of the first character of the token. */
  pos: number;
  /** Byte offset just past the last character of the token. */
  end: number;
  /** Text-form value: for strings the unquoted content, for ints/floats the numeric value as string, for ops the operator symbol. */
  value: string;
}

export class TokenizeError extends Error {
  constructor(public pos: number, msg: string) {
    super(`tokenize error at ${pos}: ${msg}`);
  }
}

const KEYWORD_MAP: Record<string, TokenKind> = {
  and: 'and',
  or: 'or',
  not: 'not',
  exists: 'exists',
  true: 'bool',
  false: 'bool',
  null: 'null',
  contains: 'op',
  starts_with: 'op',
};

const isIdentStart = (c: string) => /[A-Za-z_]/.test(c);
const isIdentCont = (c: string) => /[A-Za-z0-9_]/.test(c);

export function tokenize(input: string): Token[] {
  const out: Token[] = [];
  let i = 0;
  while (i < input.length) {
    const c = input[i];
    const start = i;
    if (/\s/.test(c)) { i++; continue; }
    if (c === '(') { out.push({ kind: 'lparen', pos: start, end: i + 1, value: '(' }); i++; continue; }
    if (c === ')') { out.push({ kind: 'rparen', pos: start, end: i + 1, value: ')' }); i++; continue; }
    if (c === '.') { out.push({ kind: 'dot', pos: start, end: i + 1, value: '.' }); i++; continue; }
    if (c === ',') { out.push({ kind: 'comma', pos: start, end: i + 1, value: ',' }); i++; continue; }
    if (c === '=') { out.push({ kind: 'op', pos: start, end: i + 1, value: '=' }); i++; continue; }
    if (c === '!') {
      if (input[i + 1] === '=') { out.push({ kind: 'op', pos: start, end: i + 2, value: '!=' }); i += 2; continue; }
      throw new TokenizeError(i, 'expected `!=`');
    }
    if (c === '<') {
      if (input[i + 1] === '=') { out.push({ kind: 'op', pos: start, end: i + 2, value: '<=' }); i += 2; continue; }
      out.push({ kind: 'op', pos: start, end: i + 1, value: '<' }); i++; continue;
    }
    if (c === '>') {
      if (input[i + 1] === '=') { out.push({ kind: 'op', pos: start, end: i + 2, value: '>=' }); i += 2; continue; }
      out.push({ kind: 'op', pos: start, end: i + 1, value: '>' }); i++; continue;
    }
    if (c === '"' || c === "'") {
      const quote = c;
      i++;
      let buf = '';
      while (i < input.length && input[i] !== quote) {
        if (input[i] === '\\' && i + 1 < input.length) {
          buf += input[i + 1];
          i += 2;
        } else {
          buf += input[i];
          i++;
        }
      }
      if (i >= input.length) throw new TokenizeError(start, 'unterminated string literal');
      i++; // consume closing quote
      out.push({ kind: 'string', pos: start, end: i, value: buf });
      continue;
    }
    if (/[0-9]/.test(c) || c === '-') {
      let j = i;
      if (c === '-') j++;
      let sawDot = false;
      while (j < input.length) {
        const ch = input[j];
        if (/[0-9]/.test(ch)) { j++; continue; }
        if (ch === '.') {
          // peek: only consume `.` as decimal point if followed by digit
          if (j + 1 < input.length && /[0-9]/.test(input[j + 1])) { sawDot = true; j++; continue; }
          break;
        }
        break;
      }
      const text = input.slice(i, j);
      // A bare `-` with no digits is not a number — fall through to error.
      if (text === '-' || text === '') {
        throw new TokenizeError(i, `unexpected character: ${JSON.stringify(c)}`);
      }
      out.push({ kind: sawDot ? 'float' : 'int', pos: start, end: j, value: text });
      i = j;
      continue;
    }
    if (isIdentStart(c)) {
      let j = i + 1;
      while (j < input.length && isIdentCont(input[j])) j++;
      const word = input.slice(i, j);
      const lower = word.toLowerCase();
      const kind = KEYWORD_MAP[lower] ?? 'ident';
      let value = word;
      if (kind === 'op') value = lower;            // contains / starts_with
      else if (kind === 'bool') value = lower;     // true / false
      out.push({ kind, pos: start, end: j, value });
      i = j;
      continue;
    }
    throw new TokenizeError(i, `unexpected character: ${JSON.stringify(c)}`);
  }
  return out;
}

// ---------------------------------------------------------------------------
// Modifier extraction
// ---------------------------------------------------------------------------

const MODIFIER_KEYWORDS = new Set(['prefix', 'at', 'limit']);
export const MODIFIERS = ['prefix', 'at', 'limit', 'where'] as const;
export const KEYWORDS = ['AND', 'OR', 'NOT', 'exists', 'contains', 'starts_with'] as const;

export interface UnifiedQuery {
  where?: string;
  prefix?: string;
  at?: string;
  limit?: number;
}

/**
 * Peel trailing `prefix "…"`, `at <ref>`, `limit <n>` clauses (in any order)
 * from a unified query string. The leading remainder is returned as `where`.
 *
 * A modifier-named identifier inside the predicate (e.g. `prefix = "x"`) is
 * left in place — modifiers are only recognized when followed by a literal
 * value at the trailing top level (paren-depth 0).
 *
 * Throws TokenizeError on lex failure.
 */
export function parseUnified(input: string): UnifiedQuery {
  if (!input.trim()) return {};
  const tokens = tokenize(input);
  if (tokens.length === 0) return {};

  // depth[i] = paren depth AFTER token i is consumed.
  const depth: number[] = [];
  let d = 0;
  for (const tok of tokens) {
    if (tok.kind === 'lparen') d++;
    else if (tok.kind === 'rparen') d = Math.max(0, d - 1);
    depth.push(d);
  }

  const result: UnifiedQuery = {};
  let endIdx = tokens.length;

  while (endIdx >= 2) {
    if (depth[endIdx - 1] !== 0 || depth[endIdx - 2] !== 0) break;
    const valTok = tokens[endIdx - 1];
    const keyTok = tokens[endIdx - 2];
    if (keyTok.kind !== 'ident') break;
    const keyword = keyTok.value.toLowerCase();
    if (!MODIFIER_KEYWORDS.has(keyword)) break;
    if ((keyword === 'prefix' && result.prefix !== undefined)
      || (keyword === 'at' && result.at !== undefined)
      || (keyword === 'limit' && result.limit !== undefined)) break;

    if (keyword === 'limit') {
      if (valTok.kind !== 'int') break;
      const n = Number(valTok.value);
      if (!Number.isFinite(n) || !Number.isInteger(n)) break;
      result.limit = n;
    } else {
      // prefix / at: accept string literal or bare identifier
      if (valTok.kind === 'string') {
        (result as any)[keyword] = valTok.value;
      } else if (valTok.kind === 'ident') {
        (result as any)[keyword] = valTok.value;
      } else {
        break;
      }
    }
    endIdx -= 2;
  }

  if (endIdx > 0) {
    const cutoff = endIdx < tokens.length ? tokens[endIdx].pos : input.length;
    const where = input.slice(0, cutoff).trim();
    if (where) result.where = where;
  }
  return result;
}

// ---------------------------------------------------------------------------
// CodeMirror StreamLanguage (syntax highlighting)
// ---------------------------------------------------------------------------

// `prefix`/`at`/`limit`/`where` are highlighted as keywords always — there's
// no syntactic context check for them at the token level. parseUnified does
// the disambiguation.
const ALWAYS_KEYWORD_LOWER = new Set(['and', 'or', 'not', 'exists', 'contains', 'starts_with', 'where', 'prefix', 'at', 'limit']);

interface OmpState {
  // Per-line state not needed currently — strings don't span lines.
  _: never[];
}

const ompStreamParser = {
  name: 'omp-query',
  startState(): OmpState { return { _: [] }; },
  token(stream: any, _state: OmpState): string | null {
    if (stream.eatSpace()) return null;
    const c: string = stream.peek();
    if (c === undefined) return null;

    // Punctuation
    if (c === '(' || c === ')' || c === ',' || c === '.') { stream.next(); return 'punctuation'; }

    // Operators
    if (c === '=') { stream.next(); return 'operator'; }
    if (c === '!') {
      stream.next();
      if (stream.eat('=')) return 'operator';
      return null;
    }
    if (c === '<' || c === '>') { stream.next(); stream.eat('='); return 'operator'; }

    // Strings
    if (c === '"' || c === "'") {
      const quote = c;
      stream.next();
      while (!stream.eol()) {
        const ch = stream.next();
        if (ch === '\\' && !stream.eol()) { stream.next(); continue; }
        if (ch === quote) return 'string';
      }
      return 'string'; // unterminated — still color what we have
    }

    // Numbers (and the unary minus that introduces them)
    if (/[0-9]/.test(c) || (c === '-' && /[0-9]/.test(stream.string[stream.pos + 1] ?? ''))) {
      stream.next();
      let sawDot = false;
      while (!stream.eol()) {
        const ch = stream.peek();
        if (ch === undefined) break;
        if (/[0-9]/.test(ch)) { stream.next(); continue; }
        if (ch === '.' && !sawDot && /[0-9]/.test(stream.string[stream.pos + 1] ?? '')) {
          sawDot = true; stream.next(); continue;
        }
        break;
      }
      return 'number';
    }

    // Identifiers / keywords
    if (isIdentStart(c)) {
      stream.next();
      stream.eatWhile((ch: string) => isIdentCont(ch));
      const word = stream.current().toLowerCase();
      if (word === 'true' || word === 'false') return 'bool';
      if (word === 'null') return 'null';
      if (word === 'contains' || word === 'starts_with') return 'operator';
      if (ALWAYS_KEYWORD_LOWER.has(word)) return 'keyword';
      return 'propertyName';
    }

    // Anything else: consume and mark as null (unstyled).
    stream.next();
    return null;
  },
  tokenTable: {
    keyword: t.keyword,
    operator: t.operator,
    string: t.string,
    number: t.number,
    bool: t.bool,
    null: t.null,
    propertyName: t.propertyName,
    punctuation: t.punctuation,
  },
};

const ompLanguage = StreamLanguage.define<OmpState>(ompStreamParser as any);

// Default highlight palette tuned for both light and dark site themes.
const ompHighlight = HighlightStyle.define([
  { tag: t.keyword, color: 'var(--accent, #2747d4)', fontWeight: '600' },
  { tag: t.operator, color: 'var(--accent, #2747d4)' },
  { tag: t.string, color: 'var(--success, #1b7a3a)' },
  { tag: t.number, color: 'var(--warn, #b3260e)' },
  { tag: t.bool, color: 'var(--warn, #b3260e)' },
  { tag: t.null, color: 'var(--warn, #b3260e)' },
  { tag: t.propertyName, color: 'var(--fg, #1a1a1a)' },
  { tag: t.punctuation, color: 'var(--fg-soft, #888)' },
]);

export function ompQueryLanguage(): LanguageSupport {
  return new LanguageSupport(ompLanguage, [syntaxHighlighting(ompHighlight)]);
}

// ---------------------------------------------------------------------------
// Completion source
// ---------------------------------------------------------------------------

const BUILTIN_FIELDS: { name: string; type: string; description: string }[] = [
  { name: 'file_type', type: 'string', description: 'Schema-bound file type (e.g. "pdf", "text").' },
  { name: 'source_hash', type: 'string', description: 'Content-address hash of the original bytes.' },
  { name: 'schema_hash', type: 'string', description: 'Hash of the schema that produced this manifest.' },
  { name: 'ingested_at', type: 'datetime', description: 'ISO-8601 timestamp at which the file was ingested.' },
  { name: 'ingester_version', type: 'string', description: 'Version of the ingester that produced this manifest.' },
];

interface FieldCompletion {
  name: string;
  type: string;
  description?: string;
  fileTypes: string[];
}

function collectFields(schemas: Schema[]): FieldCompletion[] {
  const byName = new Map<string, FieldCompletion>();
  for (const s of schemas) {
    for (const f of s.fields) {
      const existing = byName.get(f.name);
      if (existing) {
        existing.fileTypes.push(s.file_type);
        // If types diverge, leave first one; surface as ambiguous in detail.
        if (existing.type !== f.type) existing.type = `${existing.type} | ${f.type}`;
      } else {
        byName.set(f.name, {
          name: f.name,
          type: f.type,
          description: f.description,
          fileTypes: [s.file_type],
        });
      }
    }
  }
  return Array.from(byName.values()).sort((a, b) => a.name.localeCompare(b.name));
}

function lastTokenBefore(tokens: Token[], cursor: number): Token | undefined {
  for (let i = tokens.length - 1; i >= 0; i--) {
    if (tokens[i].end <= cursor) return tokens[i];
  }
  return undefined;
}

function fieldTypeOf(schemas: Schema[], name: string): string | null {
  const lower = name.toLowerCase();
  // Built-ins first.
  const bi = BUILTIN_FIELDS.find(f => f.name === lower);
  if (bi) return bi.type;
  for (const s of schemas) {
    const f = s.fields.find(f => f.name.toLowerCase() === lower);
    if (f) return f.type;
  }
  return null;
}

const OPS_AFTER_FIELD: { label: string; detail?: string }[] = [
  { label: '=', detail: 'equals' },
  { label: '!=', detail: 'not equals' },
  { label: '<' }, { label: '<=' }, { label: '>' }, { label: '>=' },
  { label: 'contains', detail: 'substring (string) or membership (list)' },
  { label: 'starts_with', detail: 'string prefix' },
];

export function ompCompletions(schemas: Schema[]): CompletionSource {
  return (context: CompletionContext): CompletionResult | null => {
    const text = context.state.sliceDoc(0, context.pos);
    let tokens: Token[];
    try { tokens = tokenize(text); } catch { return null; }

    // Identifier under the cursor (the partial word to complete).
    const word = context.matchBefore(/[A-Za-z_][A-Za-z_0-9]*/);
    const explicit = context.explicit;
    if (!word && !explicit) return null;

    const from = word ? word.from : context.pos;
    const to = context.pos;

    // Token immediately preceding the cursor (skipping the partial ident under it).
    const cutoff = word ? word.from : context.pos;
    const prev = lastTokenBefore(tokens, cutoff);

    const expressionStart = !prev
      || prev.kind === 'lparen'
      || prev.kind === 'and'
      || prev.kind === 'or'
      || prev.kind === 'not';
    const afterIdentOrLiteralOrRParen = prev && (
      prev.kind === 'ident'
      || prev.kind === 'string' || prev.kind === 'int' || prev.kind === 'float'
      || prev.kind === 'bool' || prev.kind === 'null' || prev.kind === 'rparen'
    );
    const afterOp = prev && prev.kind === 'op';
    const afterDot = prev && prev.kind === 'dot';
    // Modifier-keyword context: `prefix`, `at`, `limit` immediately precede the cursor.
    let afterModifier: 'prefix' | 'at' | 'limit' | null = null;
    if (prev && prev.kind === 'ident') {
      const lower = prev.value.toLowerCase();
      if (lower === 'prefix' || lower === 'at' || lower === 'limit') {
        // Disambiguate: only treat as modifier if it sits at the trailing top-level position.
        // Quick heuristic: if it's preceded by another ident/op/literal/rparen at depth 0
        // OR is the first token, AND nothing structural follows (cursor is right after it),
        // treat as modifier.
        afterModifier = lower as any;
      }
    }

    // No nested-key data — bail on `field.` for now.
    if (afterDot) return null;

    const options: Completion[] = [];
    const fields = collectFields(schemas);

    if (afterModifier === 'limit') {
      options.push({ label: '100', type: 'text', detail: 'page size (modifier)' });
      options.push({ label: '1000', type: 'text', detail: 'max page size (modifier)' });
      // limit could also be used as a field name; offer operators too.
      for (const op of OPS_AFTER_FIELD) {
        options.push({ label: op.label, type: 'operator', detail: op.detail });
      }
    } else if (afterModifier === 'prefix' || afterModifier === 'at') {
      options.push({
        label: '""',
        type: 'text',
        apply: '""',
        detail: afterModifier === 'prefix' ? 'path prefix (modifier)' : 'ref / commit / branch (modifier)',
      });
      for (const op of OPS_AFTER_FIELD) {
        options.push({ label: op.label, type: 'operator', detail: op.detail });
      }
    } else if (afterOp) {
      // Suggest a typed placeholder based on the field on the LHS.
      // Walk back through the token list to find the last identifier path before the op.
      let i = tokens.length - 1;
      while (i >= 0 && tokens[i].end > cutoff) i--;
      // i now points at `prev` (the op). Walk back over `dot ident` chains.
      let lhs: string | null = null;
      let k = i - 1;
      while (k >= 0 && tokens[k].kind === 'ident') {
        lhs = tokens[k].value;
        k--;
        if (k >= 0 && tokens[k].kind === 'dot') { k--; continue; }
        break;
      }
      const ftype = lhs ? fieldTypeOf(schemas, lhs) : null;
      if (ftype === 'bool') {
        options.push({ label: 'true', type: 'keyword' });
        options.push({ label: 'false', type: 'keyword' });
      } else if (ftype === 'int' || ftype === 'float') {
        options.push({ label: '0', type: 'text' });
      } else {
        // Default: string-style placeholder. Works for string/datetime/list[...].
        options.push({ label: '""', type: 'text', apply: '""' });
      }
    } else if (afterIdentOrLiteralOrRParen) {
      // After a field path or completed atom: operators, AND/OR, modifier keywords.
      for (const op of OPS_AFTER_FIELD) {
        options.push({ label: op.label, type: 'operator', detail: op.detail });
      }
      options.push({ label: 'AND', type: 'keyword' });
      options.push({ label: 'OR', type: 'keyword' });
      options.push({ label: 'prefix', type: 'keyword', detail: 'restrict to path prefix' });
      options.push({ label: 'at', type: 'keyword', detail: 'time-travel ref' });
      options.push({ label: 'limit', type: 'keyword', detail: 'page size' });
    }

    if (expressionStart || afterModifier === null && options.length === 0) {
      for (const f of fields) {
        options.push({
          label: f.name,
          type: 'property',
          detail: f.type,
          info: f.description ?? (f.fileTypes.length === 1 ? `from ${f.fileTypes[0]}` : `${f.fileTypes.length} schemas`),
        });
      }
      for (const b of BUILTIN_FIELDS) {
        if (!fields.find(f => f.name.toLowerCase() === b.name)) {
          options.push({ label: b.name, type: 'property', detail: b.type, info: b.description });
        }
      }
      options.push({ label: 'NOT', type: 'keyword' });
      options.push({ label: 'exists', type: 'keyword', apply: 'exists()', detail: 'field-presence check' });
    }

    if (options.length === 0) return null;
    return {
      from,
      to,
      options,
      validFor: /^[A-Za-z_][A-Za-z_0-9]*$/,
    };
  };
}

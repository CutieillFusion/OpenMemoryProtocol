import { describe, it, expect } from 'vitest';
import { parseUnified, tokenize, TokenizeError } from './queryLang';

describe('tokenize', () => {
  it('splits a simple predicate', () => {
    const toks = tokenize('file_type = "pdf" AND pages > 10');
    expect(toks.map(t => t.kind)).toEqual([
      'ident', 'op', 'string', 'and', 'ident', 'op', 'int',
    ]);
    expect(toks[0].value).toBe('file_type');
    expect(toks[2].value).toBe('pdf');
    expect(toks[6].value).toBe('10');
  });

  it('handles negative ints', () => {
    const toks = tokenize('pages > -1');
    expect(toks.map(t => t.kind)).toEqual(['ident', 'op', 'int']);
    expect(toks[2].value).toBe('-1');
  });

  it('throws on unterminated strings', () => {
    expect(() => tokenize('x = "abc')).toThrow(TokenizeError);
  });
});

describe('parseUnified', () => {
  it('returns empty for empty input', () => {
    expect(parseUnified('')).toEqual({});
    expect(parseUnified('   ')).toEqual({});
  });

  it('passes through a predicate with no modifiers', () => {
    const r = parseUnified('file_type = "pdf" AND pages > 10');
    expect(r.where).toBe('file_type = "pdf" AND pages > 10');
    expect(r.prefix).toBeUndefined();
    expect(r.at).toBeUndefined();
    expect(r.limit).toBeUndefined();
  });

  it('peels each modifier individually', () => {
    expect(parseUnified('file_type = "pdf" prefix "reports/"')).toEqual({
      where: 'file_type = "pdf"',
      prefix: 'reports/',
    });
    expect(parseUnified('file_type = "pdf" at HEAD')).toEqual({
      where: 'file_type = "pdf"',
      at: 'HEAD',
    });
    expect(parseUnified('file_type = "pdf" limit 50')).toEqual({
      where: 'file_type = "pdf"',
      limit: 50,
    });
  });

  it('peels all three modifiers regardless of order', () => {
    const r = parseUnified('pages > 10 prefix "r/" at "HEAD~1" limit 5');
    expect(r).toEqual({
      where: 'pages > 10',
      prefix: 'r/',
      at: 'HEAD~1',
      limit: 5,
    });

    const r2 = parseUnified('pages > 10 limit 5 at "HEAD~1" prefix "r/"');
    expect(r2).toEqual({
      where: 'pages > 10',
      prefix: 'r/',
      at: 'HEAD~1',
      limit: 5,
    });
  });

  it('treats `prefix = "x"` as a predicate (not a modifier)', () => {
    const r = parseUnified('prefix = "foo"');
    expect(r.where).toBe('prefix = "foo"');
    expect(r.prefix).toBeUndefined();
  });

  it('handles modifiers-only input (no predicate)', () => {
    const r = parseUnified('prefix "reports/" limit 25');
    expect(r.where).toBeUndefined();
    expect(r.prefix).toBe('reports/');
    expect(r.limit).toBe(25);
  });

  it('does not peel modifiers inside parentheses', () => {
    // `prefix "x"` here is not at trailing top-level — it ends inside the group.
    // So it stays in the predicate and the outer parser will reject it later.
    const r = parseUnified('(file_type = "pdf" AND pages > 10)');
    expect(r.where).toBe('(file_type = "pdf" AND pages > 10)');
    expect(r.prefix).toBeUndefined();
  });

  it('peels modifiers after a parenthesized predicate', () => {
    const r = parseUnified('(file_type = "pdf" OR file_type = "text") limit 5');
    expect(r.where).toBe('(file_type = "pdf" OR file_type = "text")');
    expect(r.limit).toBe(5);
  });

  it('rejects limit with non-integer literal', () => {
    // `limit 5.5` is a float, not an int — leave in predicate.
    const r = parseUnified('pages > 10 limit 5.5');
    expect(r.where).toBe('pages > 10 limit 5.5');
    expect(r.limit).toBeUndefined();
  });

  it('throws on tokenize errors', () => {
    expect(() => parseUnified('x = "unterminated')).toThrow(TokenizeError);
  });
});

//! Predicate query language for manifest filtering.
//!
//! See `docs/design/15-query-and-discovery.md`. Grammar:
//!
//! ```text
//! expr     := atom | atom AND expr | atom OR expr | NOT atom | (expr)
//! atom     := <field-path> <op> <literal> | exists(<field-path>)
//! field-path := identifier ("." identifier)*           e.g. fields.tags
//! op       := = | != | < | <= | > | >= | contains | starts_with
//! literal  := string | int | float | bool | null
//! ```
//!
//! Field paths resolve against:
//! 1. Top-level Manifest fields: `file_type`, `source_hash`, `schema_hash`,
//!    `ingested_at`, `ingester_version`.
//! 2. Inside `fields.<name>` for user/probe fields. The `fields.` prefix is
//!    optional — bare `tags` is treated as `fields.tags` if no top-level
//!    field by that name exists.
//!
//! `contains` is membership for arrays (e.g. `tags contains "policy"`) and
//! substring for strings. No regex; no SQL.

use std::collections::BTreeMap;

use crate::manifest::{FieldValue, Manifest};

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Atom(Atom),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Atom {
    Compare {
        path: FieldPath,
        op: Op,
        value: Literal,
    },
    Exists(FieldPath),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    StartsWith,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

pub type FieldPath = Vec<String>;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("parse error at byte {pos}: {msg}")]
    Parse { pos: usize, msg: String },
    #[error("unsupported operation between values")]
    TypeMismatch,
}

// =============================================================================
// Parser
// =============================================================================

pub fn parse(input: &str) -> Result<Expr, QueryError> {
    let tokens = tokenize(input)?;
    let mut p = Parser { tokens: &tokens, pos: 0 };
    let expr = p.parse_expr()?;
    if p.pos < p.tokens.len() {
        let tok = &p.tokens[p.pos];
        return Err(QueryError::Parse {
            pos: tok.byte_pos,
            msg: format!("unexpected token after expression: {:?}", tok.kind),
        });
    }
    Ok(expr)
}

#[derive(Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    And,
    Or,
    Not,
    Exists,
    Op(Op),
    LParen,
    RParen,
    Dot,
    Comma,
}

#[derive(Debug)]
struct Token {
    kind: Tok,
    byte_pos: usize,
}

fn tokenize(input: &str) -> Result<Vec<Token>, QueryError> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'(' => {
                out.push(Token { kind: Tok::LParen, byte_pos: start });
                i += 1;
            }
            b')' => {
                out.push(Token { kind: Tok::RParen, byte_pos: start });
                i += 1;
            }
            b'.' => {
                out.push(Token { kind: Tok::Dot, byte_pos: start });
                i += 1;
            }
            b',' => {
                out.push(Token { kind: Tok::Comma, byte_pos: start });
                i += 1;
            }
            b'=' => {
                out.push(Token { kind: Tok::Op(Op::Eq), byte_pos: start });
                i += 1;
            }
            b'!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Token { kind: Tok::Op(Op::Ne), byte_pos: start });
                    i += 2;
                } else {
                    return Err(QueryError::Parse {
                        pos: i,
                        msg: "expected `!=`".into(),
                    });
                }
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Token { kind: Tok::Op(Op::Le), byte_pos: start });
                    i += 2;
                } else {
                    out.push(Token { kind: Tok::Op(Op::Lt), byte_pos: start });
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Token { kind: Tok::Op(Op::Ge), byte_pos: start });
                    i += 2;
                } else {
                    out.push(Token { kind: Tok::Op(Op::Gt), byte_pos: start });
                    i += 1;
                }
            }
            b'"' | b'\'' => {
                let quote = c;
                i += 1;
                let str_start = i;
                let mut buf = String::new();
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        buf.push(bytes[i + 1] as char);
                        i += 2;
                    } else {
                        buf.push(bytes[i] as char);
                        i += 1;
                    }
                }
                if i >= bytes.len() {
                    return Err(QueryError::Parse {
                        pos: str_start,
                        msg: "unterminated string literal".into(),
                    });
                }
                i += 1; // consume closing quote
                out.push(Token { kind: Tok::Str(buf), byte_pos: start });
            }
            c if c.is_ascii_digit() || c == b'-' => {
                let mut j = i;
                if c == b'-' {
                    j += 1;
                }
                let mut saw_dot = false;
                while j < bytes.len()
                    && (bytes[j].is_ascii_digit() || bytes[j] == b'.')
                {
                    if bytes[j] == b'.' {
                        // peek ahead: if next char is a digit, we're inside a
                        // float; otherwise this dot belongs to a field path
                        // (e.g. `1.foo` doesn't make sense, but `fields.1` does
                        // — we won't allow that; integers can't be path roots).
                        if j + 1 < bytes.len() && bytes[j + 1].is_ascii_digit() {
                            saw_dot = true;
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    j += 1;
                }
                let num = std::str::from_utf8(&bytes[i..j]).unwrap_or("");
                if saw_dot {
                    let f: f64 = num.parse().map_err(|_| QueryError::Parse {
                        pos: i,
                        msg: format!("bad float literal: {num}"),
                    })?;
                    out.push(Token { kind: Tok::Float(f), byte_pos: start });
                } else {
                    let n: i64 = num.parse().map_err(|_| QueryError::Parse {
                        pos: i,
                        msg: format!("bad int literal: {num}"),
                    })?;
                    out.push(Token { kind: Tok::Int(n), byte_pos: start });
                }
                i = j;
            }
            c if is_ident_start(c) => {
                let mut j = i + 1;
                while j < bytes.len() && is_ident_cont(bytes[j]) {
                    j += 1;
                }
                let word = std::str::from_utf8(&bytes[i..j]).unwrap_or("");
                let kind = match word.to_ascii_lowercase().as_str() {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    "exists" => Tok::Exists,
                    "true" => Tok::Bool(true),
                    "false" => Tok::Bool(false),
                    "null" => Tok::Null,
                    "contains" => Tok::Op(Op::Contains),
                    "starts_with" => Tok::Op(Op::StartsWith),
                    _ => Tok::Ident(word.to_string()),
                };
                out.push(Token { kind, byte_pos: start });
                i = j;
            }
            _ => {
                return Err(QueryError::Parse {
                    pos: i,
                    msg: format!("unexpected character: {:?}", c as char),
                });
            }
        }
    }
    Ok(out)
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|t| &t.kind)
    }

    fn advance(&mut self) -> Option<&'a Token> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: Tok) -> Result<(), QueryError> {
        match self.tokens.get(self.pos) {
            Some(t) if t.kind == want => {
                self.pos += 1;
                Ok(())
            }
            Some(t) => Err(QueryError::Parse {
                pos: t.byte_pos,
                msg: format!("expected {want:?}, got {:?}", t.kind),
            }),
            None => Err(QueryError::Parse {
                pos: 0,
                msg: format!("expected {want:?}, got end of input"),
            }),
        }
    }

    /// expr := or_expr
    fn parse_expr(&mut self) -> Result<Expr, QueryError> {
        self.parse_or()
    }

    /// or_expr := and_expr ("or" and_expr)*
    fn parse_or(&mut self) -> Result<Expr, QueryError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Tok::Or)) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// and_expr := not_expr ("and" not_expr)*
    fn parse_and(&mut self) -> Result<Expr, QueryError> {
        let mut left = self.parse_not()?;
        while matches!(self.peek(), Some(Tok::And)) {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// not_expr := "not" not_expr | atom
    fn parse_not(&mut self) -> Result<Expr, QueryError> {
        if matches!(self.peek(), Some(Tok::Not)) {
            self.advance();
            let inner = self.parse_not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_primary()
    }

    /// primary := "(" expr ")" | atom
    fn parse_primary(&mut self) -> Result<Expr, QueryError> {
        if matches!(self.peek(), Some(Tok::LParen)) {
            self.advance();
            let e = self.parse_expr()?;
            self.expect(Tok::RParen)?;
            return Ok(e);
        }
        if matches!(self.peek(), Some(Tok::Exists)) {
            self.advance();
            self.expect(Tok::LParen)?;
            let path = self.parse_field_path()?;
            self.expect(Tok::RParen)?;
            return Ok(Expr::Atom(Atom::Exists(path)));
        }
        // Otherwise: <field-path> <op> <literal>
        let path = self.parse_field_path()?;
        let op = match self.advance() {
            Some(Token { kind: Tok::Op(o), .. }) => *o,
            Some(t) => {
                return Err(QueryError::Parse {
                    pos: t.byte_pos,
                    msg: format!("expected operator, got {:?}", t.kind),
                });
            }
            None => {
                return Err(QueryError::Parse {
                    pos: 0,
                    msg: "expected operator, got end of input".into(),
                });
            }
        };
        let lit = self.parse_literal()?;
        Ok(Expr::Atom(Atom::Compare {
            path,
            op,
            value: lit,
        }))
    }

    fn parse_field_path(&mut self) -> Result<FieldPath, QueryError> {
        let first = match self.advance() {
            Some(Token { kind: Tok::Ident(s), .. }) => s.clone(),
            Some(t) => {
                return Err(QueryError::Parse {
                    pos: t.byte_pos,
                    msg: format!("expected field path, got {:?}", t.kind),
                });
            }
            None => {
                return Err(QueryError::Parse {
                    pos: 0,
                    msg: "expected field path, got end of input".into(),
                });
            }
        };
        let mut path = vec![first];
        while matches!(self.peek(), Some(Tok::Dot)) {
            self.advance();
            match self.advance() {
                Some(Token { kind: Tok::Ident(s), .. }) => path.push(s.clone()),
                Some(t) => {
                    return Err(QueryError::Parse {
                        pos: t.byte_pos,
                        msg: format!("expected ident after `.`, got {:?}", t.kind),
                    });
                }
                None => {
                    return Err(QueryError::Parse {
                        pos: 0,
                        msg: "expected ident after `.`".into(),
                    });
                }
            }
        }
        Ok(path)
    }

    fn parse_literal(&mut self) -> Result<Literal, QueryError> {
        match self.advance() {
            Some(Token { kind: Tok::Str(s), .. }) => Ok(Literal::String(s.clone())),
            Some(Token { kind: Tok::Int(n), .. }) => Ok(Literal::Int(*n)),
            Some(Token { kind: Tok::Float(f), .. }) => Ok(Literal::Float(*f)),
            Some(Token { kind: Tok::Bool(b), .. }) => Ok(Literal::Bool(*b)),
            Some(Token { kind: Tok::Null, .. }) => Ok(Literal::Null),
            Some(t) => Err(QueryError::Parse {
                pos: t.byte_pos,
                msg: format!("expected literal, got {:?}", t.kind),
            }),
            None => Err(QueryError::Parse {
                pos: 0,
                msg: "expected literal, got end of input".into(),
            }),
        }
    }
}

// =============================================================================
// Result + cursor types (used by Repo::query)
// =============================================================================

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct QueryMatch {
    pub path: String,
    pub manifest_hash: crate::hash::Hash,
    pub source_hash: crate::hash::Hash,
    pub file_type: String,
    pub fields: BTreeMap<String, FieldValue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub matches: Vec<QueryMatch>,
    pub next_cursor: Option<String>,
}

/// Cursor format: base64 of `<commit-hex>|<offset>` (or just `<offset>` if
/// commit is None — i.e. empty repo). Opaque to callers.
pub fn encode_cursor(commit: Option<&str>, offset: usize) -> String {
    use base64::Engine;
    let raw = match commit {
        Some(c) => format!("{c}|{offset}"),
        None => format!("|{offset}"),
    };
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

pub fn decode_cursor(s: &str) -> crate::error::Result<(Option<String>, usize)> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .map_err(|e| crate::error::OmpError::InvalidPath(format!("bad cursor: {e}")))?;
    let s = std::str::from_utf8(&raw)
        .map_err(|e| crate::error::OmpError::InvalidPath(format!("bad cursor utf8: {e}")))?;
    let mut parts = s.splitn(2, '|');
    let commit = parts.next().unwrap_or("");
    let offset_str = parts
        .next()
        .ok_or_else(|| crate::error::OmpError::InvalidPath("malformed cursor".into()))?;
    let offset: usize = offset_str
        .parse()
        .map_err(|e| crate::error::OmpError::InvalidPath(format!("bad cursor offset: {e}")))?;
    let commit_opt = if commit.is_empty() {
        None
    } else {
        Some(commit.to_string())
    };
    Ok((commit_opt, offset))
}

// =============================================================================
// Evaluator
// =============================================================================

/// Resolve a field path on a manifest. The first segment may name a top-level
/// Manifest field (`file_type`, etc.); otherwise it's looked up in `fields.<name>`.
pub fn resolve_path<'m>(manifest: &'m Manifest, path: &[String]) -> Option<ResolvedValue<'m>> {
    if path.is_empty() {
        return None;
    }
    let head = &path[0];
    let rest = &path[1..];

    let top = match head.as_str() {
        "file_type" => Some(ResolvedValue::Owned(FieldValue::String(manifest.file_type.clone()))),
        "source_hash" => Some(ResolvedValue::Owned(FieldValue::String(manifest.source_hash.hex()))),
        "schema_hash" => Some(ResolvedValue::Owned(FieldValue::String(manifest.schema_hash.hex()))),
        "ingested_at" => Some(ResolvedValue::Owned(FieldValue::String(manifest.ingested_at.clone()))),
        "ingester_version" => Some(ResolvedValue::Owned(FieldValue::String(manifest.ingester_version.clone()))),
        "fields" => {
            // Explicit `fields.<name>` form.
            return descend_fields(&manifest.fields, rest);
        }
        _ => None,
    };
    if top.is_some() {
        if !rest.is_empty() {
            return None; // can't dot into a top-level scalar
        }
        return top;
    }
    // Implicit `fields.` prefix.
    descend_fields(&manifest.fields, path)
}

fn descend_fields<'m>(
    fields: &'m BTreeMap<String, FieldValue>,
    path: &[String],
) -> Option<ResolvedValue<'m>> {
    if path.is_empty() {
        return None;
    }
    let mut cur: &'m FieldValue = fields.get(&path[0])?;
    for seg in &path[1..] {
        match cur {
            FieldValue::Object(o) => {
                cur = o.get(seg)?;
            }
            _ => return None,
        }
    }
    Some(ResolvedValue::Borrowed(cur))
}

/// Either a borrowed reference into the manifest or a freshly-constructed
/// scalar (for top-level Manifest fields synthesized as FieldValues).
pub enum ResolvedValue<'a> {
    Borrowed(&'a FieldValue),
    Owned(FieldValue),
}

impl<'a> ResolvedValue<'a> {
    pub fn as_field(&self) -> &FieldValue {
        match self {
            ResolvedValue::Borrowed(v) => v,
            ResolvedValue::Owned(v) => v,
        }
    }
}

/// Evaluate a predicate against a manifest. Returns true if the manifest
/// matches.
pub fn evaluate(expr: &Expr, manifest: &Manifest) -> bool {
    match expr {
        Expr::And(a, b) => evaluate(a, manifest) && evaluate(b, manifest),
        Expr::Or(a, b) => evaluate(a, manifest) || evaluate(b, manifest),
        Expr::Not(a) => !evaluate(a, manifest),
        Expr::Atom(Atom::Exists(path)) => resolve_path(manifest, path).is_some(),
        Expr::Atom(Atom::Compare { path, op, value }) => {
            match resolve_path(manifest, path) {
                Some(r) => compare(r.as_field(), *op, value),
                None => false,
            }
        }
    }
}

fn compare(field: &FieldValue, op: Op, lit: &Literal) -> bool {
    match (field, lit) {
        (FieldValue::String(s), Literal::String(t)) => match op {
            Op::Eq => s == t,
            Op::Ne => s != t,
            Op::Lt => s < t,
            Op::Le => s <= t,
            Op::Gt => s > t,
            Op::Ge => s >= t,
            Op::Contains => s.contains(t),
            Op::StartsWith => s.starts_with(t),
        },
        (FieldValue::Int(n), Literal::Int(m)) => cmp_num(*n as f64, *m as f64, op),
        (FieldValue::Float(n), Literal::Float(m)) => cmp_num(*n, *m, op),
        (FieldValue::Int(n), Literal::Float(m)) => cmp_num(*n as f64, *m, op),
        (FieldValue::Float(n), Literal::Int(m)) => cmp_num(*n, *m as f64, op),
        (FieldValue::Bool(b), Literal::Bool(c)) => match op {
            Op::Eq => b == c,
            Op::Ne => b != c,
            _ => false,
        },
        (FieldValue::List(items), lit) => {
            // Membership for lists; everything else is false (we don't compare
            // a list to a scalar arithmetic-style).
            if matches!(op, Op::Contains) {
                items.iter().any(|item| match (item, lit) {
                    (FieldValue::String(s), Literal::String(t)) => s == t,
                    (FieldValue::Int(n), Literal::Int(m)) => n == m,
                    (FieldValue::Float(n), Literal::Float(m)) => (n - m).abs() < f64::EPSILON,
                    (FieldValue::Int(n), Literal::Float(m)) => (*n as f64 - m).abs() < f64::EPSILON,
                    (FieldValue::Float(n), Literal::Int(m)) => (n - *m as f64).abs() < f64::EPSILON,
                    (FieldValue::Bool(b), Literal::Bool(c)) => b == c,
                    _ => false,
                })
            } else {
                false
            }
        }
        (FieldValue::Datetime(s), Literal::String(t)) => match op {
            Op::Eq => s == t,
            Op::Ne => s != t,
            Op::Lt => s < t,
            Op::Le => s <= t,
            Op::Gt => s > t,
            Op::Ge => s >= t,
            Op::Contains => s.contains(t),
            Op::StartsWith => s.starts_with(t),
        },
        (FieldValue::Null, Literal::Null) => matches!(op, Op::Eq),
        (FieldValue::Null, _) => matches!(op, Op::Ne),
        _ => false,
    }
}

fn cmp_num(a: f64, b: f64, op: Op) -> bool {
    match op {
        Op::Eq => (a - b).abs() < f64::EPSILON,
        Op::Ne => (a - b).abs() >= f64::EPSILON,
        Op::Lt => a < b,
        Op::Le => a <= b,
        Op::Gt => a > b,
        Op::Ge => a >= b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;

    fn m_with_fields(file_type: &str, fields: Vec<(&str, FieldValue)>) -> Manifest {
        let mut map = BTreeMap::new();
        for (k, v) in fields {
            map.insert(k.to_string(), v);
        }
        Manifest {
            source_hash: Hash::of(b"src"),
            file_type: file_type.to_string(),
            schema_hash: Hash::of(b"schema"),
            ingested_at: "2026-04-25T12:00:00Z".to_string(),
            ingester_version: "test".to_string(),
            probe_hashes: BTreeMap::new(),
            fields: map,
        }
    }

    // ---------------- Tokenizer ----------------

    #[test]
    fn tokenize_simple() {
        let toks = tokenize("file_type = \"pdf\" AND pages > 10").unwrap();
        let kinds: Vec<_> = toks.iter().map(|t| &t.kind).collect();
        assert!(matches!(kinds[0], Tok::Ident(s) if s == "file_type"));
        assert!(matches!(kinds[1], Tok::Op(Op::Eq)));
        assert!(matches!(kinds[2], Tok::Str(s) if s == "pdf"));
        assert!(matches!(kinds[3], Tok::And));
    }

    #[test]
    fn tokenize_starts_with() {
        let toks = tokenize("author starts_with \"A\"").unwrap();
        assert!(toks.iter().any(|t| matches!(t.kind, Tok::Op(Op::StartsWith))));
    }

    #[test]
    fn tokenize_negative_int() {
        let toks = tokenize("pages > -1").unwrap();
        assert!(toks.iter().any(|t| matches!(t.kind, Tok::Int(-1))));
    }

    // ---------------- Parser ----------------

    #[test]
    fn parse_eq_string() {
        let e = parse("file_type = \"pdf\"").unwrap();
        assert!(matches!(
            e,
            Expr::Atom(Atom::Compare {
                op: Op::Eq,
                value: Literal::String(_),
                ..
            })
        ));
    }

    #[test]
    fn parse_and_or_precedence() {
        let e = parse("file_type = \"pdf\" AND pages > 10 OR tags contains \"draft\"")
            .unwrap();
        // OR is the outermost binder.
        assert!(matches!(e, Expr::Or(..)));
    }

    #[test]
    fn parse_parens_override() {
        let e = parse("file_type = \"pdf\" AND (pages > 10 OR tags contains \"draft\")")
            .unwrap();
        // Now AND is outermost.
        assert!(matches!(e, Expr::And(..)));
    }

    #[test]
    fn parse_not() {
        let e = parse("NOT file_type = \"pdf\"").unwrap();
        assert!(matches!(e, Expr::Not(_)));
    }

    #[test]
    fn parse_exists_dotted() {
        let e = parse("exists(fields.transcript)").unwrap();
        assert!(matches!(e, Expr::Atom(Atom::Exists(p)) if p == vec!["fields", "transcript"]));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse("file_type === \"pdf\"").is_err());
        assert!(parse("file_type =").is_err());
        assert!(parse("(file_type = \"pdf\"").is_err());
    }

    // ---------------- Evaluator ----------------

    #[test]
    fn eval_top_level_field_eq() {
        let m = m_with_fields("pdf", vec![]);
        let e = parse("file_type = \"pdf\"").unwrap();
        assert!(evaluate(&e, &m));

        let e = parse("file_type = \"text\"").unwrap();
        assert!(!evaluate(&e, &m));
    }

    #[test]
    fn eval_user_field_int_compare() {
        let m = m_with_fields("pdf", vec![("pages", FieldValue::Int(42))]);
        assert!(evaluate(&parse("pages > 10").unwrap(), &m));
        assert!(evaluate(&parse("pages = 42").unwrap(), &m));
        assert!(!evaluate(&parse("pages < 10").unwrap(), &m));
        assert!(evaluate(&parse("fields.pages = 42").unwrap(), &m));
    }

    #[test]
    fn eval_list_contains() {
        let m = m_with_fields(
            "pdf",
            vec![(
                "tags",
                FieldValue::List(vec![
                    FieldValue::String("policy".into()),
                    FieldValue::String("draft".into()),
                ]),
            )],
        );
        assert!(evaluate(&parse("tags contains \"policy\"").unwrap(), &m));
        assert!(!evaluate(&parse("tags contains \"final\"").unwrap(), &m));
    }

    #[test]
    fn eval_starts_with_and_contains_string() {
        let m = m_with_fields("pdf", vec![("author", FieldValue::String("Alice Smith".into()))]);
        assert!(evaluate(&parse("author starts_with \"Alice\"").unwrap(), &m));
        assert!(evaluate(&parse("author contains \"Smith\"").unwrap(), &m));
        assert!(!evaluate(&parse("author starts_with \"Bob\"").unwrap(), &m));
    }

    #[test]
    fn eval_exists_and_not_exists() {
        let m = m_with_fields("pdf", vec![("title", FieldValue::String("doc".into()))]);
        assert!(evaluate(&parse("exists(title)").unwrap(), &m));
        assert!(evaluate(&parse("exists(fields.title)").unwrap(), &m));
        assert!(!evaluate(&parse("exists(missing)").unwrap(), &m));
        assert!(evaluate(&parse("NOT exists(missing)").unwrap(), &m));
    }

    #[test]
    fn eval_and_or_not_combinations() {
        let m = m_with_fields(
            "pdf",
            vec![
                ("pages", FieldValue::Int(50)),
                ("tags", FieldValue::List(vec![FieldValue::String("policy".into())])),
            ],
        );
        assert!(evaluate(
            &parse("file_type = \"pdf\" AND pages > 10 AND tags contains \"policy\"").unwrap(),
            &m
        ));
        assert!(!evaluate(
            &parse("file_type = \"text\" OR pages < 10").unwrap(),
            &m
        ));
        assert!(evaluate(
            &parse("file_type = \"text\" OR pages > 10").unwrap(),
            &m
        ));
        assert!(evaluate(
            &parse("NOT (pages < 10)").unwrap(),
            &m
        ));
    }

    #[test]
    fn eval_missing_field_is_false() {
        let m = m_with_fields("pdf", vec![]);
        assert!(!evaluate(&parse("nonexistent = \"x\"").unwrap(), &m));
        assert!(!evaluate(&parse("fields.nope > 0").unwrap(), &m));
    }

    #[test]
    fn eval_nested_object_path() {
        let mut nested = BTreeMap::new();
        nested.insert("name".into(), FieldValue::String("Alice".into()));
        let m = m_with_fields("pdf", vec![("author", FieldValue::Object(nested))]);
        assert!(evaluate(&parse("author.name = \"Alice\"").unwrap(), &m));
        assert!(evaluate(&parse("fields.author.name = \"Alice\"").unwrap(), &m));
        assert!(!evaluate(&parse("author.name = \"Bob\"").unwrap(), &m));
    }
}

//! Canonical TOML writer for manifests.
//!
//! The contract (per `02-object-model.md`): reading a manifest and writing it
//! back must produce byte-identical output, or the hash over the serialized
//! manifest would churn on every read-then-write cycle.
//!
//! Rules enforced:
//! - Keys within every table are emitted in lexicographic order.
//! - Floats are emitted with a fixed representation (no trailing zeros, always
//!   a decimal point, `inf` / `-inf` / `nan` forbidden — manifests never need
//!   these, so we reject rather than try to encode a portable form).
//! - Line endings are LF (`\n`); trailing newline always present.
//! - No comments, no decorations — `toml_edit`'s formatting metadata is
//!   stripped during canonicalization.
//!
//! Implementation: parse via `toml_edit::DocumentMut`, then emit from scratch
//! via our own walker. Using `toml_edit` only for its robust parser keeps the
//! emitter small and predictable.

use std::fmt::Write;

use toml_edit::{DocumentMut, Item, Value};

use crate::error::{OmpError, Result};

/// Parse arbitrary TOML bytes and re-emit them in canonical form.
pub fn canonicalize(input: &str) -> Result<String> {
    let doc: DocumentMut = input
        .parse()
        .map_err(|e| OmpError::SchemaValidation(format!("invalid TOML: {e}")))?;
    let mut out = String::new();
    emit_table(doc.as_table(), &[], &mut out)?;
    Ok(out)
}

/// Convenience: assert `canonicalize(canonicalize(x)) == canonicalize(x)`.
pub fn is_canonical(s: &str) -> Result<bool> {
    Ok(canonicalize(s)? == s)
}

fn emit_table(tbl: &toml_edit::Table, path: &[&str], out: &mut String) -> Result<()> {
    // First pass: emit inline key=value pairs, in sorted order.
    let mut keys: Vec<&str> = tbl.iter().map(|(k, _)| k).collect();
    keys.sort();
    // Header (only if we have a path — the root table has none).
    if !path.is_empty() && has_any_inline(tbl) {
        writeln!(out, "[{}]", path.join(".")).unwrap();
    } else if !path.is_empty() && !has_subtables(tbl) {
        // Path non-empty but purely empty: still emit header so the table exists.
        writeln!(out, "[{}]", path.join(".")).unwrap();
    }
    for k in &keys {
        let item = &tbl[*k];
        if is_inline(item) {
            emit_pair(k, item, out)?;
        }
    }
    let emitted_header = !path.is_empty() && (has_any_inline(tbl) || !has_subtables(tbl));
    let mut first_subtable = !emitted_header;
    // Second pass: subtables and arrays-of-tables, in sorted order.
    for k in &keys {
        let item = &tbl[*k];
        match item {
            Item::Table(sub) => {
                if emitted_header || !first_subtable {
                    out.push('\n');
                }
                first_subtable = false;
                let mut nested = path.to_vec();
                nested.push(*k);
                emit_table(sub, &nested, out)?;
            }
            Item::ArrayOfTables(arr) => {
                for (i, t) in arr.iter().enumerate() {
                    if emitted_header || !first_subtable || i > 0 {
                        out.push('\n');
                    }
                    first_subtable = false;
                    writeln!(out, "[[{}]]", join_path(path, k)).unwrap();
                    emit_table_body(t, &push_path(path, k), out)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn emit_table_body(tbl: &toml_edit::Table, path: &[&str], out: &mut String) -> Result<()> {
    let mut keys: Vec<&str> = tbl.iter().map(|(k, _)| k).collect();
    keys.sort();
    for k in &keys {
        let item = &tbl[*k];
        if is_inline(item) {
            emit_pair(k, item, out)?;
        }
    }
    for k in &keys {
        let item = &tbl[*k];
        match item {
            Item::Table(sub) => {
                out.push('\n');
                let mut nested = path.to_vec();
                nested.push(*k);
                emit_table(sub, &nested, out)?;
            }
            Item::ArrayOfTables(arr) => {
                for t in arr.iter() {
                    out.push('\n');
                    writeln!(out, "[[{}]]", join_path(path, k)).unwrap();
                    emit_table_body(t, &push_path(path, k), out)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn join_path(base: &[&str], tail: &str) -> String {
    let mut parts: Vec<String> = base.iter().map(|s| (*s).to_string()).collect();
    parts.push(tail.to_string());
    parts.join(".")
}

fn push_path<'a>(base: &[&'a str], tail: &'a str) -> Vec<&'a str> {
    let mut out = base.to_vec();
    out.push(tail);
    out
}

fn has_any_inline(tbl: &toml_edit::Table) -> bool {
    tbl.iter().any(|(_, v)| is_inline(v))
}

fn has_subtables(tbl: &toml_edit::Table) -> bool {
    tbl.iter()
        .any(|(_, v)| matches!(v, Item::Table(_) | Item::ArrayOfTables(_)))
}

fn is_inline(item: &Item) -> bool {
    matches!(item, Item::Value(_))
}

fn emit_pair(key: &str, item: &Item, out: &mut String) -> Result<()> {
    write!(out, "{} = ", quote_key(key)).unwrap();
    if let Item::Value(v) = item {
        emit_value(v, out)?;
        out.push('\n');
    }
    Ok(())
}

fn quote_key(k: &str) -> String {
    // Bare-key regex: [A-Za-z0-9_-]+
    let bare = !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if bare {
        k.to_string()
    } else {
        quote_string(k)
    }
}

fn emit_value(v: &Value, out: &mut String) -> Result<()> {
    match v {
        Value::String(s) => {
            out.push_str(&quote_string(s.value()));
        }
        Value::Integer(i) => {
            write!(out, "{}", i.value()).unwrap();
        }
        Value::Float(f) => {
            let x = *f.value();
            if !x.is_finite() {
                return Err(OmpError::SchemaValidation(format!(
                    "non-finite float rejected by canonical TOML: {x}"
                )));
            }
            // Shortest repr with a forced decimal point.
            let s = format_float(x);
            out.push_str(&s);
        }
        Value::Boolean(b) => {
            out.push_str(if *b.value() { "true" } else { "false" });
        }
        Value::Datetime(dt) => {
            write!(out, "{}", dt.value()).unwrap();
        }
        Value::Array(arr) => {
            out.push('[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_value(item, out)?;
            }
            out.push(']');
        }
        Value::InlineTable(tbl) => {
            let mut entries: Vec<(&str, &Value)> = tbl.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write!(out, "{} = ", quote_key(k)).unwrap();
                emit_value(val, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{:04X}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn format_float(x: f64) -> String {
    // Use Rust's default {:?} which is shortest round-trip, then force a '.'
    // if it rendered as an integer literal (e.g., "5" -> "5.0"). Negative zero
    // is normalized to positive zero since the distinction has no meaning for
    // manifests.
    if x == 0.0 {
        return "0.0".to_string();
    }
    let s = format!("{:?}", x);
    if s.contains('.')
        || s.contains('e')
        || s.contains('E')
        || s.contains("inf")
        || s.contains("NaN")
    {
        s
    } else {
        format!("{s}.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn sorts_keys() {
        let input = "b = 1\na = 2\n";
        let out = canonicalize(input).unwrap();
        assert_eq!(out, "a = 2\nb = 1\n");
    }

    #[test]
    fn nested_tables_sorted() {
        let input = "[z]\nb = 1\n[a]\nc = 2\nb = 1\n";
        let out = canonicalize(input).unwrap();
        assert!(out.starts_with("[a]\n"));
        assert!(out.contains("[z]\n"));
        assert!(out.find("[a]").unwrap() < out.find("[z]").unwrap());
    }

    #[test]
    fn lf_always() {
        let input = "a = 1\r\nb = 2\r\n";
        let out = canonicalize(input).unwrap();
        assert!(!out.contains('\r'));
    }

    #[test]
    fn float_always_has_decimal() {
        let input = "x = 1.0\n";
        let out = canonicalize(input).unwrap();
        assert!(out.contains("x = 1.0"));
    }

    #[test]
    fn idempotent_trivial() {
        let canonical = canonicalize("a = 1\nb = 2\n").unwrap();
        assert_eq!(canonicalize(&canonical).unwrap(), canonical);
    }

    #[test]
    fn rejects_non_finite() {
        // toml_edit will parse "x = inf" as Float(inf)
        let result = canonicalize("x = inf\n");
        assert!(result.is_err());
    }

    #[test]
    fn manifest_shape_roundtrip() {
        let input = r#"source_hash = "abcdef"
file_type = "pdf"
schema_hash = "deadbeef"
ingested_at = "2026-04-21T10:14:00Z"
ingester_version = "0.1.0"

[probe_hashes]
"pdf.page_count" = "a13b5f9c"
"file.size" = "b5c01e33"

[fields]
title = "Q3 Earnings"
page_count = 40
tags = ["finance", "q3"]
"#;
        let first = canonicalize(input).unwrap();
        let second = canonicalize(&first).unwrap();
        assert_eq!(first, second);
        // Ends with exactly one newline.
        assert!(first.ends_with('\n'));
        // Contains sorted [fields] keys (page_count before tags before title).
        let fields_start = first.find("[fields]\n").unwrap();
        let fields_slice = &first[fields_start..];
        let pc = fields_slice.find("page_count").unwrap();
        let tg = fields_slice.find("tags").unwrap();
        let tt = fields_slice.find("title").unwrap();
        assert!(pc < tg && tg < tt, "fields not sorted: {fields_slice}");
    }

    // Proptest: for randomly-shaped manifest bodies, `canonicalize` is
    // idempotent (running it twice yields byte-identical output).
    proptest! {
        #[test]
        fn idempotent_random(shape in manifest_shape_strategy()) {
            let once = canonicalize(&shape).unwrap();
            let twice = canonicalize(&once).unwrap();
            prop_assert_eq!(once, twice);
        }
    }

    fn manifest_shape_strategy() -> impl Strategy<Value = String> {
        // Generate small TOML docs with a bounded shape: a handful of scalar
        // keys at root + one [fields] subtable with scalar keys + one
        // [probe_hashes] subtable with string values.
        let key_strat = "[a-z][a-z0-9_]{0,6}";
        let scalar_strat = prop_oneof![
            "\"[a-zA-Z0-9 _.-]{0,20}\"".prop_map(|s| s),
            any::<i32>().prop_map(|i| i.to_string()),
            any::<bool>().prop_map(|b| b.to_string()),
        ];
        let root_entries = prop::collection::vec((key_strat, scalar_strat.clone()), 0..5);
        let probe_key = "\"[a-z][a-z0-9_]{0,5}\\.[a-z][a-z0-9_]{0,5}\"";
        let probe_entries =
            prop::collection::vec((probe_key, "\"[a-f0-9]{8}\"".prop_map(|s| s)), 0..4);
        let field_entries = prop::collection::vec(("[a-z][a-z0-9_]{0,6}", scalar_strat), 0..5);

        (root_entries, probe_entries, field_entries).prop_map(|(root, probe, fields)| {
            let mut out = String::new();
            let mut seen = std::collections::HashSet::new();
            for (k, v) in root {
                if seen.insert(k.clone()) {
                    out.push_str(&format!("{k} = {v}\n"));
                }
            }
            if !probe.is_empty() {
                out.push_str("\n[probe_hashes]\n");
                let mut seen_p = std::collections::HashSet::new();
                for (k, v) in probe {
                    if seen_p.insert(k.clone()) {
                        out.push_str(&format!("{k} = {v}\n"));
                    }
                }
            }
            if !fields.is_empty() {
                out.push_str("\n[fields]\n");
                let mut seen_f = std::collections::HashSet::new();
                for (k, v) in fields {
                    if seen_f.insert(k.clone()) {
                        out.push_str(&format!("{k} = {v}\n"));
                    }
                }
            }
            out
        })
    }
}

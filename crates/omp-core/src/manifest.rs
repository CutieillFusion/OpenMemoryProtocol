//! Manifest objects — the TOML blob describing one user file.
//! See `02-object-model.md §manifest`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::error::{OmpError, Result};
use crate::hash::Hash;
use crate::toml_canonical;

/// A structural value from the closed type system. Equivalent to a TOML value,
/// but constrained to what the schema type system accepts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Null,
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Datetime(String),
    List(Vec<FieldValue>),
    Object(BTreeMap<String, FieldValue>),
}

impl FieldValue {
    pub fn is_null(&self) -> bool {
        matches!(self, FieldValue::Null)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub source_hash: Hash,
    pub file_type: String,
    pub schema_hash: Hash,
    pub ingested_at: String,
    pub ingester_version: String,
    pub probe_hashes: BTreeMap<String, Hash>,
    pub fields: BTreeMap<String, FieldValue>,
}

impl Manifest {
    /// Serialize to canonical TOML bytes. This is the form that gets hashed.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut doc = DocumentMut::new();
        doc["source_hash"] = value(self.source_hash.hex());
        doc["file_type"] = value(self.file_type.clone());
        doc["schema_hash"] = value(self.schema_hash.hex());
        doc["ingested_at"] = value(self.ingested_at.clone());
        doc["ingester_version"] = value(self.ingester_version.clone());

        let mut probe_tbl = Table::new();
        for (name, h) in &self.probe_hashes {
            probe_tbl[name.as_str()] = value(h.hex());
        }
        probe_tbl.set_implicit(false);
        doc["probe_hashes"] = Item::Table(probe_tbl);

        let mut fields_tbl = Table::new();
        for (name, v) in &self.fields {
            if let Some(item) = field_value_to_item(v)? {
                fields_tbl[name.as_str()] = item;
            }
            // Null values are not emitted; they represent "no value produced".
        }
        fields_tbl.set_implicit(false);
        doc["fields"] = Item::Table(fields_tbl);

        let raw = doc.to_string();
        let canonical = toml_canonical::canonicalize(&raw)?;
        Ok(canonical.into_bytes())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::Corrupt("manifest is not UTF-8".into()))?;
        let doc: DocumentMut = s
            .parse()
            .map_err(|e| OmpError::Corrupt(format!("manifest TOML: {e}")))?;

        let source_hash = read_hash_str(&doc, "source_hash")?;
        let file_type = read_string(&doc, "file_type")?;
        let schema_hash = read_hash_str(&doc, "schema_hash")?;
        let ingested_at = read_string(&doc, "ingested_at")?;
        let ingester_version = read_string(&doc, "ingester_version")?;

        let mut probe_hashes = BTreeMap::new();
        if let Some(Item::Table(t)) = doc.get("probe_hashes") {
            for (k, v) in t.iter() {
                let s = v
                    .as_str()
                    .ok_or_else(|| OmpError::Corrupt(format!("probe_hashes[{k}] not a string")))?;
                let h: Hash = s
                    .parse()
                    .map_err(|e| OmpError::Corrupt(format!("probe_hashes[{k}]: {e}")))?;
                probe_hashes.insert(k.to_string(), h);
            }
        }

        let mut fields = BTreeMap::new();
        if let Some(Item::Table(t)) = doc.get("fields") {
            for (k, v) in t.iter() {
                fields.insert(k.to_string(), item_to_field_value(v)?);
            }
        }

        Ok(Manifest {
            source_hash,
            file_type,
            schema_hash,
            ingested_at,
            ingester_version,
            probe_hashes,
            fields,
        })
    }
}

fn read_string(doc: &DocumentMut, key: &str) -> Result<String> {
    doc.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| OmpError::Corrupt(format!("manifest missing string {key}")))
}

fn read_hash_str(doc: &DocumentMut, key: &str) -> Result<Hash> {
    let s = read_string(doc, key)?;
    s.parse()
        .map_err(|e| OmpError::Corrupt(format!("manifest {key}: {e}")))
}

pub fn field_value_to_item(v: &FieldValue) -> Result<Option<Item>> {
    match v {
        FieldValue::Null => Ok(None),
        FieldValue::String(s) => Ok(Some(value(s.clone()))),
        FieldValue::Int(i) => Ok(Some(value(*i))),
        FieldValue::Float(f) => {
            if !f.is_finite() {
                return Err(OmpError::IngestValidation(format!(
                    "non-finite float in field: {f}"
                )));
            }
            Ok(Some(value(*f)))
        }
        FieldValue::Bool(b) => Ok(Some(value(*b))),
        FieldValue::Datetime(s) => {
            // Re-parse to a proper toml_edit Datetime if possible, else store as string.
            match s.parse::<toml_edit::Datetime>() {
                Ok(dt) => Ok(Some(value(dt))),
                Err(_) => Ok(Some(value(s.clone()))),
            }
        }
        FieldValue::List(items) => {
            let mut arr = Array::new();
            for item in items {
                if let Some(Item::Value(val)) = field_value_to_item(item)? {
                    arr.push(val);
                } else {
                    // lists of nulls / nested tables are not part of the type system
                    return Err(OmpError::IngestValidation(
                        "list element must be a scalar".into(),
                    ));
                }
            }
            Ok(Some(Item::Value(Value::Array(arr))))
        }
        FieldValue::Object(map) => {
            let mut t = InlineTable::new();
            for (k, val) in map {
                if let Some(Item::Value(v)) = field_value_to_item(val)? {
                    t.insert(k, v);
                } else {
                    return Err(OmpError::IngestValidation(format!(
                        "object field {k} must be a scalar"
                    )));
                }
            }
            Ok(Some(Item::Value(Value::InlineTable(t))))
        }
    }
}

fn item_to_field_value(item: &Item) -> Result<FieldValue> {
    match item {
        Item::None => Ok(FieldValue::Null),
        Item::Value(v) => value_to_field_value(v),
        Item::Table(t) => {
            let mut map = BTreeMap::new();
            for (k, v) in t.iter() {
                map.insert(k.to_string(), item_to_field_value(v)?);
            }
            Ok(FieldValue::Object(map))
        }
        Item::ArrayOfTables(arr) => array_of_tables_to_field_value(arr),
    }
}

fn value_to_field_value(v: &Value) -> Result<FieldValue> {
    match v {
        Value::String(s) => Ok(FieldValue::String(s.value().clone())),
        Value::Integer(i) => Ok(FieldValue::Int(*i.value())),
        Value::Float(f) => Ok(FieldValue::Float(*f.value())),
        Value::Boolean(b) => Ok(FieldValue::Bool(*b.value())),
        Value::Datetime(dt) => Ok(FieldValue::Datetime(dt.value().to_string())),
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr.iter() {
                out.push(value_to_field_value(item)?);
            }
            Ok(FieldValue::List(out))
        }
        Value::InlineTable(t) => {
            let mut map = BTreeMap::new();
            for (k, val) in t.iter() {
                map.insert(k.to_string(), value_to_field_value(val)?);
            }
            Ok(FieldValue::Object(map))
        }
    }
}

fn array_of_tables_to_field_value(arr: &ArrayOfTables) -> Result<FieldValue> {
    let mut out = Vec::with_capacity(arr.len());
    for t in arr.iter() {
        let mut map = BTreeMap::new();
        for (k, v) in t.iter() {
            map.insert(k.to_string(), item_to_field_value(v)?);
        }
        out.push(FieldValue::Object(map));
    }
    Ok(FieldValue::List(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Manifest {
        let mut probe_hashes = BTreeMap::new();
        probe_hashes.insert("file.size".to_string(), Hash::of(b"file.size.wasm"));
        probe_hashes.insert(
            "pdf.page_count".to_string(),
            Hash::of(b"pdf.page_count.wasm"),
        );

        let mut fields = BTreeMap::new();
        fields.insert(
            "title".to_string(),
            FieldValue::String("Q3 Earnings Report".into()),
        );
        fields.insert("page_count".to_string(), FieldValue::Int(40));
        fields.insert(
            "tags".to_string(),
            FieldValue::List(vec![
                FieldValue::String("finance".into()),
                FieldValue::String("q3".into()),
            ]),
        );

        Manifest {
            source_hash: Hash::of(b"pdf bytes"),
            file_type: "pdf".into(),
            schema_hash: Hash::of(b"pdf schema"),
            ingested_at: "2026-04-21T10:14:00Z".into(),
            ingester_version: "0.1.0".into(),
            probe_hashes,
            fields,
        }
    }

    #[test]
    fn serialize_is_canonical_and_roundtrips() {
        let m = fixture();
        let bytes = m.serialize().unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Canonical: running canonicalize again is a no-op.
        assert_eq!(crate::toml_canonical::canonicalize(s).unwrap(), s);
        let back = Manifest::parse(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn null_fields_are_omitted() {
        let mut m = fixture();
        m.fields.insert("summary".to_string(), FieldValue::Null);
        let bytes = m.serialize().unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(!s.contains("summary"));
    }
}

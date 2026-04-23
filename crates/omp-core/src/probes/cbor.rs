//! CBOR payload shape for the probe ABI.
//!
//! Input: `{"bytes": <byte string>, "kwargs": <map>}`.
//! Output: any CBOR value, with null meaning "no value produced".

use std::collections::BTreeMap;

use ciborium::value::{Integer, Value as Cbor};

use crate::error::{OmpError, Result};
use crate::manifest::FieldValue;

pub fn encode_input(bytes: &[u8], kwargs: &BTreeMap<String, FieldValue>) -> Result<Vec<u8>> {
    let mut kw_entries: Vec<(Cbor, Cbor)> = Vec::with_capacity(kwargs.len());
    for (k, v) in kwargs {
        kw_entries.push((Cbor::Text(k.clone()), field_to_cbor(v)?));
    }
    let map = Cbor::Map(vec![
        (Cbor::Text("bytes".into()), Cbor::Bytes(bytes.to_vec())),
        (Cbor::Text("kwargs".into()), Cbor::Map(kw_entries)),
    ]);
    let mut out = Vec::with_capacity(bytes.len() + 64);
    ciborium::ser::into_writer(&map, &mut out)
        .map_err(|e| OmpError::internal(format!("cbor encode input: {e}")))?;
    Ok(out)
}

pub fn decode_output(bytes: &[u8]) -> Result<FieldValue> {
    let v: Cbor = ciborium::de::from_reader(bytes)
        .map_err(|e| OmpError::ProbeFailed {
            probe: "<output>".into(),
            reason: format!("decoding output: {e}"),
        })?;
    cbor_to_field(&v)
}

fn field_to_cbor(v: &FieldValue) -> Result<Cbor> {
    Ok(match v {
        FieldValue::Null => Cbor::Null,
        FieldValue::String(s) => Cbor::Text(s.clone()),
        FieldValue::Int(i) => Cbor::Integer((*i).into()),
        FieldValue::Float(f) => Cbor::Float(*f),
        FieldValue::Bool(b) => Cbor::Bool(*b),
        FieldValue::Datetime(s) => Cbor::Text(s.clone()),
        FieldValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(field_to_cbor(item)?);
            }
            Cbor::Array(out)
        }
        FieldValue::Object(map) => {
            let mut out = Vec::with_capacity(map.len());
            for (k, v) in map {
                out.push((Cbor::Text(k.clone()), field_to_cbor(v)?));
            }
            Cbor::Map(out)
        }
    })
}

fn cbor_to_field(v: &Cbor) -> Result<FieldValue> {
    Ok(match v {
        Cbor::Null => FieldValue::Null,
        Cbor::Bool(b) => FieldValue::Bool(*b),
        Cbor::Integer(i) => {
            let as_i64 = i64::try_from(*i).map_err(|e| OmpError::ProbeFailed {
                probe: "<output>".into(),
                reason: format!("integer out of range: {e}"),
            })?;
            FieldValue::Int(as_i64)
        }
        Cbor::Float(f) => FieldValue::Float(*f),
        Cbor::Text(s) => FieldValue::String(s.clone()),
        Cbor::Bytes(b) => FieldValue::String(hex_encode(b)),
        Cbor::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(cbor_to_field(item)?);
            }
            FieldValue::List(out)
        }
        Cbor::Map(entries) => {
            let mut map = BTreeMap::new();
            for (k, v) in entries {
                let key = match k {
                    Cbor::Text(s) => s.clone(),
                    Cbor::Integer(i) => {
                        let n: i64 = (*i).try_into().map_err(|_| OmpError::ProbeFailed {
                            probe: "<output>".into(),
                            reason: "map key integer out of range".into(),
                        })?;
                        n.to_string()
                    }
                    _ => {
                        return Err(OmpError::ProbeFailed {
                            probe: "<output>".into(),
                            reason: "map key is not string or integer".into(),
                        });
                    }
                };
                map.insert(key, cbor_to_field(v)?);
            }
            FieldValue::Object(map)
        }
        Cbor::Tag(_, inner) => cbor_to_field(inner)?,
        other => {
            return Err(OmpError::ProbeFailed {
                probe: "<output>".into(),
                reason: format!("unsupported CBOR value: {other:?}"),
            });
        }
    })
}

fn hex_encode(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(b.len() * 2);
    for byte in b {
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

/// Silence the unused-import warning for `Integer`.
#[allow(dead_code)]
fn _use_integer(_: Integer) {}

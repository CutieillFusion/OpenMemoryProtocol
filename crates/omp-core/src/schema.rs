//! Schema loader + validator.
//!
//! A schema is a TOML file declaring `file_type`, `mime_patterns`, and a set
//! of `[fields.<name>]` blocks. See `04-schemas.md` for the full shape.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::error::{OmpError, Result};
use crate::manifest::FieldValue;

/// How the UI should render a file's bytes. The schema names the kind; the
/// engine surfaces it on `ShowResult::Manifest` so the frontend can pick a
/// renderer without sniffing MIME itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderKind {
    Text,
    Hex,
    Image,
    Markdown,
    Binary,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderHint {
    pub kind: RenderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inline_bytes: Option<u64>,
}

pub const DEFAULT_MAX_INLINE_BYTES: u64 = 65_536;

/// The closed set of value types the schema type system supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldType {
    String,
    Int,
    Float,
    Bool,
    Datetime,
    ListString,
    ListInt,
    ListFloat,
    ListBool,
    ListDatetime,
    Object,
}

impl FieldType {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "string" => FieldType::String,
            "int" => FieldType::Int,
            "float" => FieldType::Float,
            "bool" => FieldType::Bool,
            "datetime" => FieldType::Datetime,
            "list[string]" => FieldType::ListString,
            "list[int]" => FieldType::ListInt,
            "list[float]" => FieldType::ListFloat,
            "list[bool]" => FieldType::ListBool,
            "list[datetime]" => FieldType::ListDatetime,
            "object" => FieldType::Object,
            other => {
                return Err(OmpError::SchemaValidation(format!(
                    "unknown field type: {other:?}"
                )));
            }
        })
    }

    /// Inverse of `parse`. Used by the wire-format `SchemaSummary` so the
    /// frontend autocompleter can render a type label next to each field.
    pub fn as_str(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Int => "int",
            FieldType::Float => "float",
            FieldType::Bool => "bool",
            FieldType::Datetime => "datetime",
            FieldType::ListString => "list[string]",
            FieldType::ListInt => "list[int]",
            FieldType::ListFloat => "list[float]",
            FieldType::ListBool => "list[bool]",
            FieldType::ListDatetime => "list[datetime]",
            FieldType::Object => "object",
        }
    }

    /// For probe return type: `"int?"` etc. — the `?` means nullable.
    pub fn parse_nullable(s: &str) -> Result<(Self, bool)> {
        match s.strip_suffix('?') {
            Some(inner) => Ok((Self::parse(inner)?, true)),
            None => Ok((Self::parse(s)?, false)),
        }
    }

    /// Does the given FieldValue satisfy this type? Null always satisfies
    /// (whether a null is permitted is a separate "required" check).
    pub fn accepts(self, v: &FieldValue) -> bool {
        if v.is_null() {
            return true;
        }
        match (self, v) {
            (FieldType::String, FieldValue::String(_)) => true,
            (FieldType::Int, FieldValue::Int(_)) => true,
            (FieldType::Float, FieldValue::Float(_)) => true,
            // Allow ints where a float was declared, for ergonomic user input.
            (FieldType::Float, FieldValue::Int(_)) => true,
            (FieldType::Bool, FieldValue::Bool(_)) => true,
            (FieldType::Datetime, FieldValue::String(_)) => true,
            (FieldType::Datetime, FieldValue::Datetime(_)) => true,
            (FieldType::Object, FieldValue::Object(_)) => true,
            (FieldType::ListString, FieldValue::List(items)) => {
                items.iter().all(|i| matches!(i, FieldValue::String(_)))
            }
            (FieldType::ListInt, FieldValue::List(items)) => {
                items.iter().all(|i| matches!(i, FieldValue::Int(_)))
            }
            (FieldType::ListFloat, FieldValue::List(items)) => items
                .iter()
                .all(|i| matches!(i, FieldValue::Float(_) | FieldValue::Int(_))),
            (FieldType::ListBool, FieldValue::List(items)) => {
                items.iter().all(|i| matches!(i, FieldValue::Bool(_)))
            }
            (FieldType::ListDatetime, FieldValue::List(items)) => items
                .iter()
                .all(|i| matches!(i, FieldValue::String(_) | FieldValue::Datetime(_))),
            _ => false,
        }
    }
}

/// A single source within a field declaration. The closed set of four sources.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Constant {
        value: FieldValue,
    },
    Probe {
        probe: String,
        args: BTreeMap<String, FieldValue>,
    },
    UserProvided,
    Field {
        from: String,
        transform: Transform,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    Identity,
    Slugify,
    Lower,
}

impl Transform {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "identity" | "" => Transform::Identity,
            "slugify" => Transform::Slugify,
            "lower" => Transform::Lower,
            other => {
                return Err(OmpError::SchemaValidation(format!(
                    "unknown transform: {other:?}"
                )));
            }
        })
    }

    pub fn apply(self, v: FieldValue) -> FieldValue {
        match self {
            Transform::Identity => v,
            Transform::Lower => match v {
                FieldValue::String(s) => FieldValue::String(s.to_lowercase()),
                other => other,
            },
            Transform::Slugify => match v {
                FieldValue::String(s) => FieldValue::String(slugify(&s)),
                other => other,
            },
        }
    }
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub source: Source,
    pub type_: FieldType,
    pub required: bool,
    pub description: Option<String>,
    pub fallback: Option<Box<Field>>,
}

#[derive(Clone, Debug)]
pub struct Schema {
    pub file_type: String,
    pub mime_patterns: Vec<String>,
    pub allow_extra_fields: bool,
    pub fields: Vec<Field>, // topologically sorted
    pub render: Option<RenderHint>,
}

/// Wire-format projection of `Schema` returned by `GET /schemas`. The web UI
/// uses these to drive query autocomplete; it doesn't need the source/probe
/// machinery, so we ship a minimal shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaSummary {
    pub file_type: String,
    pub mime_patterns: Vec<String>,
    pub fields: Vec<SchemaFieldSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaFieldSummary {
    pub name: String,
    pub r#type: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Schema {
    /// Parse schema TOML. Does not yet resolve probe references — that needs a
    /// tree. For pure-syntactic validation, use `parse` and then call
    /// `validate_probe_refs` once a tree is available.
    pub fn parse(bytes: &[u8], filename_stem: &str) -> Result<Self> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::SchemaValidation("schema not UTF-8".into()))?;
        let raw: RawSchema = toml::from_str(s)
            .map_err(|e| OmpError::SchemaValidation(format!("schema TOML: {e}")))?;

        if raw.file_type != filename_stem {
            return Err(OmpError::SchemaValidation(format!(
                "file_type {:?} does not match schema filename stem {:?}",
                raw.file_type, filename_stem
            )));
        }
        if raw.mime_patterns.is_empty() {
            return Err(OmpError::SchemaValidation(
                "schema: mime_patterns must be non-empty".into(),
            ));
        }

        // Convert raw field map to Field list, then topologically sort.
        let mut converted: HashMap<String, Field> = HashMap::new();
        for (name, raw_field) in &raw.fields {
            let field = convert_field(name, raw_field)?;
            converted.insert(name.clone(), field);
        }

        let ordered = topo_sort(&converted)?;
        // For each `source = field`, ensure the `from` is a sibling field.
        for f in &ordered {
            check_field_refs(f, &converted)?;
        }

        let render = raw.render.map(|rr| RenderHint {
            kind: parse_render_kind(&rr.kind),
            max_inline_bytes: rr.max_inline_bytes,
        });

        Ok(Schema {
            file_type: raw.file_type,
            mime_patterns: raw.mime_patterns,
            allow_extra_fields: raw.allow_extra_fields.unwrap_or(false),
            fields: ordered,
            render,
        })
    }

    /// Wire-format summary used by `GET /schemas`. Strips internal source/
    /// fallback shape — the autocompleter only needs name + type + description.
    pub fn summary(&self) -> SchemaSummary {
        SchemaSummary {
            file_type: self.file_type.clone(),
            mime_patterns: self.mime_patterns.clone(),
            fields: self
                .fields
                .iter()
                .map(|f| SchemaFieldSummary {
                    name: f.name.clone(),
                    r#type: f.type_.as_str().to_string(),
                    required: f.required,
                    description: f.description.clone(),
                })
                .collect(),
        }
    }

    /// Resolve the render hint to apply for this schema. Returns the explicit
    /// `[render]` block when set; otherwise infers from `mime_patterns[0]` so
    /// existing schemas keep working without an opt-in.
    pub fn effective_render(&self) -> RenderHint {
        if let Some(r) = &self.render {
            return r.clone();
        }
        let kind = self
            .mime_patterns
            .first()
            .map(|m| heuristic_render_kind(m))
            .unwrap_or(RenderKind::Binary);
        RenderHint {
            kind,
            max_inline_bytes: None,
        }
    }

    /// Validate that every `probe` reference names a probe whose `.wasm` +
    /// `.probe.toml` pair is present in the provided `probe_names` set.
    pub fn validate_probe_refs(&self, probe_names: &HashSet<String>) -> Result<()> {
        for f in &self.fields {
            validate_probe_refs_inner(f, probe_names)?;
        }
        Ok(())
    }
}

/// Parse a TOML `kind = "..."` value. Unknown kinds resolve to `Binary` so a
/// future schema written with a newer kind doesn't break older readers.
fn parse_render_kind(s: &str) -> RenderKind {
    match s.trim().to_ascii_lowercase().as_str() {
        "text" => RenderKind::Text,
        "hex" => RenderKind::Hex,
        "image" => RenderKind::Image,
        "markdown" => RenderKind::Markdown,
        "binary" => RenderKind::Binary,
        "none" => RenderKind::None,
        _ => RenderKind::Binary,
    }
}

/// Cheap MIME → kind mapping for schemas that don't declare `[render]`.
fn heuristic_render_kind(mime_pattern: &str) -> RenderKind {
    let lower = mime_pattern.to_ascii_lowercase();
    if lower.starts_with("text/markdown") || lower == "text/x-markdown" {
        return RenderKind::Markdown;
    }
    if lower.starts_with("image/") {
        return RenderKind::Image;
    }
    if lower.starts_with("text/") {
        return RenderKind::Text;
    }
    if lower.starts_with("application/json")
        || lower.starts_with("application/toml")
        || lower.starts_with("application/yaml")
        || lower.starts_with("application/x-yaml")
        || lower.contains("xml")
    {
        return RenderKind::Text;
    }
    RenderKind::Binary
}

fn validate_probe_refs_inner(f: &Field, probes: &HashSet<String>) -> Result<()> {
    match &f.source {
        Source::Probe { probe, .. } => {
            if !probes.contains(probe) {
                return Err(OmpError::SchemaValidation(format!(
                    "field {:?}: probe {:?} not found in tree",
                    f.name, probe
                )));
            }
        }
        _ => {}
    }
    if let Some(fb) = &f.fallback {
        validate_probe_refs_inner(fb, probes)?;
    }
    Ok(())
}

fn check_field_refs(f: &Field, all: &HashMap<String, Field>) -> Result<()> {
    if let Source::Field { from, .. } = &f.source {
        if !all.contains_key(from) {
            return Err(OmpError::SchemaValidation(format!(
                "field {:?}: `from = {:?}` has no matching field",
                f.name, from
            )));
        }
    }
    if let Some(fb) = &f.fallback {
        check_field_refs(fb, all)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    file_type: String,
    mime_patterns: Vec<String>,
    #[serde(default)]
    allow_extra_fields: Option<bool>,
    #[serde(default)]
    fields: HashMap<String, RawField>,
    #[serde(default)]
    render: Option<RawRender>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRender {
    kind: String,
    #[serde(default)]
    max_inline_bytes: Option<u64>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RawField {
    source: String,
    /// Type is required on the outer field; for a `fallback` wrapper it is
    /// inherited from the parent when omitted.
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    required: Option<bool>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    value: Option<toml::Value>, // for `constant`
    #[serde(default)]
    probe: Option<String>, // for `probe`
    #[serde(default)]
    args: Option<HashMap<String, toml::Value>>, // for `probe`
    #[serde(default)]
    from: Option<String>, // for `field`
    #[serde(default)]
    transform: Option<String>, // for `field`
    #[serde(default)]
    fallback: Option<Box<RawField>>,
}

fn convert_field(name: &str, raw: &RawField) -> Result<Field> {
    convert_field_with_inherited_type(name, raw, None)
}

fn convert_field_with_inherited_type(
    name: &str,
    raw: &RawField,
    inherited: Option<FieldType>,
) -> Result<Field> {
    let ty = match raw.r#type.as_deref() {
        Some(s) => FieldType::parse(s)?,
        None => inherited
            .ok_or_else(|| OmpError::SchemaValidation(format!("field {name:?}: missing `type`")))?,
    };
    let source = match raw.source.as_str() {
        "constant" => {
            let v = raw.value.as_ref().ok_or_else(|| {
                OmpError::SchemaValidation(format!("field {name:?}: `constant` needs `value`"))
            })?;
            Source::Constant {
                value: toml_value_to_field(v)?,
            }
        }
        "probe" => {
            let probe = raw.probe.clone().ok_or_else(|| {
                OmpError::SchemaValidation(format!("field {name:?}: `probe` needs `probe`"))
            })?;
            let mut args = BTreeMap::new();
            if let Some(a) = &raw.args {
                for (k, v) in a {
                    args.insert(k.clone(), toml_value_to_field(v)?);
                }
            }
            Source::Probe { probe, args }
        }
        "user_provided" => Source::UserProvided,
        "field" => {
            let from = raw.from.clone().ok_or_else(|| {
                OmpError::SchemaValidation(format!("field {name:?}: `field` needs `from`"))
            })?;
            let transform = Transform::parse(raw.transform.as_deref().unwrap_or(""))?;
            Source::Field { from, transform }
        }
        "fallback" => {
            return Err(OmpError::SchemaValidation(format!(
                "field {name:?}: `fallback` is not a valid `source` (it is a nested wrapper)"
            )));
        }
        other => {
            return Err(OmpError::SchemaValidation(format!(
                "field {name:?}: unknown source {other:?}"
            )));
        }
    };

    let fallback = match &raw.fallback {
        Some(raw_fb) => Some(Box::new(convert_field_with_inherited_type(
            &format!("{name}.fallback"),
            raw_fb,
            Some(ty),
        )?)),
        None => None,
    };

    Ok(Field {
        name: name.to_string(),
        source,
        type_: ty,
        required: raw.required.unwrap_or(false),
        description: raw.description.clone(),
        fallback,
    })
}

/// Kahn's algorithm. Returns fields in dependency order. Rejects cycles and
/// self-references.
fn topo_sort(fields: &HashMap<String, Field>) -> Result<Vec<Field>> {
    // Dependencies: `source = field` depends on `from`.
    let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut indeg: HashMap<&str, usize> = HashMap::new();
    for name in fields.keys() {
        indeg.insert(name.as_str(), 0);
    }
    for (name, f) in fields {
        if let Source::Field { from, .. } = &f.source {
            if from == name {
                return Err(OmpError::SchemaValidation(format!(
                    "field {name:?} references itself"
                )));
            }
            deps.entry(from.as_str()).or_default().push(name.as_str());
            *indeg.entry(name.as_str()).or_insert(0) += 1;
        }
    }
    // Sort ready queue alphabetically so ordering is deterministic.
    let mut ready: Vec<&str> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(k, _)| *k)
        .collect();
    ready.sort();

    let mut out: Vec<Field> = Vec::with_capacity(fields.len());
    while let Some(n) = ready.pop() {
        let f = fields.get(n).expect("field exists").clone();
        out.push(f);
        if let Some(dependents) = deps.get(n) {
            let mut unlocked: Vec<&str> = Vec::new();
            for dep in dependents {
                let d = indeg.get_mut(dep).unwrap();
                *d -= 1;
                if *d == 0 {
                    unlocked.push(dep);
                }
            }
            unlocked.sort();
            ready.extend(unlocked);
            ready.sort_by(|a, b| b.cmp(a));
        }
    }
    if out.len() != fields.len() {
        return Err(OmpError::SchemaValidation(
            "schema: field dependency cycle".into(),
        ));
    }
    Ok(out)
}

fn toml_value_to_field(v: &toml::Value) -> Result<FieldValue> {
    Ok(match v {
        toml::Value::String(s) => FieldValue::String(s.clone()),
        toml::Value::Integer(i) => FieldValue::Int(*i),
        toml::Value::Float(f) => FieldValue::Float(*f),
        toml::Value::Boolean(b) => FieldValue::Bool(*b),
        toml::Value::Datetime(dt) => FieldValue::Datetime(dt.to_string()),
        toml::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(toml_value_to_field(item)?);
            }
            FieldValue::List(out)
        }
        toml::Value::Table(t) => {
            let mut map = BTreeMap::new();
            for (k, v) in t.iter() {
                map.insert(k.clone(), toml_value_to_field(v)?);
            }
            FieldValue::Object(map)
        }
    })
}

/// fnmatch-style glob matching over MIME types. Supports `*` and `?`.
/// Case-insensitive per the spec.
pub fn mime_matches(pattern: &str, mime: &str) -> bool {
    glob_match(&pattern.to_ascii_lowercase(), &mime.to_ascii_lowercase())
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_inner(&p, 0, &t, 0)
}

fn glob_inner(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            // Try consuming zero or more characters.
            let mut i = ti;
            loop {
                if glob_inner(p, pi + 1, t, i) {
                    return true;
                }
                if i == t.len() {
                    return false;
                }
                i += 1;
            }
        }
        '?' => {
            if ti == t.len() {
                return false;
            }
            glob_inner(p, pi + 1, t, ti + 1)
        }
        c => {
            if ti == t.len() || t[ti] != c {
                return false;
            }
            glob_inner(p, pi + 1, t, ti + 1)
        }
    }
}

/// The built-in `_minimal` schema content. Frozen bytes; its hash is stable.
pub const MINIMAL_SCHEMA: &[u8] = br#"file_type = "_minimal"
mime_patterns = ["*/*"]

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"

[fields.sha256]
source = "probe"
probe = "file.sha256"
type = "string"

[fields.mime]
source = "probe"
probe = "file.mime"
type = "string"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_schema() {
        let s = Schema::parse(MINIMAL_SCHEMA, "_minimal").unwrap();
        assert_eq!(s.file_type, "_minimal");
        assert_eq!(s.fields.len(), 3);
    }

    #[test]
    fn rejects_unknown_source() {
        let bad = br#"file_type = "pdf"
mime_patterns = ["application/pdf"]
[fields.x]
source = "magic"
type = "string"
"#;
        assert!(Schema::parse(bad, "pdf").is_err());
    }

    #[test]
    fn rejects_fallback_as_source() {
        let bad = br#"file_type = "pdf"
mime_patterns = ["application/pdf"]
[fields.x]
source = "fallback"
type = "string"
"#;
        assert!(Schema::parse(bad, "pdf").is_err());
    }

    #[test]
    fn detects_cycle() {
        let bad = br#"file_type = "pdf"
mime_patterns = ["application/pdf"]
[fields.a]
source = "field"
from = "b"
type = "string"
[fields.b]
source = "field"
from = "a"
type = "string"
"#;
        let err = Schema::parse(bad, "pdf").unwrap_err();
        assert!(matches!(err, OmpError::SchemaValidation(_)));
    }

    #[test]
    fn detects_self_reference() {
        let bad = br#"file_type = "pdf"
mime_patterns = ["application/pdf"]
[fields.a]
source = "field"
from = "a"
type = "string"
"#;
        assert!(Schema::parse(bad, "pdf").is_err());
    }

    #[test]
    fn topological_order() {
        let good = br#"file_type = "pdf"
mime_patterns = ["application/pdf"]
[fields.slug]
source = "field"
from = "title"
transform = "slugify"
type = "string"
[fields.title]
source = "user_provided"
type = "string"
"#;
        let s = Schema::parse(good, "pdf").unwrap();
        let names: Vec<&str> = s.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["title", "slug"]);
    }

    #[test]
    fn file_type_must_match_filename() {
        let input = br#"file_type = "pdf"
mime_patterns = ["application/pdf"]
"#;
        assert!(Schema::parse(input, "audio").is_err());
    }

    #[test]
    fn mime_patterns_required() {
        let input = br#"file_type = "pdf"
mime_patterns = []
"#;
        assert!(Schema::parse(input, "pdf").is_err());
    }

    #[test]
    fn slugify_is_reasonable() {
        assert_eq!(slugify("Q3 Earnings Report!"), "q3-earnings-report");
        assert_eq!(slugify("  spaces  "), "spaces");
    }

    #[test]
    fn transform_parse() {
        assert_eq!(Transform::parse("").unwrap(), Transform::Identity);
        assert_eq!(Transform::parse("slugify").unwrap(), Transform::Slugify);
        assert!(Transform::parse("explode").is_err());
    }

    #[test]
    fn mime_glob_matches() {
        assert!(mime_matches("application/pdf", "application/pdf"));
        assert!(mime_matches("text/*", "text/markdown"));
        assert!(mime_matches("*/*", "image/png"));
        assert!(!mime_matches("application/pdf", "text/plain"));
    }

    #[test]
    fn parses_render_block() {
        let input = br#"file_type = "pdf"
mime_patterns = ["application/pdf"]

[render]
kind = "binary"
max_inline_bytes = 1024
"#;
        let s = Schema::parse(input, "pdf").unwrap();
        let r = s.render.as_ref().unwrap();
        assert_eq!(r.kind, RenderKind::Binary);
        assert_eq!(r.max_inline_bytes, Some(1024));
    }

    #[test]
    fn effective_render_uses_explicit_block_when_set() {
        let input = br#"file_type = "md"
mime_patterns = ["application/octet-stream"]

[render]
kind = "markdown"
"#;
        let s = Schema::parse(input, "md").unwrap();
        let r = s.effective_render();
        assert_eq!(r.kind, RenderKind::Markdown);
        assert!(r.max_inline_bytes.is_none());
    }

    #[test]
    fn effective_render_falls_back_to_mime_heuristic() {
        let png = Schema::parse(
            br#"file_type = "png"
mime_patterns = ["image/png"]
"#,
            "png",
        )
        .unwrap();
        assert_eq!(png.effective_render().kind, RenderKind::Image);

        let md = Schema::parse(
            br#"file_type = "md"
mime_patterns = ["text/markdown"]
"#,
            "md",
        )
        .unwrap();
        assert_eq!(md.effective_render().kind, RenderKind::Markdown);

        let opaque = Schema::parse(
            br#"file_type = "blob"
mime_patterns = ["application/octet-stream"]
"#,
            "blob",
        )
        .unwrap();
        assert_eq!(opaque.effective_render().kind, RenderKind::Binary);
    }

    #[test]
    fn unknown_render_kind_resolves_to_binary() {
        // Forward-compat: a future kind a v1 reader doesn't recognize should
        // still parse, just resolve to Binary.
        let input = br#"file_type = "x"
mime_patterns = ["application/x-x"]

[render]
kind = "future-format"
"#;
        let s = Schema::parse(input, "x").unwrap();
        assert_eq!(s.effective_render().kind, RenderKind::Binary);
    }

    #[test]
    fn field_type_accepts() {
        assert!(FieldType::String.accepts(&FieldValue::String("x".into())));
        assert!(!FieldType::String.accepts(&FieldValue::Int(1)));
        assert!(FieldType::Float.accepts(&FieldValue::Int(1))); // int coerces into float slot
        assert!(FieldType::ListString.accepts(&FieldValue::List(vec![
            FieldValue::String("a".into()),
            FieldValue::String("b".into()),
        ])));
        assert!(!FieldType::ListString.accepts(&FieldValue::List(vec![FieldValue::Int(1)])));
    }
}

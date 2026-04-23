//! Parser for the `.probe.toml` sidecar that lives next to every probe
//! `.wasm` blob. See `docs/design/05-probes.md` and
//! `docs/design/12-large-files.md §max_input_bytes`.

use serde::Deserialize;

use crate::error::{OmpError, Result};

/// Parsed probe manifest. Only the fields the host currently consumes are
/// deserialized into strongly-typed members; everything else is tolerated so
/// probe authors can add descriptive metadata without bumping a grammar.
#[derive(Clone, Debug, Default)]
pub struct ProbeManifest {
    pub name: String,
    pub returns: Option<String>,
    pub accepts_kwargs: Vec<String>,
    pub description: Option<String>,
    /// From `[limits].max_input_bytes`. When `Some`, the engine skips this
    /// probe if the blob content length exceeds the cap. `None` ⇒ no
    /// explicit per-probe cap (falls back to the repo-wide `memory_mb`).
    pub max_input_bytes: Option<u64>,
    /// From `[limits].memory_mb` / `fuel` / `wall_clock_s`. Reserved for a
    /// future iteration that lets probes lower the host defaults; currently
    /// captured but not applied — repo defaults from `omp.toml` still win.
    pub limits_override: ProbeLimitsOverride,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeLimitsOverride {
    pub memory_mb: Option<u32>,
    pub fuel: Option<u64>,
    pub wall_clock_s: Option<u32>,
}

impl ProbeManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            name: String,
            #[serde(default)]
            returns: Option<String>,
            #[serde(default)]
            accepts_kwargs: Vec<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            limits: Option<Limits>,
        }
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct Limits {
            #[serde(default)]
            memory_mb: Option<u32>,
            #[serde(default)]
            fuel: Option<u64>,
            #[serde(default)]
            wall_clock_s: Option<u32>,
            #[serde(default)]
            max_input_bytes: Option<u64>,
        }

        let s = std::str::from_utf8(bytes).map_err(|_| {
            OmpError::SchemaValidation("probe.toml is not UTF-8".into())
        })?;
        let raw: Raw = toml::from_str(s)
            .map_err(|e| OmpError::SchemaValidation(format!("probe.toml: {e}")))?;
        let (max_input_bytes, limits_override) = match raw.limits {
            Some(l) => (
                l.max_input_bytes,
                ProbeLimitsOverride {
                    memory_mb: l.memory_mb,
                    fuel: l.fuel,
                    wall_clock_s: l.wall_clock_s,
                },
            ),
            None => (None, ProbeLimitsOverride::default()),
        };
        Ok(ProbeManifest {
            name: raw.name,
            returns: raw.returns,
            accepts_kwargs: raw.accepts_kwargs,
            description: raw.description,
            max_input_bytes,
            limits_override,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_manifest_parses() {
        let m = ProbeManifest::parse(
            br#"name = "file.size"
returns = "int"
accepts_kwargs = []
description = "Byte length."
"#,
        )
        .unwrap();
        assert_eq!(m.name, "file.size");
        assert_eq!(m.returns.as_deref(), Some("int"));
        assert!(m.max_input_bytes.is_none());
    }

    #[test]
    fn limits_section_is_parsed() {
        let m = ProbeManifest::parse(
            br#"name = "pdf.page_count"
returns = "int"
accepts_kwargs = []

[limits]
memory_mb = 128
fuel = 2000000000
wall_clock_s = 20
max_input_bytes = 33554432
"#,
        )
        .unwrap();
        assert_eq!(m.max_input_bytes, Some(33554432));
        assert_eq!(m.limits_override.memory_mb, Some(128));
        assert_eq!(m.limits_override.wall_clock_s, Some(20));
    }

    #[test]
    fn unknown_fields_rejected() {
        let err = ProbeManifest::parse(
            br#"name = "x"
returns = "int"
accepts_kwargs = []
mystery_field = 1
"#,
        )
        .unwrap_err();
        assert!(matches!(err, OmpError::SchemaValidation(_)));
    }
}

//! Starter probe pack: compiled WASM blobs + their sibling `.probe.toml` files
//! embedded at build time.
//!
//! The `.wasm` blobs are staged by `scripts/build-probes.sh` under
//! `crates/omp-core/build/wasm/`. The `.probe.toml` files are hand-authored
//! under `crates/omp-core/starter-probes/`. Both are tree-committed at
//! `omp init` time.

/// A single starter probe — its namespace-qualified name plus the two blobs
/// that belong in the tree.
pub struct StarterProbe {
    pub name: &'static str, // "file.size", "text.frontmatter", …
    pub namespace: &'static str,
    pub basename: &'static str, // the bit after the dot: "size", "frontmatter"
    pub wasm: &'static [u8],
    pub manifest_toml: &'static [u8],
}

impl StarterProbe {
    /// As of `docs/design/23-probe-marketplace.md`, each probe lives in its
    /// own directory `probes/<namespace>/<basename>/` containing
    /// `probe.wasm`, `probe.toml`, and optional companions (`README.md`,
    /// `source/`, examples). The per-folder identity is what the
    /// marketplace publishes and installs.
    pub fn tree_path_dir(&self) -> String {
        format!("probes/{}/{}", self.namespace, self.basename)
    }
    pub fn tree_path_wasm(&self) -> String {
        format!("{}/probe.wasm", self.tree_path_dir())
    }
    pub fn tree_path_manifest(&self) -> String {
        format!("{}/probe.toml", self.tree_path_dir())
    }
}

macro_rules! probe {
    ($name:literal, $ns:literal, $base:literal) => {
        StarterProbe {
            name: $name,
            namespace: $ns,
            basename: $base,
            wasm: include_bytes!(concat!("../../build/wasm/", $name, ".wasm")),
            manifest_toml: include_bytes!(concat!("../../starter-probes/", $name, ".probe.toml")),
        }
    };
}

/// The v1 starter pack: three universal `file.*` probes plus the
/// format-specific probes referenced by the default schemas in
/// `docs/design/26-default-schemas-and-probes.md`. Probes for audio/video
/// metadata (decoders) remain deferred and are not in the starter pack.
pub const STARTER_PROBES: &[StarterProbe] = &[
    probe!("file.size", "file", "size"),
    probe!("file.mime", "file", "mime"),
    probe!("file.sha256", "file", "sha256"),
    probe!("text.line_count", "text", "line_count"),
    probe!("image.dimensions", "image", "dimensions"),
    probe!("audio.duration_seconds", "audio", "duration_seconds"),
    probe!("video.duration_seconds", "video", "duration_seconds"),
    probe!("video.dimensions", "video", "dimensions"),
    probe!("pdf.page_count", "pdf", "page_count"),
];

pub fn starter_schemas() -> Vec<(&'static str, &'static [u8])> {
    vec![
        (
            "schemas/text/schema.toml",
            include_bytes!("../../starter-schemas/text/schema.toml"),
        ),
        (
            "schemas/markdown/schema.toml",
            include_bytes!("../../starter-schemas/markdown/schema.toml"),
        ),
        (
            "schemas/png/schema.toml",
            include_bytes!("../../starter-schemas/png/schema.toml"),
        ),
        (
            "schemas/jpeg/schema.toml",
            include_bytes!("../../starter-schemas/jpeg/schema.toml"),
        ),
        (
            "schemas/mp3/schema.toml",
            include_bytes!("../../starter-schemas/mp3/schema.toml"),
        ),
        (
            "schemas/wav/schema.toml",
            include_bytes!("../../starter-schemas/wav/schema.toml"),
        ),
        (
            "schemas/mp4/schema.toml",
            include_bytes!("../../starter-schemas/mp4/schema.toml"),
        ),
        (
            "schemas/pdf/schema.toml",
            include_bytes!("../../starter-schemas/pdf/schema.toml"),
        ),
    ]
}

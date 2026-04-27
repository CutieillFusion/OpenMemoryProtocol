//! OpenMemoryProtocol core.
//!
//! Implements the v1 design: object model, disk ObjectStore, paths, canonical
//! TOML, schema loader, ingest engine, WASM probe host, and the `omp_core::api`
//! contract consumed by the CLI and HTTP server. See `docs/design/` for the
//! authoritative design.

pub mod api;
pub mod audit;
pub mod chunks;
pub mod commit;
pub mod config;
pub mod encrypted_manifest;
pub mod engine;
pub mod error;
pub mod gc;
pub mod hash;
pub mod hex;
pub mod keys;
pub mod manifest;
pub mod object;
pub mod paths;
pub mod probes;
pub mod query;
pub mod refs;
pub mod registry;
pub mod schema;
pub mod share;
pub mod store;
pub mod tenant;
pub mod time;
pub mod toml_canonical;
pub mod tree;
pub mod uploads;
pub mod walker;

pub use error::{ErrorCode, OmpError, Result};
pub use hash::Hash;
pub use object::{frame, hash_of, ObjectType};

/// Crate version as embedded in every manifest's `ingester_version` field.
pub const INGESTER_VERSION: &str = env!("CARGO_PKG_VERSION");

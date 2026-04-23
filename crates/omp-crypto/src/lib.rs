//! Crypto primitives for OpenMemoryProtocol end-to-end encryption.
//!
//! See `docs/design/13-end-to-end-encryption.md` for the full design.
//!
//! This crate is deliberately minimal:
//!   - Argon2id for passphrase → root key
//!   - HKDF-SHA-256 for subkey derivation
//!   - ChaCha20-Poly1305 for every AEAD operation
//!   - X25519 (via `x25519-dalek`) for identity + recipient wraps
//!
//! The crate is the one audit surface for cryptographic decisions in OMP —
//! it refuses unsafe code and keeps its dependency graph small.

#![forbid(unsafe_code)]

pub mod aead;
pub mod chunk_nonce;
pub mod error;
pub mod identity;
pub mod kdf;

pub use error::{CryptoError, Result};

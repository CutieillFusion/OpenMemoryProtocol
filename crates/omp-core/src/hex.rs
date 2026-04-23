//! Lowercase-hex encode/decode used throughout the crate.
//!
//! A handful of call sites need to emit or parse hex outside of `Hash`
//! (tree-name ciphertext, share `wrapped_key`, identity public keys,
//! SHA-256 digests embedded in TOML). Centralizing here keeps the
//! formatting byte-identical across those sites.

use std::fmt::Write as _;

use crate::error::{OmpError, Result};

pub fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

pub fn decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(OmpError::Corrupt(format!(
            "hex string has odd length: {}",
            s.len()
        )));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.as_bytes().chunks(2) {
        let hi = nibble(pair[0])?;
        let lo = nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

pub fn nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(OmpError::Corrupt(format!(
            "invalid hex char: {:?}",
            c as char
        ))),
    }
}

/// `SHA-256(bytes)` emitted as lowercase hex. Used for the `source_hash`
/// streaming built-in and the tenant-token hash in the registry.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    encode(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let orig = vec![0u8, 1, 2, 0xff];
        let enc = encode(&orig);
        assert_eq!(enc, "000102ff");
        assert_eq!(decode(&enc).unwrap(), orig);
    }

    #[test]
    fn sha256_hex_matches_manual() {
        let got = sha256_hex(b"");
        assert_eq!(
            got,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn decode_rejects_odd_length() {
        assert!(decode("abc").is_err());
    }

    #[test]
    fn decode_rejects_non_hex() {
        assert!(decode("zz").is_err());
    }
}

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let out = hasher.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&out);
        Hash(arr)
    }

    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use std::fmt::Write as _;
            write!(s, "{:02x}", b).unwrap();
        }
        s
    }

    pub fn short(&self) -> String {
        self.hex().chars().take(12).collect()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.hex())
    }
}

impl FromStr for Hash {
    type Err = HashParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(HashParseError::WrongLength(s.len()));
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_digit(chunk[0])?;
            let lo = hex_digit(chunk[1])?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Hash(out))
    }
}

fn hex_digit(c: u8) -> Result<u8, HashParseError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(HashParseError::InvalidChar(c as char)),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HashParseError {
    #[error("hash has wrong length: {0} (expected 64 hex chars)")]
    WrongLength(usize),
    #[error("hash contains invalid hex character: {0:?}")]
    InvalidChar(char),
}

impl Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sha256_of_empty() {
        let h = Hash::of(b"");
        assert_eq!(
            h.hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn roundtrip() {
        let h = Hash::of(b"hello");
        let reparsed: Hash = h.hex().parse().unwrap();
        assert_eq!(h, reparsed);
    }

    #[test]
    fn rejects_short() {
        assert!("abc".parse::<Hash>().is_err());
    }

    #[test]
    fn rejects_non_hex() {
        let mut s = "a".repeat(63);
        s.push('z');
        assert!(s.parse::<Hash>().is_err());
    }
}

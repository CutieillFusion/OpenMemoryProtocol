//! Commit objects: Git-style header-then-body. See `02-object-model.md §commit`.
//!
//! ```text
//! tree <hash>
//! parent <hash>              (0..n)
//! author <name> <<email>> <timestamp>
//!
//! <message>
//! ```
//!
//! For encrypted tenants, the message body is AEAD-sealed under the
//! tenant's `commit_key` and hex-encoded on a single line, prefixed with
//! the marker `!encrypted-message v1 `. Headers stay plaintext so the
//! commit DAG remains walkable server-side (doc 13 §What is encrypted).

use crate::error::{OmpError, Result};
use crate::hash::Hash;

/// Prefix on the body line when the message is sealed. Parsers auto-detect.
const ENCRYPTED_MESSAGE_PREFIX: &str = "!encrypted-message v1 ";
/// AEAD associated-data label binding a sealed message to this usage.
const COMMIT_MSG_AAD: &[u8] = b"omp-commit-message";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Author {
    pub name: String,
    pub email: String,
    pub timestamp: String, // ISO 8601 UTC.
}

impl Author {
    pub fn format(&self) -> String {
        format!("{} <{}> {}", self.name, self.email, self.timestamp)
    }

    pub fn parse(s: &str) -> Result<Self> {
        // `<name> <<email>> <timestamp>`. Walk from the right: timestamp is the
        // last whitespace-delimited token, email is the `<...>` before it,
        // everything else is name.
        let s = s.trim();
        let (rest, timestamp) = s
            .rsplit_once(' ')
            .ok_or_else(|| OmpError::Corrupt("commit author: missing timestamp".into()))?;
        let rest = rest.trim_end();
        let email_start = rest
            .rfind('<')
            .ok_or_else(|| OmpError::Corrupt("commit author: missing '<'".into()))?;
        let email_end = rest
            .rfind('>')
            .ok_or_else(|| OmpError::Corrupt("commit author: missing '>'".into()))?;
        if email_end < email_start {
            return Err(OmpError::Corrupt("commit author: unbalanced '<>'".into()));
        }
        let name = rest[..email_start].trim_end().to_string();
        let email = rest[email_start + 1..email_end].to_string();
        Ok(Author {
            name,
            email,
            timestamp: timestamp.to_string(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Hash,
    pub parents: Vec<Hash>,
    pub author: Author,
    pub message: String,
}

impl Commit {
    pub fn serialize(&self) -> Vec<u8> {
        self.serialize_with_commit_key(None)
            .expect("plaintext serialize cannot fail")
    }

    /// Serialize with optional message encryption. `None` produces the v1
    /// plaintext body; `Some(commit_key)` seals the message body under
    /// ChaCha20-Poly1305 with a deterministic nonce derived from the key
    /// and the message bytes, keeping re-serialization byte-stable.
    pub fn serialize_with_commit_key(&self, commit_key: Option<&[u8; 32]>) -> Result<Vec<u8>> {
        let mut out = String::new();
        out.push_str("tree ");
        out.push_str(&self.tree.hex());
        out.push('\n');
        for p in &self.parents {
            out.push_str("parent ");
            out.push_str(&p.hex());
            out.push('\n');
        }
        out.push_str("author ");
        out.push_str(&self.author.format());
        out.push('\n');
        out.push('\n');
        match commit_key {
            None => {
                let msg = self.message.trim_end_matches('\n');
                out.push_str(msg);
                out.push('\n');
            }
            Some(key) => {
                let msg = self.message.trim_end_matches('\n');
                let nonce = crate::keys::derive_nonce(key, b"commit-msg", msg.as_bytes())?;
                let sealed = omp_crypto::aead::seal(key, &nonce, COMMIT_MSG_AAD, msg.as_bytes())
                    .map_err(|e| OmpError::internal(format!("seal commit message: {e}")))?;
                out.push_str(ENCRYPTED_MESSAGE_PREFIX);
                out.push_str(&crate::share::hex_encode(&sealed));
                out.push('\n');
            }
        }
        Ok(out.into_bytes())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_with_commit_key(bytes, None)
    }

    pub fn parse_with_commit_key(bytes: &[u8], commit_key: Option<&[u8; 32]>) -> Result<Self> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| OmpError::Corrupt("commit is not UTF-8".into()))?;

        let mut tree: Option<Hash> = None;
        let mut parents: Vec<Hash> = Vec::new();
        let mut author: Option<Author> = None;

        let mut lines = s.split_inclusive('\n');
        let mut header_bytes_consumed = 0usize;
        for line in &mut lines {
            header_bytes_consumed += line.len();
            let l = line.trim_end_matches('\n');
            if l.is_empty() {
                break;
            }
            if let Some(rest) = l.strip_prefix("tree ") {
                tree = Some(
                    rest.parse()
                        .map_err(|e| OmpError::Corrupt(format!("commit tree: {e}")))?,
                );
            } else if let Some(rest) = l.strip_prefix("parent ") {
                parents.push(
                    rest.parse()
                        .map_err(|e| OmpError::Corrupt(format!("commit parent: {e}")))?,
                );
            } else if let Some(rest) = l.strip_prefix("author ") {
                author = Some(Author::parse(rest)?);
            } else {
                return Err(OmpError::Corrupt(format!(
                    "commit: unknown header line: {l:?}"
                )));
            }
        }

        let body = &s[header_bytes_consumed..];
        let body_trimmed = body.trim_end_matches('\n');

        let message = if let Some(hex_sealed) = body_trimmed.strip_prefix(ENCRYPTED_MESSAGE_PREFIX)
        {
            let key = commit_key.ok_or_else(|| {
                OmpError::Unauthorized(
                    "commit message is encrypted but no commit_key supplied".into(),
                )
            })?;
            let sealed = crate::share::hex_decode(hex_sealed)?;
            let opened = omp_crypto::aead::open(key, COMMIT_MSG_AAD, &sealed).map_err(|_| {
                OmpError::Unauthorized(
                    "commit message did not open (wrong commit_key or tampered)".into(),
                )
            })?;
            String::from_utf8(opened)
                .map_err(|_| OmpError::Corrupt("decrypted commit message is not UTF-8".into()))?
        } else {
            body_trimmed.to_string()
        };

        Ok(Commit {
            tree: tree.ok_or_else(|| OmpError::Corrupt("commit: missing tree".into()))?,
            parents,
            author: author.ok_or_else(|| OmpError::Corrupt("commit: missing author".into()))?,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(parents: Vec<Hash>) -> Commit {
        Commit {
            tree: Hash::of(b"root"),
            parents,
            author: Author {
                name: "claude-code".into(),
                email: "claude@local".into(),
                timestamp: "2026-04-21T10:14:00Z".into(),
            },
            message: "add Q3 earnings report\ningested under schema pdf@77c10e42".into(),
        }
    }

    #[test]
    fn roundtrip_single_parent() {
        let c = fixture(vec![Hash::of(b"p")]);
        let bytes = c.serialize();
        let back = Commit::parse(&bytes).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn roundtrip_root_commit() {
        let c = fixture(vec![]);
        let bytes = c.serialize();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(!s.contains("parent "));
        let back = Commit::parse(&bytes).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn roundtrip_merge_commit() {
        let c = fixture(vec![Hash::of(b"p1"), Hash::of(b"p2")]);
        let bytes = c.serialize();
        let back = Commit::parse(&bytes).unwrap();
        assert_eq!(back, c);
        // Parent order preserved.
        assert_eq!(back.parents[0], Hash::of(b"p1"));
        assert_eq!(back.parents[1], Hash::of(b"p2"));
    }

    #[test]
    fn exactly_one_trailing_newline() {
        let c = fixture(vec![]);
        let bytes = c.serialize();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.ends_with('\n'));
        assert!(!s.ends_with("\n\n"));
    }

    #[test]
    fn author_with_spaces_in_name() {
        let a = Author {
            name: "Alice B. Cooper".into(),
            email: "alice@example.com".into(),
            timestamp: "2026-04-21T10:14:00Z".into(),
        };
        let s = a.format();
        let back = Author::parse(&s).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn encrypted_message_hides_body() {
        let c = fixture(vec![]);
        let key = [0x42u8; 32];
        let bytes = c.serialize_with_commit_key(Some(&key)).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Headers visible.
        assert!(s.contains("tree "));
        assert!(s.contains("author claude-code"));
        // Message body is not visible anywhere.
        assert!(!s.contains("Q3 earnings"));
        assert!(!s.contains("pdf@77c10e42"));
        // Roundtrip recovers the message.
        let back = Commit::parse_with_commit_key(&bytes, Some(&key)).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn encrypted_commit_is_byte_stable() {
        let c = fixture(vec![]);
        let key = [0x11u8; 32];
        let a = c.serialize_with_commit_key(Some(&key)).unwrap();
        let b = c.serialize_with_commit_key(Some(&key)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn encrypted_commit_rejects_missing_key() {
        let c = fixture(vec![]);
        let bytes = c.serialize_with_commit_key(Some(&[9u8; 32])).unwrap();
        let err = Commit::parse(&bytes).unwrap_err();
        assert!(matches!(err, OmpError::Unauthorized(_)));
    }

    #[test]
    fn encrypted_commit_rejects_wrong_key() {
        let c = fixture(vec![]);
        let bytes = c.serialize_with_commit_key(Some(&[1u8; 32])).unwrap();
        let err = Commit::parse_with_commit_key(&bytes, Some(&[2u8; 32])).unwrap_err();
        assert!(matches!(err, OmpError::Unauthorized(_)));
    }

    #[test]
    fn plaintext_commit_parses_without_key() {
        let c = fixture(vec![]);
        let bytes = c.serialize();
        let back = Commit::parse_with_commit_key(&bytes, None).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn hash_stability() {
        let c = fixture(vec![]);
        let bytes = c.serialize();
        // The expected wire form, byte-for-byte.
        let expected = format!(
            "tree {}\nauthor claude-code <claude@local> 2026-04-21T10:14:00Z\n\nadd Q3 earnings report\ningested under schema pdf@77c10e42\n",
            Hash::of(b"root").hex()
        );
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), expected);
    }
}

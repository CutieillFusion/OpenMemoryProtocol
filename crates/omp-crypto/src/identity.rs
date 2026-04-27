//! X25519 identity keys + age-style recipient stanzas.
//!
//! The spec-relevant primitive from `docs/design/13-end-to-end-encryption.md`
//! is **age's X25519 recipient stanza**: for each recipient, an ephemeral
//! X25519 keypair is generated, a shared secret is derived via ECDH, and the
//! content key is wrapped under a HKDF-derived AEAD key using that shared
//! secret as IKM.
//!
//! We implement a stanza-shaped wire format directly rather than pulling in
//! the full `age` crate, because (a) the dep graph of `age` is large and (b)
//! the doc says "age-style", not "age itself". This keeps OMP's crypto
//! surface minimal and self-contained.
//!
//! Wire format of the wrapped key (bytes):
//!
//! ```text
//! <alg:1B = 0x02> <ephemeral_pub:32B> <sealed_content_key:49B>
//! ```
//!
//! The 49-byte sealed payload is the output of `aead::seal` over the 32-byte
//! content key with a zero nonce (safe: the wrap key is fresh per-recipient
//! per-share because the ephemeral secret is fresh).

use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::aead::{self, NONCE_SIZE};
use crate::error::{CryptoError, Result};

/// Algorithm tag in the wrapped-key wire format: X25519 + ChaCha20-Poly1305.
pub const ALG_X25519_CHACHA20_POLY1305: u8 = 0x02;

/// Total length of a wrapped-key blob: 1 alg + 32 ephemeral_pub + sealed.
/// The sealed portion is always `1 + NONCE_SIZE + 32 + TAG_SIZE = 61`
/// bytes (aead framing wraps the 32-byte key).
pub const WRAPPED_KEY_LEN: usize = 1 + 32 + 1 + NONCE_SIZE + 32 + 16;

/// Opaque identity secret — kept client-side, persisted under tenant-root-
/// key wrap.
pub struct IdentityPrivate(pub [u8; 32]);

impl IdentityPrivate {
    pub fn public(&self) -> [u8; 32] {
        let sec = StaticSecret::from(self.0);
        PublicKey::from(&sec).to_bytes()
    }
}

/// Generate a fresh X25519 identity keypair. Returns `(private, public)`
/// as raw 32-byte arrays.
pub fn generate_identity() -> (IdentityPrivate, [u8; 32]) {
    let mut sk_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut sk_bytes);
    let sec = StaticSecret::from(sk_bytes);
    let pk = PublicKey::from(&sec).to_bytes();
    (IdentityPrivate(sk_bytes), pk)
}

/// Encrypt a 32-byte content key to a single X25519 recipient.
///
/// The returned blob is self-describing (includes the ephemeral public key
/// the recipient needs for ECDH); callers embed it in a `share` object.
pub fn wrap_to_recipient(content_key: &[u8; 32], recipient_pub: &[u8; 32]) -> Result<Vec<u8>> {
    let mut eph_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut eph_bytes);
    let eph_secret = StaticSecret::from(eph_bytes);
    let eph_pub = PublicKey::from(&eph_secret).to_bytes();

    let recipient = PublicKey::from(*recipient_pub);
    let shared = eph_secret.diffie_hellman(&recipient);

    // Derive a 32-byte wrap key from the shared secret. Mix the ephemeral
    // public into the info to bind the wrap to this exchange.
    let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());
    let mut info = Vec::with_capacity(32 + 32 + b"omp-share".len());
    info.extend_from_slice(b"omp-share");
    info.extend_from_slice(&eph_pub);
    info.extend_from_slice(recipient_pub);
    let mut wrap_key = [0u8; 32];
    hk.expand(&info, &mut wrap_key)
        .map_err(|e| CryptoError::Hkdf(e.to_string()))?;

    // Zero nonce: safe here because the wrap key is fresh per exchange
    // (derived from a fresh ephemeral secret).
    let zero_nonce = [0u8; NONCE_SIZE];
    let sealed = aead::seal(&wrap_key, &zero_nonce, b"omp-share-wrap", content_key)?;

    let mut out = Vec::with_capacity(1 + 32 + sealed.len());
    out.push(ALG_X25519_CHACHA20_POLY1305);
    out.extend_from_slice(&eph_pub);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Recover the content key from a wrapped-key blob using the recipient's
/// identity private key.
pub fn unwrap_from_stanza(wrapped: &[u8], recipient_priv: &IdentityPrivate) -> Result<[u8; 32]> {
    if wrapped.len() < 1 + 32 {
        return Err(CryptoError::Invalid(format!(
            "wrapped-key blob too short: {}",
            wrapped.len()
        )));
    }
    let alg = wrapped[0];
    if alg != ALG_X25519_CHACHA20_POLY1305 {
        return Err(CryptoError::Invalid(format!(
            "unknown wrap alg tag: 0x{alg:02x}"
        )));
    }
    let eph_pub_bytes: [u8; 32] = wrapped[1..33]
        .try_into()
        .map_err(|_| CryptoError::Invalid("truncated ephemeral pub".into()))?;
    let eph_pub = PublicKey::from(eph_pub_bytes);
    let sealed = &wrapped[33..];

    let secret = StaticSecret::from(recipient_priv.0);
    let shared = secret.diffie_hellman(&eph_pub);

    let recipient_pub = PublicKey::from(&secret).to_bytes();
    let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());
    let mut info = Vec::with_capacity(32 + 32 + b"omp-share".len());
    info.extend_from_slice(b"omp-share");
    info.extend_from_slice(&eph_pub_bytes);
    info.extend_from_slice(&recipient_pub);
    let mut wrap_key = [0u8; 32];
    hk.expand(&info, &mut wrap_key)
        .map_err(|e| CryptoError::Hkdf(e.to_string()))?;

    let plain = aead::open(&wrap_key, b"omp-share-wrap", sealed)?;
    let arr: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::Wrap(format!("unwrapped key wrong length: {}", plain.len())))?;
    Ok(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_roundtrip() {
        let (bob_sk, bob_pk) = generate_identity();
        let content_key = [0x77u8; 32];
        let wrapped = wrap_to_recipient(&content_key, &bob_pk).unwrap();
        let recovered = unwrap_from_stanza(&wrapped, &bob_sk).unwrap();
        assert_eq!(recovered, content_key);
    }

    #[test]
    fn wrong_recipient_cannot_unwrap() {
        let (_alice_sk, alice_pk) = generate_identity();
        let (bob_sk, _bob_pk) = generate_identity();
        let content_key = [0x55u8; 32];
        let wrapped = wrap_to_recipient(&content_key, &alice_pk).unwrap();
        let err = unwrap_from_stanza(&wrapped, &bob_sk).unwrap_err();
        assert!(matches!(err, CryptoError::Aead(_) | CryptoError::Wrap(_)));
    }

    #[test]
    fn two_wraps_of_same_key_differ() {
        // Ephemeral secret is fresh each time, so ciphertext differs even
        // when the plaintext + recipient are identical.
        let (_sk, pk) = generate_identity();
        let key = [0u8; 32];
        let w1 = wrap_to_recipient(&key, &pk).unwrap();
        let w2 = wrap_to_recipient(&key, &pk).unwrap();
        assert_ne!(w1, w2);
    }

    #[test]
    fn tamper_on_wrapped_fails_open() {
        let (sk, pk) = generate_identity();
        let mut w = wrap_to_recipient(&[9u8; 32], &pk).unwrap();
        let last = w.len() - 1;
        w[last] ^= 1;
        let err = unwrap_from_stanza(&w, &sk).unwrap_err();
        assert!(matches!(err, CryptoError::Aead(_)));
    }

    #[test]
    fn unknown_alg_tag_rejected() {
        let w = {
            let (_sk, pk) = generate_identity();
            let mut w = wrap_to_recipient(&[0u8; 32], &pk).unwrap();
            w[0] = 0xff;
            w
        };
        let (sk, _pk) = generate_identity();
        let err = unwrap_from_stanza(&w, &sk).unwrap_err();
        assert!(matches!(err, CryptoError::Invalid(_)));
    }
}

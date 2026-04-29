//! Signed tenant context — the auth artifact that travels between OMP services.
//!
//! See `docs/design/14-microservice-decomposition.md` § Inter-service auth.
//!
//! Wire format: `X-OMP-Tenant-Context: <base64(cbor(TenantContext))>`. CBOR
//! payload because OMP already speaks CBOR via the probe ABI; one parser.
//!
//! Workflow:
//! 1. Gateway issues a `SigningKey`-signed `TenantContext` per inbound request,
//!    naming the resolved tenant id and an `exp_unix` budget+30s in the future.
//! 2. Downstream services call `TenantContext::verify(&header, &verifying_key)`
//!    on every internal call. Expired or wrongly-signed contexts are rejected.
//!
//! Signing covers the canonical CBOR encoding of `(tenant_id, quotas_ref,
//! exp_unix)` only; the `signature` field is not part of the signed body.

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const HEADER_NAME: &str = "x-omp-tenant-context";

/// Default lifetime added on top of an estimated request budget. Short, so a
/// stolen header expires quickly even if mTLS is bypassed.
pub const DEFAULT_EXTRA_LIFETIME_SECS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TenantContext {
    pub tenant_id: String,
    /// Opaque pointer into the gateway's quota registry; downstream services
    /// don't interpret it, they just round-trip it back if they need it.
    #[serde(default, with = "serde_bytes")]
    pub quotas_ref: Vec<u8>,
    /// Unix epoch seconds at which this context expires.
    pub exp_unix: i64,
    /// Optional WorkOS user id (the `sub` from the session cookie). Populated
    /// when the request was authenticated via WorkOS; absent for Bearer
    /// machine clients. Downstream services that record publisher/actor
    /// identity (e.g., `omp-marketplace`) read this. Added in
    /// `docs/design/23-probe-marketplace.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    /// Ed25519 signature over canonical CBOR of `{tenant_id, quotas_ref,
    /// exp_unix, sub}` (i.e. the same struct without the `signature` field).
    /// Empty during construction; populated by `sign`.
    #[serde(default, with = "serde_bytes")]
    pub signature: Vec<u8>,
}

mod serde_bytes {
    use serde::{Deserializer, Serializer};

    #[allow(clippy::ptr_arg)] // serde `with` requires this exact signature.
    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, b: &[u8]) -> Result<Vec<u8>, E> {
                Ok(b.to_vec())
            }
            fn visit_byte_buf<E: serde::de::Error>(self, b: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(b)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::new();
                while let Some(b) = seq.next_element::<u8>()? {
                    out.push(b);
                }
                Ok(out)
            }
        }
        d.deserialize_bytes(V)
    }
}

#[derive(Debug, Error)]
pub enum CtxError {
    #[error("base64 decode: {0}")]
    Base64(String),
    #[error("cbor decode: {0}")]
    Cbor(String),
    #[error("cbor encode: {0}")]
    CborEncode(String),
    #[error("missing signature")]
    MissingSignature,
    #[error("bad signature")]
    BadSignature,
    #[error("expired (exp={0}, now={1})")]
    Expired(i64, i64),
    #[error("tenant id is empty")]
    EmptyTenant,
}

/// The signing material gateway services hold.
#[derive(Clone)]
pub struct GatewaySigner {
    key: SigningKey,
}

impl GatewaySigner {
    pub fn from_signing_key(key: SigningKey) -> Self {
        Self { key }
    }

    /// Generate a fresh keypair. Callers persist the signing key (never the
    /// signature) and distribute the verifying key to downstream services.
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        Self {
            key: SigningKey::generate(&mut rng),
        }
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Sign arbitrary bytes with the gateway's Ed25519 key. Used by the
    /// session-cookie codec (`omp-gateway::auth`) to reuse the same key as
    /// the `TenantContext` envelope without exposing the raw `SigningKey`.
    pub fn sign_bytes(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }

    /// Build a context for the named tenant valid until `exp_unix`. Signing
    /// happens here; the resulting context is ready to encode-and-send.
    pub fn issue(
        &self,
        tenant_id: &str,
        quotas_ref: Vec<u8>,
        exp_unix: i64,
    ) -> Result<TenantContext, CtxError> {
        self.issue_with_sub(tenant_id, quotas_ref, exp_unix, None)
    }

    /// Like `issue`, with an optional WorkOS user id baked into the signed
    /// envelope. See `docs/design/23-probe-marketplace.md` for why
    /// downstream services need this.
    pub fn issue_with_sub(
        &self,
        tenant_id: &str,
        quotas_ref: Vec<u8>,
        exp_unix: i64,
        sub: Option<String>,
    ) -> Result<TenantContext, CtxError> {
        if tenant_id.is_empty() {
            return Err(CtxError::EmptyTenant);
        }
        let mut ctx = TenantContext {
            tenant_id: tenant_id.to_string(),
            quotas_ref,
            exp_unix,
            sub,
            signature: Vec::new(),
        };
        let canonical = canonical_signed_bytes(&ctx)?;
        let sig = self.key.sign(&canonical);
        ctx.signature = sig.to_bytes().to_vec();
        Ok(ctx)
    }

    /// Convenience: issue with a default 30-second extra lifetime added to
    /// the current time. Use only when you don't know the request budget.
    pub fn issue_default(
        &self,
        tenant_id: &str,
        quotas_ref: Vec<u8>,
    ) -> Result<TenantContext, CtxError> {
        let now = current_unix();
        self.issue(tenant_id, quotas_ref, now + DEFAULT_EXTRA_LIFETIME_SECS)
    }

    pub fn issue_default_with_sub(
        &self,
        tenant_id: &str,
        quotas_ref: Vec<u8>,
        sub: Option<String>,
    ) -> Result<TenantContext, CtxError> {
        let now = current_unix();
        self.issue_with_sub(tenant_id, quotas_ref, now + DEFAULT_EXTRA_LIFETIME_SECS, sub)
    }
}

impl TenantContext {
    /// Encode to the wire string used in `X-OMP-Tenant-Context`.
    pub fn encode(&self) -> Result<String, CtxError> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(self, &mut buf)
            .map_err(|e| CtxError::CborEncode(e.to_string()))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(&buf))
    }

    /// Decode from the wire string without verifying the signature. Useful
    /// for logging; production code paths should call `verify` instead.
    pub fn decode_unverified(s: &str) -> Result<Self, CtxError> {
        let buf = base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(|e| CtxError::Base64(e.to_string()))?;
        let ctx: TenantContext =
            ciborium::de::from_reader(&buf[..]).map_err(|e| CtxError::Cbor(e.to_string()))?;
        Ok(ctx)
    }

    /// Decode + verify the signature + check expiry against `now_unix`.
    pub fn verify_at(s: &str, verifier: &VerifyingKey, now_unix: i64) -> Result<Self, CtxError> {
        let ctx = Self::decode_unverified(s)?;
        if ctx.signature.is_empty() {
            return Err(CtxError::MissingSignature);
        }
        if ctx.exp_unix < now_unix {
            return Err(CtxError::Expired(ctx.exp_unix, now_unix));
        }
        let signed = canonical_signed_bytes(&ctx)?;
        let sig = Signature::from_slice(&ctx.signature).map_err(|_| CtxError::BadSignature)?;
        verifier
            .verify(&signed, &sig)
            .map_err(|_| CtxError::BadSignature)?;
        Ok(ctx)
    }

    /// Convenience for current wall-clock time.
    pub fn verify(s: &str, verifier: &VerifyingKey) -> Result<Self, CtxError> {
        Self::verify_at(s, verifier, current_unix())
    }
}

fn canonical_signed_bytes(ctx: &TenantContext) -> Result<Vec<u8>, CtxError> {
    // The signed body is the same struct minus the signature. Serializing a
    // companion `SignedBody` keeps the wire shape stable across CBOR libraries.
    //
    // `sub` is `skip_serializing_if = "Option::is_none"` so existing
    // contexts (issued before doc 23) hash identically — the new field is
    // additive and back-compatible. Downstream verifiers need to be on the
    // same crate version, but old contexts on the wire still verify.
    #[derive(Serialize)]
    struct SignedBody<'a> {
        tenant_id: &'a str,
        #[serde(with = "super_serde_bytes")]
        quotas_ref: &'a [u8],
        exp_unix: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        sub: Option<&'a str>,
    }
    let body = SignedBody {
        tenant_id: &ctx.tenant_id,
        quotas_ref: &ctx.quotas_ref,
        exp_unix: ctx.exp_unix,
        sub: ctx.sub.as_deref(),
    };
    let mut out = Vec::new();
    ciborium::ser::into_writer(&body, &mut out).map_err(|e| CtxError::CborEncode(e.to_string()))?;
    Ok(out)
}

mod super_serde_bytes {
    use serde::Serializer;
    pub fn serialize<S: Serializer>(v: &&[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }
}

fn current_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_signed() {
        let signer = GatewaySigner::generate();
        let vk = signer.verifying_key();
        let ctx = signer
            .issue("alice", vec![1, 2, 3], current_unix() + 60)
            .expect("issue");
        let wire = ctx.encode().expect("encode");
        let verified = TenantContext::verify(&wire, &vk).expect("verify");
        assert_eq!(verified.tenant_id, "alice");
        assert_eq!(verified.quotas_ref, vec![1, 2, 3]);
    }

    #[test]
    fn rejects_expired() {
        let signer = GatewaySigner::generate();
        let vk = signer.verifying_key();
        let ctx = signer
            .issue("alice", vec![], current_unix() - 1)
            .expect("issue");
        let wire = ctx.encode().expect("encode");
        let err = TenantContext::verify(&wire, &vk).unwrap_err();
        assert!(matches!(err, CtxError::Expired(..)), "got {err:?}");
    }

    #[test]
    fn rejects_tampering() {
        let signer = GatewaySigner::generate();
        let vk = signer.verifying_key();
        let mut ctx = signer
            .issue("alice", vec![], current_unix() + 60)
            .expect("issue");
        // Flip the tenant id without re-signing.
        ctx.tenant_id = "bob".into();
        let wire = ctx.encode().expect("encode");
        let err = TenantContext::verify(&wire, &vk).unwrap_err();
        assert!(matches!(err, CtxError::BadSignature), "got {err:?}");
    }

    #[test]
    fn rejects_wrong_key() {
        let issuer = GatewaySigner::generate();
        let other = GatewaySigner::generate();
        let ctx = issuer
            .issue("alice", vec![], current_unix() + 60)
            .expect("issue");
        let wire = ctx.encode().expect("encode");
        let err = TenantContext::verify(&wire, &other.verifying_key()).unwrap_err();
        assert!(matches!(err, CtxError::BadSignature), "got {err:?}");
    }

    #[test]
    fn sub_field_round_trips_in_signed_envelope() {
        let signer = GatewaySigner::generate();
        let vk = signer.verifying_key();
        let ctx = signer
            .issue_with_sub(
                "acme",
                vec![],
                current_unix() + 60,
                Some("user_01ABC".into()),
            )
            .expect("issue");
        let wire = ctx.encode().expect("encode");
        let verified = TenantContext::verify(&wire, &vk).expect("verify");
        assert_eq!(verified.sub.as_deref(), Some("user_01ABC"));
    }

    #[test]
    fn sub_absence_is_signed_distinctly_from_empty_string() {
        // A context issued with sub=None must not verify if someone
        // tampers it to sub=Some(""). This guards against the
        // skip_serializing_if optimization being a soundness bug.
        let signer = GatewaySigner::generate();
        let vk = signer.verifying_key();
        let mut ctx = signer
            .issue("alice", vec![], current_unix() + 60)
            .expect("issue");
        ctx.sub = Some(String::new());
        let wire = ctx.encode().expect("encode");
        let err = TenantContext::verify(&wire, &vk).unwrap_err();
        assert!(matches!(err, CtxError::BadSignature), "got {err:?}");
    }

    #[test]
    fn rejects_missing_signature() {
        let mut ctx = TenantContext {
            tenant_id: "alice".into(),
            quotas_ref: vec![],
            exp_unix: current_unix() + 60,
            sub: None,
            signature: vec![],
        };
        ctx.signature.clear();
        let wire = ctx.encode().expect("encode");
        let signer = GatewaySigner::generate();
        let err = TenantContext::verify(&wire, &signer.verifying_key()).unwrap_err();
        assert!(matches!(err, CtxError::MissingSignature), "got {err:?}");
    }
}

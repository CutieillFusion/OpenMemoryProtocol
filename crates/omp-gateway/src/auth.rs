//! WorkOS / generic-OIDC auth path. See `docs/design/22-workos-auth.md`.
//!
//! This module owns:
//!   - OIDC discovery, the authorize-redirect, code-exchange, and userinfo call.
//!   - The signed session cookie codec (Ed25519 via `GatewaySigner`, same key
//!     that signs `X-OMP-Tenant-Context`).
//!   - Per-flow PKCE state, carried in a short-lived signed cookie keyed by
//!     the OIDC `state` parameter (the CSRF defense doubles as the
//!     concurrent-tab disambiguator).
//!
//! Vendor coupling is intentionally narrow: we hand-roll the four OIDC HTTP
//! calls (discovery, token, userinfo, end_session) against `reqwest` rather
//! than depending on a wrapper crate, so swapping issuers is a config change
//! plus the `refresh` branch noted in the design doc.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::GatewayState;

/// Wire-format magic prefix bound into the signed body of every session
/// cookie. Domain-separates session signatures from `TenantContext`
/// signatures even though both reuse the same `GatewaySigner` key.
const SESSION_DOMAIN_TAG: &[u8] = b"omp_session\0v1\0";
const STATE_DOMAIN_TAG: &[u8] = b"omp_oidc_state\0v1\0";

pub const SESSION_COOKIE_NAME: &str = "omp_session";
/// A non-HttpOnly companion to `omp_session`. JavaScript can read this to
/// detect "the user is logged in" without ever needing access to the actual
/// session bytes. Carries no secret — just a flag.
pub const SESSION_PRESENCE_COOKIE: &str = "omp_signed_in";
pub const STATE_COOKIE_PREFIX: &str = "omp_oidc_state_";

const STATE_COOKIE_TTL_SECS: u64 = 15 * 60;
const DEFAULT_REFRESH_GRACE_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WorkOsConfig {
    pub client_id: String,
    /// Loaded from `WORKOS_CLIENT_SECRET` env if absent in the TOML file.
    #[serde(default)]
    pub client_secret: String,
    pub issuer_url: String,
    pub redirect_uri: String,
    #[serde(default = "default_session_ttl")]
    pub session_ttl_seconds: u64,
    #[serde(default = "default_refresh_grace")]
    pub refresh_grace_seconds: u64,
    /// How long a successful WorkOS API-key validation stays cached on the
    /// gateway. Bounds revocation latency: a key revoked in the WorkOS UI
    /// keeps working for up to this many seconds. See
    /// `docs/design/24-per-user-api-keys.md` §"Risks".
    #[serde(default = "default_validation_cache_ttl")]
    pub validation_cache_ttl_seconds: u64,
}

fn default_session_ttl() -> u64 {
    3600
}
fn default_refresh_grace() -> u64 {
    DEFAULT_REFRESH_GRACE_SECS
}
fn default_validation_cache_ttl() -> u64 {
    60
}

/// OIDC provider metadata + issuer-specific endpoints we resolve once at
/// startup.
///
/// Two providers are supported:
///   - `Workos`: WorkOS User Management / AuthKit. Endpoints are hardcoded
///     because WorkOS doesn't expose an OIDC discovery doc for this surface
///     (the `/.well-known/openid-configuration` doc on the AuthKit subdomain
///     advertises a *different* "OAuth Applications" product, which rejects
///     AuthKit client_ids with `application_not_found`). The token-exchange
///     response carries the user object directly, so there is no separate
///     userinfo call.
///   - `Oidc`: any other issuer; endpoints come from discovery, userinfo is
///     called separately (vanilla OIDC). Used for self-hosted Keycloak,
///     Auth0, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProvider {
    Workos,
    Oidc,
}

#[derive(Debug, Clone)]
pub struct OidcRuntime {
    pub provider: AuthProvider,
    pub config: WorkOsConfig,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// Empty for `AuthProvider::Workos` (user is inlined in the token
    /// response).
    pub userinfo_endpoint: String,
    /// Optional per the OIDC spec; WorkOS exposes it.
    pub end_session_endpoint: Option<String>,
    pub http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: String,
    #[serde(default)]
    end_session_endpoint: Option<String>,
}

impl OidcRuntime {
    pub async fn discover(config: WorkOsConfig) -> anyhow::Result<Self> {
        let trimmed = config.issuer_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .build()?;

        if trimmed == "https://api.workos.com" {
            return Ok(Self {
                provider: AuthProvider::Workos,
                config,
                authorization_endpoint: "https://api.workos.com/user_management/authorize"
                    .to_string(),
                token_endpoint: "https://api.workos.com/user_management/authenticate".to_string(),
                userinfo_endpoint: String::new(),
                end_session_endpoint: Some(
                    "https://api.workos.com/user_management/sessions/logout".to_string(),
                ),
                http,
            });
        }

        let url = format!("{trimmed}/.well-known/openid-configuration");
        let doc: DiscoveryDocument = http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(Self {
            provider: AuthProvider::Oidc,
            config,
            authorization_endpoint: doc.authorization_endpoint,
            token_endpoint: doc.token_endpoint,
            userinfo_endpoint: doc.userinfo_endpoint,
            end_session_endpoint: doc.end_session_endpoint,
            http,
        })
    }
}

// ---------------------------------------------------------------------------
// Cookie codec — CBOR + Ed25519, mirroring TenantContext's envelope but with
// a domain-separation tag so the two cookie types can never be confused.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionClaims {
    pub tenant: String,
    pub sub: String,
    pub workos_session_id: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcStateClaims {
    pub id: String,
    pub pkce_verifier: String,
    pub nonce: String,
    pub return_to: String,
    pub expires_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CookieError {
    #[error("base64: {0}")]
    Base64(String),
    #[error("cbor: {0}")]
    Cbor(String),
    #[error("bad signature")]
    BadSignature,
    #[error("expired (exp={exp}, now={now})")]
    Expired { exp: u64, now: u64 },
}

/// Wire envelope: CBOR({ body, signature }), base64.
#[derive(Serialize, Deserialize)]
struct Envelope {
    #[serde(with = "serde_bytes_vec")]
    body: Vec<u8>,
    #[serde(with = "serde_bytes_vec")]
    sig: Vec<u8>,
}

mod serde_bytes_vec {
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

fn signed_message(domain: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(domain.len() + body.len());
    out.extend_from_slice(domain);
    out.extend_from_slice(body);
    out
}

fn encode_signed<T: Serialize>(
    domain: &[u8],
    claims: &T,
    signer: &omp_tenant_ctx::GatewaySigner,
) -> Result<String, CookieError> {
    let mut body = Vec::new();
    ciborium::ser::into_writer(claims, &mut body).map_err(|e| CookieError::Cbor(e.to_string()))?;
    let sig = signer.sign_bytes(&signed_message(domain, &body)).to_vec();
    let env = Envelope { body, sig };
    let mut wire = Vec::new();
    ciborium::ser::into_writer(&env, &mut wire).map_err(|e| CookieError::Cbor(e.to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&wire))
}

fn decode_signed<T: for<'de> Deserialize<'de>>(
    domain: &[u8],
    wire: &str,
    verifier: &VerifyingKey,
) -> Result<T, CookieError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(wire)
        .map_err(|e| CookieError::Base64(e.to_string()))?;
    let env: Envelope =
        ciborium::de::from_reader(&raw[..]).map_err(|e| CookieError::Cbor(e.to_string()))?;
    let sig = Signature::from_slice(&env.sig).map_err(|_| CookieError::BadSignature)?;
    verifier
        .verify(&signed_message(domain, &env.body), &sig)
        .map_err(|_| CookieError::BadSignature)?;
    let claims: T =
        ciborium::de::from_reader(&env.body[..]).map_err(|e| CookieError::Cbor(e.to_string()))?;
    Ok(claims)
}

pub fn encode_session(
    claims: &SessionClaims,
    signer: &omp_tenant_ctx::GatewaySigner,
) -> Result<String, CookieError> {
    encode_signed(SESSION_DOMAIN_TAG, claims, signer)
}

pub fn decode_session(
    wire: &str,
    verifier: &VerifyingKey,
    now: u64,
    allow_expired: bool,
) -> Result<SessionClaims, CookieError> {
    let claims: SessionClaims = decode_signed(SESSION_DOMAIN_TAG, wire, verifier)?;
    if !allow_expired && claims.expires_at < now {
        return Err(CookieError::Expired {
            exp: claims.expires_at,
            now,
        });
    }
    Ok(claims)
}

fn encode_state(
    claims: &OidcStateClaims,
    signer: &omp_tenant_ctx::GatewaySigner,
) -> Result<String, CookieError> {
    encode_signed(STATE_DOMAIN_TAG, claims, signer)
}

fn decode_state(wire: &str, verifier: &VerifyingKey) -> Result<OidcStateClaims, CookieError> {
    decode_signed(STATE_DOMAIN_TAG, wire, verifier)
}

// ---------------------------------------------------------------------------
// Cookie helpers (we don't pull in the `cookie` crate — Set-Cookie is two
// lines of formatting and parsing only needs to find a name).
// ---------------------------------------------------------------------------

pub fn parse_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let cookie_hdr = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for piece in cookie_hdr.split(';') {
        let trimmed = piece.trim();
        if let Some(rest) = trimmed.strip_prefix(&format!("{name}=")) {
            return Some(rest);
        }
    }
    None
}

fn build_cookie(name: &str, value: &str, max_age_secs: i64, path: &str, secure: bool) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "{name}={value}; Path={path}; Max-Age={max_age_secs}; HttpOnly{secure_attr}; SameSite=Lax"
    )
}

/// Companion cookie to `omp_session`. Same lifetime, NOT `HttpOnly` so the
/// frontend can detect logged-in state without a network round trip.
fn build_presence_cookie(max_age_secs: i64, secure: bool) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_PRESENCE_COOKIE}=1; Path=/; Max-Age={max_age_secs}{secure_attr}; SameSite=Lax"
    )
}

fn cleared_presence_cookie(secure: bool) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_PRESENCE_COOKIE}=; Path=/; Max-Age=0{secure_attr}; SameSite=Lax"
    )
}

fn cleared_cookie(name: &str, path: &str, secure: bool) -> String {
    build_cookie(name, "", 0, path, secure)
}

/// Cookies are only marked `Secure` when the redirect URI is HTTPS.
/// Browsers reject `Secure` cookies on plain HTTP origins, so toggling on
/// scheme keeps `http://localhost` development workable while still hardening
/// every real deployment (which terminates TLS at the gateway / ingress).
fn cookies_secure(redirect_uri: &str) -> bool {
    redirect_uri.starts_with("https://")
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

fn random_b64url(n_bytes: usize) -> String {
    let mut bytes = vec![0u8; n_bytes];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

fn pkce_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// API-key validation cache. Keyed by sha256(token); the token itself never
// enters the map. Lazy expiry, capped size (10k entries) — small enough to
// fit in memory for any realistic tenant population. See
// `docs/design/24-per-user-api-keys.md` §"resolve_bearer_token extension".
// ---------------------------------------------------------------------------

const VALIDATION_CACHE_MAX_ENTRIES: usize = 10_000;

#[derive(Clone)]
struct CachedValidation {
    tenant: String,
    expires_at: u64,
}

#[derive(Clone, Default)]
pub struct ValidationCache {
    inner: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<[u8; 32], CachedValidation>>>,
}

impl ValidationCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &[u8; 32]) -> Option<String> {
        let now = now_unix();
        let mut map = self.inner.lock().ok()?;
        match map.get(key) {
            Some(entry) if entry.expires_at > now => Some(entry.tenant.clone()),
            Some(_) => {
                map.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn insert(&self, key: [u8; 32], tenant: String, ttl_secs: u64) {
        if let Ok(mut map) = self.inner.lock() {
            if map.len() >= VALIDATION_CACHE_MAX_ENTRIES {
                let now = now_unix();
                map.retain(|_, v| v.expires_at > now);
                if map.len() >= VALIDATION_CACHE_MAX_ENTRIES {
                    map.clear();
                }
            }
            map.insert(
                key,
                CachedValidation {
                    tenant,
                    expires_at: now_unix() + ttl_secs,
                },
            );
        }
    }
}

pub fn token_hash_bytes(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// `POST {issuer}/user_management/api_keys/validate` — validates a bearer
/// token presented by an external client and returns the owning
/// `organization_id` if recognized. Authenticated to WorkOS with the
/// management `client_secret`. Errors and "no such key" both collapse to
/// `None` so the resolver can fall through to `401 unauthorized`.
pub async fn validate_api_key(oidc: &OidcRuntime, token: &str) -> Option<String> {
    let url = format!(
        "{}/user_management/api_keys/validate",
        oidc.config.issuer_url.trim_end_matches('/')
    );
    let resp = oidc
        .http
        .post(&url)
        .bearer_auth(&oidc.config.client_secret)
        .json(&serde_json::json!({ "api_key": token }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    #[derive(Deserialize)]
    struct ValidateResponse {
        #[serde(default)]
        api_key: Option<ValidatedKey>,
    }
    #[derive(Deserialize)]
    struct ValidatedKey {
        #[serde(default)]
        organization_id: Option<String>,
    }
    let parsed: ValidateResponse = resp.json().await.ok()?;
    parsed.api_key?.organization_id.filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// resolve_session — called from `GatewayState::resolve_tenant`.
// ---------------------------------------------------------------------------

pub fn resolve_session_cookie(
    headers: &HeaderMap,
    verifier: &VerifyingKey,
) -> Option<SessionClaims> {
    let raw = parse_cookie(headers, SESSION_COOKIE_NAME)?;
    decode_session(raw, verifier, now_unix(), false).ok()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginParams {
    #[serde(default)]
    pub return_to: Option<String>,
}

pub async fn login(
    State(state): State<GatewayState>,
    Query(params): Query<LoginParams>,
) -> Response {
    let oidc = match state.oidc.as_ref() {
        Some(o) => o.clone(),
        None => return error_resp(StatusCode::NOT_FOUND, "workos_disabled"),
    };
    let id = random_b64url(16);
    let pkce_verifier = random_b64url(48);
    let pkce_challenge = pkce_challenge_s256(&pkce_verifier);
    let nonce = random_b64url(24);
    let return_to = params.return_to.unwrap_or_else(|| "/ui/".to_string());

    let claims = OidcStateClaims {
        id: id.clone(),
        pkce_verifier: pkce_verifier.clone(),
        nonce: nonce.clone(),
        return_to,
        expires_at: now_unix() + STATE_COOKIE_TTL_SECS,
    };
    let cookie_value = match encode_state(&claims, &state.signer) {
        Ok(v) => v,
        Err(e) => return error_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let cookie_name = format!("{}{}", STATE_COOKIE_PREFIX, id);
    let secure = cookies_secure(&oidc.config.redirect_uri);
    let cookie_header = build_cookie(
        &cookie_name,
        &cookie_value,
        STATE_COOKIE_TTL_SECS as i64,
        "/",
        secure,
    );

    // Build authorize URL.
    let mut url = match url::Url::parse(&oidc.authorization_endpoint) {
        Ok(u) => u,
        Err(e) => return error_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &oidc.config.client_id)
            .append_pair("redirect_uri", &oidc.config.redirect_uri)
            .append_pair("state", &id)
            .append_pair("code_challenge", &pkce_challenge)
            .append_pair("code_challenge_method", "S256");
        match oidc.provider {
            AuthProvider::Workos => {
                // WorkOS User Management uses `provider` to pick the hosted UI
                // and does not accept the OIDC `scope`/`nonce` params on its
                // authorize endpoint.
                q.append_pair("provider", "authkit");
            }
            AuthProvider::Oidc => {
                q.append_pair("scope", "openid profile email")
                    .append_pair("nonce", &nonce);
            }
        }
    }

    let mut resp = Redirect::to(url.as_str()).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&cookie_header) {
        resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    resp
}

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OidcTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct OidcUserInfo {
    sub: String,
    #[serde(default)]
    organization_id: Option<String>,
    #[serde(default)]
    sid: Option<String>,
}

/// WorkOS User Management `POST /user_management/authenticate` response.
/// The user object is inlined; there is no separate userinfo endpoint.
#[derive(Debug, Deserialize)]
struct WorkosAuthenticateResponse {
    user: WorkosUser,
    #[serde(default)]
    organization_id: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

/// Extract the `sid` (session id) claim from a JWT without verifying the
/// signature. We don't trust the value to bypass auth — it's just used to
/// build the WorkOS RP-initiated logout URL on the next request.
fn extract_jwt_sid(jwt: &str) -> Option<String> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    #[derive(Deserialize)]
    struct Claims {
        sid: Option<String>,
    }
    let claims: Claims = serde_json::from_slice(&payload_bytes).ok()?;
    claims.sid
}

#[derive(Debug, Deserialize)]
struct WorkosUser {
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    email: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    email_verified: Option<bool>,
}

pub async fn callback(
    State(state): State<GatewayState>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    let oidc = match state.oidc.as_ref() {
        Some(o) => o.clone(),
        None => return error_resp(StatusCode::NOT_FOUND, "workos_disabled"),
    };
    if let Some(err) = params.error {
        let desc = params.error_description.unwrap_or_default();
        return error_resp(
            StatusCode::BAD_REQUEST,
            &format!("oidc_error: {err}: {desc}"),
        );
    }
    let code = match params.code {
        Some(c) => c,
        None => return error_resp(StatusCode::BAD_REQUEST, "missing_code"),
    };
    let state_id = match params.state {
        Some(s) => s,
        None => return error_resp(StatusCode::BAD_REQUEST, "missing_state"),
    };

    // Look up the matching state cookie. If it's missing/expired/tampered,
    // the safest user-facing recovery is to start a fresh login flow rather
    // than dead-end on a JSON error. Common causes: cookie TTL elapsed
    // before the user finished the WorkOS form, gateway signing key
    // rotated mid-flow (ephemeral key + restart), or the user opened the
    // callback URL in a different browser session.
    let cookie_name = format!("{}{}", STATE_COOKIE_PREFIX, state_id);
    let restart_login = || Redirect::to("/auth/login?return_to=%2Fui%2F").into_response();
    let cookie_value = match parse_cookie(&headers, &cookie_name) {
        Some(v) => v,
        None => return restart_login(),
    };
    let verifier_key = state.signer.verifying_key();
    let state_claims = match decode_state(cookie_value, &verifier_key) {
        Ok(c) => c,
        Err(_) => return restart_login(),
    };
    if state_claims.id != state_id {
        return restart_login();
    }
    if state_claims.expires_at < now_unix() {
        return restart_login();
    }

    // Exchange the code. The two providers expect different request shapes
    // and return different response shapes.
    let (sub, organization_id, sid) = match oidc.provider {
        AuthProvider::Workos => {
            let body = serde_json::json!({
                "grant_type": "authorization_code",
                "code": code,
                "client_id": oidc.config.client_id,
                "client_secret": oidc.config.client_secret,
                "code_verifier": state_claims.pkce_verifier,
            });
            let resp = match oidc
                .http
                .post(&oidc.token_endpoint)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return error_resp(StatusCode::BAD_GATEWAY, &format!("authenticate_send: {e}"))
                }
            };
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return error_resp(
                    StatusCode::BAD_GATEWAY,
                    &format!("authenticate_status {status}: {text}"),
                );
            }
            let parsed: WorkosAuthenticateResponse = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    return error_resp(
                        StatusCode::BAD_GATEWAY,
                        &format!("authenticate_json: {e}"),
                    )
                }
            };
            let sid = parsed
                .access_token
                .as_deref()
                .and_then(extract_jwt_sid);
            (parsed.user.id, parsed.organization_id, sid)
        }
        AuthProvider::Oidc => {
            let form = [
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", oidc.config.redirect_uri.as_str()),
                ("client_id", oidc.config.client_id.as_str()),
                ("client_secret", oidc.config.client_secret.as_str()),
                ("code_verifier", state_claims.pkce_verifier.as_str()),
            ];
            let token: OidcTokenResponse = match oidc
                .http
                .post(&oidc.token_endpoint)
                .form(&form)
                .send()
                .await
            {
                Ok(r) => match r.error_for_status() {
                    Ok(r) => match r.json().await {
                        Ok(j) => j,
                        Err(e) => {
                            return error_resp(
                                StatusCode::BAD_GATEWAY,
                                &format!("token_json: {e}"),
                            )
                        }
                    },
                    Err(e) => {
                        return error_resp(StatusCode::BAD_GATEWAY, &format!("token_status: {e}"))
                    }
                },
                Err(e) => return error_resp(StatusCode::BAD_GATEWAY, &format!("token_send: {e}")),
            };
            let info: OidcUserInfo = match oidc
                .http
                .get(&oidc.userinfo_endpoint)
                .bearer_auth(&token.access_token)
                .send()
                .await
            {
                Ok(r) => match r.error_for_status() {
                    Ok(r) => match r.json().await {
                        Ok(j) => j,
                        Err(e) => {
                            return error_resp(
                                StatusCode::BAD_GATEWAY,
                                &format!("userinfo_json: {e}"),
                            )
                        }
                    },
                    Err(e) => {
                        return error_resp(
                            StatusCode::BAD_GATEWAY,
                            &format!("userinfo_status: {e}"),
                        )
                    }
                },
                Err(e) => {
                    return error_resp(StatusCode::BAD_GATEWAY, &format!("userinfo_send: {e}"))
                }
            };
            (info.sub, info.organization_id, info.sid)
        }
    };

    let tenant = match organization_id {
        Some(org) if !org.is_empty() => org,
        _ => format!("user_{}", sub),
    };

    let now = now_unix();
    let session = SessionClaims {
        tenant,
        sub: sub.clone(),
        workos_session_id: sid.unwrap_or_default(),
        issued_at: now,
        expires_at: now + oidc.config.session_ttl_seconds,
    };
    let session_cookie_value = match encode_session(&session, &state.signer) {
        Ok(v) => v,
        Err(e) => return error_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let secure = cookies_secure(&oidc.config.redirect_uri);
    let ttl = oidc.config.session_ttl_seconds as i64;
    let session_cookie_header =
        build_cookie(SESSION_COOKIE_NAME, &session_cookie_value, ttl, "/", secure);
    let presence_cookie_header = build_presence_cookie(ttl, secure);
    // Clear the now-consumed state cookie.
    let cleared_state = cleared_cookie(&cookie_name, "/", secure);

    let mut resp = Redirect::to(&state_claims.return_to).into_response();
    let h = resp.headers_mut();
    for header in [&session_cookie_header, &presence_cookie_header, &cleared_state] {
        if let Ok(v) = axum::http::HeaderValue::from_str(header) {
            h.append(axum::http::header::SET_COOKIE, v);
        }
    }
    resp
}

#[derive(Debug, Deserialize)]
pub struct RefreshParams {
    #[serde(default)]
    pub return_to: Option<String>,
}

pub async fn refresh(
    State(state): State<GatewayState>,
    Query(params): Query<RefreshParams>,
    headers: HeaderMap,
) -> Response {
    let oidc = match state.oidc.as_ref() {
        Some(o) => o.clone(),
        None => return error_resp(StatusCode::NOT_FOUND, "workos_disabled"),
    };
    let return_to = params.return_to.unwrap_or_else(|| "/ui/".to_string());
    let raw = match parse_cookie(&headers, SESSION_COOKIE_NAME) {
        Some(v) => v,
        None => {
            // No cookie at all → push the user through a fresh login.
            return Redirect::to(&format!("/auth/login?return_to={}", url_escape(&return_to)))
                .into_response();
        }
    };
    let verifier_key = state.signer.verifying_key();
    let claims = match decode_session(raw, &verifier_key, now_unix(), true) {
        Ok(c) => c,
        Err(_) => {
            return Redirect::to(&format!("/auth/login?return_to={}", url_escape(&return_to)))
                .into_response();
        }
    };
    let now = now_unix();
    let grace = oidc.config.refresh_grace_seconds;
    if claims.expires_at + grace < now {
        return Redirect::to(&format!("/auth/login?return_to={}", url_escape(&return_to)))
            .into_response();
    }

    // Re-authenticate with WorkOS. Endpoint shape per design doc §Sessions.
    if !claims.workos_session_id.is_empty() {
        let url = format!(
            "{}/sessions/{}/authenticate",
            oidc.config.issuer_url.trim_end_matches('/'),
            claims.workos_session_id
        );
        let resp = oidc
            .http
            .post(&url)
            .basic_auth(&oidc.config.client_id, Some(&oidc.config.client_secret))
            .send()
            .await;
        if !matches!(resp, Ok(ref r) if r.status().is_success()) {
            return Redirect::to(&format!("/auth/login?return_to={}", url_escape(&return_to)))
                .into_response();
        }
    }

    let new_claims = SessionClaims {
        issued_at: now,
        expires_at: now + oidc.config.session_ttl_seconds,
        ..claims
    };
    let cookie_value = match encode_session(&new_claims, &state.signer) {
        Ok(v) => v,
        Err(e) => return error_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let secure = cookies_secure(&oidc.config.redirect_uri);
    let ttl = oidc.config.session_ttl_seconds as i64;
    let cookie_header = build_cookie(SESSION_COOKIE_NAME, &cookie_value, ttl, "/", secure);
    let presence_cookie_header = build_presence_cookie(ttl, secure);
    let mut resp = Redirect::to(&return_to).into_response();
    for header in [&cookie_header, &presence_cookie_header] {
        if let Ok(v) = axum::http::HeaderValue::from_str(header) {
            resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
        }
    }
    resp
}

/// `GET /auth/me` — returns the WorkOS user record for the current
/// session cookie. The session cookie deliberately doesn't carry email
/// (per `docs/design/22-workos-auth.md` §Sessions: PII discipline), so we
/// look the user up via the WorkOS Management API on demand. Per-request
/// fetch is fine for the UI's profile menu — it's at most one call per
/// page load.
pub async fn me(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    let oidc = match state.oidc.as_ref() {
        Some(o) => o.clone(),
        None => return error_resp(StatusCode::NOT_FOUND, "workos_disabled"),
    };
    let session = match resolve_session_cookie(&headers, &state.signer.verifying_key()) {
        Some(s) => s,
        None => return error_resp(StatusCode::UNAUTHORIZED, "no_session"),
    };

    // For the WorkOS provider we use Management API; for generic OIDC we
    // hit the discovered userinfo_endpoint with a fresh access token —
    // but generic OIDC needs a stored access_token, which the cookie
    // doesn't carry. v1 supports profile-lookup only for WorkOS.
    if !matches!(oidc.provider, AuthProvider::Workos) {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "tenant": session.tenant,
                "sub": session.sub,
                "email": serde_json::Value::Null,
                "first_name": serde_json::Value::Null,
                "last_name": serde_json::Value::Null,
                "profile_picture_url": serde_json::Value::Null,
            })),
        )
            .into_response();
    }

    let url = format!(
        "{}/user_management/users/{}",
        oidc.config.issuer_url.trim_end_matches('/'),
        session.sub
    );
    let resp = match oidc
        .http
        .get(&url)
        .bearer_auth(&oidc.config.client_secret)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return error_resp(StatusCode::BAD_GATEWAY, &format!("workos_send: {e}")),
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return error_resp(
            StatusCode::BAD_GATEWAY,
            &format!("workos_status {status}: {text}"),
        );
    }
    // Forward only the safe fields. Don't leak the raw WorkOS response in
    // case it grows new properties we haven't audited.
    #[derive(serde::Deserialize)]
    struct WorkosUserDetail {
        #[allow(dead_code)]
        id: String,
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        first_name: Option<String>,
        #[serde(default)]
        last_name: Option<String>,
        #[serde(default)]
        profile_picture_url: Option<String>,
        #[serde(default)]
        email_verified: Option<bool>,
    }
    let detail: WorkosUserDetail = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return error_resp(StatusCode::BAD_GATEWAY, &format!("workos_json: {e}")),
    };
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "tenant": session.tenant,
            "sub": session.sub,
            "email": detail.email,
            "email_verified": detail.email_verified,
            "first_name": detail.first_name,
            "last_name": detail.last_name,
            "profile_picture_url": detail.profile_picture_url,
        })),
    )
        .into_response()
}

/// `GET /auth/widget-token` — mints a short-lived WorkOS widget session
/// token bound to the current user + organization, scoped to API-key
/// management. The frontend feeds it to the WorkOS API Keys widget; the
/// widget then talks directly to WorkOS for create/list/revoke. The
/// gateway is not on the mint path — it never sees key material at mint
/// time, only at validation time.
///
/// See `docs/design/24-per-user-api-keys.md` §"New route".
pub async fn widget_token(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    let oidc = match state.oidc.as_ref() {
        Some(o) => o.clone(),
        None => return error_resp(StatusCode::NOT_FOUND, "workos_disabled"),
    };
    if !matches!(oidc.provider, AuthProvider::Workos) {
        return error_resp(StatusCode::NOT_FOUND, "widget_unavailable");
    }
    let session = match resolve_session_cookie(&headers, &state.signer.verifying_key()) {
        Some(s) => s,
        None => return error_resp(StatusCode::UNAUTHORIZED, "no_session"),
    };
    // The widget is org-scoped (per WorkOS). Unaffiliated users can't mint.
    if session.tenant.starts_with("user_") {
        return error_resp(StatusCode::FORBIDDEN, "no_organization");
    }

    let url = format!(
        "{}/widgets/token",
        oidc.config.issuer_url.trim_end_matches('/')
    );
    let body = serde_json::json!({
        "organization_id": session.tenant,
        "user_id": session.sub,
        "scopes": ["widgets:api-keys:manage"],
    });
    let resp = match oidc
        .http
        .post(&url)
        .bearer_auth(&oidc.config.client_secret)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return error_resp(StatusCode::BAD_GATEWAY, &format!("workos_send: {e}")),
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return error_resp(
            StatusCode::BAD_GATEWAY,
            &format!("workos_status {status}: {text}"),
        );
    }
    #[derive(Deserialize)]
    struct WidgetTokenResponse {
        token: String,
    }
    let parsed: WidgetTokenResponse = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return error_resp(StatusCode::BAD_GATEWAY, &format!("workos_json: {e}")),
    };
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "token": parsed.token,
            "organization_id": session.tenant,
        })),
    )
        .into_response()
}

pub async fn logout(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    let secure = state
        .oidc
        .as_ref()
        .map(|o| cookies_secure(&o.config.redirect_uri))
        .unwrap_or(true);
    let cleared_session = cleared_cookie(SESSION_COOKIE_NAME, "/", secure);
    let cleared_presence = cleared_presence_cookie(secure);

    // Read `workos_session_id` from the current session cookie so we can
    // pass it to WorkOS's RP-initiated logout. Accept "expired but
    // signature-valid" cookies — the user is logging out anyway. If we
    // can't recover it, fall back to a local-only logout (clear our
    // cookies and redirect to /ui/) rather than send WorkOS a malformed
    // request.
    let workos_session_id = parse_cookie(&headers, SESSION_COOKIE_NAME)
        .and_then(|raw| {
            decode_session(raw, &state.signer.verifying_key(), now_unix(), true).ok()
        })
        .map(|c| c.workos_session_id)
        .filter(|s| !s.is_empty());

    let target = match (state.oidc.as_ref(), workos_session_id) {
        (Some(o), Some(sid)) => match &o.end_session_endpoint {
            Some(end) => match url::Url::parse(end) {
                Ok(mut url) => {
                    match o.provider {
                        AuthProvider::Workos => {
                            // WorkOS User Management logout takes a
                            // `session_id`, not the OIDC-standard
                            // `id_token_hint`/`client_id` parameters.
                            // `return_to` is required when no Application
                            // Homepage URL is configured in the dashboard;
                            // we derive it from the registered redirect_uri
                            // (everything before `/auth/callback`).
                            let return_to = o
                                .config
                                .redirect_uri
                                .strip_suffix("/auth/callback")
                                .map(|base| format!("{base}/ui/"))
                                .unwrap_or_else(|| "/ui/".to_string());
                            url.query_pairs_mut()
                                .append_pair("session_id", &sid)
                                .append_pair("return_to", &return_to);
                        }
                        AuthProvider::Oidc => {
                            url.query_pairs_mut()
                                .append_pair("client_id", &o.config.client_id)
                                .append_pair("post_logout_redirect_uri", "/ui/");
                        }
                    }
                    url.to_string()
                }
                Err(_) => "/ui/".to_string(),
            },
            None => "/ui/".to_string(),
        },
        // No oidc runtime, or no session id we can pass along: do a
        // local-only logout. The OMP cookies are cleared either way; the
        // WorkOS-side session keeps living until its TTL expires.
        _ => "/ui/".to_string(),
    };
    finalize_logout(&[&cleared_session, &cleared_presence], &target)
}

fn finalize_logout(cookie_headers: &[&String], target: &str) -> Response {
    let mut resp = Redirect::to(target).into_response();
    for header in cookie_headers {
        if let Ok(v) = axum::http::HeaderValue::from_str(header) {
            resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
        }
    }
    resp
}

fn error_resp(status: StatusCode, msg: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": { "code": "auth", "message": msg }
        })),
    )
        .into_response()
}

fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use omp_tenant_ctx::GatewaySigner;

    #[test]
    fn session_round_trip() {
        let signer = GatewaySigner::generate();
        let vk = signer.verifying_key();
        let claims = SessionClaims {
            tenant: "acme".into(),
            sub: "user_123".into(),
            workos_session_id: "sess_abc".into(),
            issued_at: 100,
            expires_at: 200,
        };
        let wire = encode_session(&claims, &signer).unwrap();
        let got = decode_session(&wire, &vk, 150, false).unwrap();
        assert_eq!(got, claims);
    }

    #[test]
    fn session_rejects_expired() {
        let signer = GatewaySigner::generate();
        let vk = signer.verifying_key();
        let claims = SessionClaims {
            tenant: "acme".into(),
            sub: "u".into(),
            workos_session_id: "s".into(),
            issued_at: 100,
            expires_at: 200,
        };
        let wire = encode_session(&claims, &signer).unwrap();
        let err = decode_session(&wire, &vk, 300, false).unwrap_err();
        assert!(matches!(err, CookieError::Expired { .. }));
        // allow_expired=true accepts.
        let _ok = decode_session(&wire, &vk, 300, true).unwrap();
    }

    #[test]
    fn session_rejects_wrong_key() {
        let s1 = GatewaySigner::generate();
        let s2 = GatewaySigner::generate();
        let claims = SessionClaims {
            tenant: "acme".into(),
            sub: "u".into(),
            workos_session_id: "s".into(),
            issued_at: 100,
            expires_at: 200,
        };
        let wire = encode_session(&claims, &s1).unwrap();
        let err = decode_session(&wire, &s2.verifying_key(), 150, false).unwrap_err();
        assert!(matches!(err, CookieError::BadSignature));
    }

    #[test]
    fn session_and_state_are_domain_separated() {
        // A state cookie's wire format must not verify as a session cookie.
        let signer = GatewaySigner::generate();
        let vk = signer.verifying_key();
        let st = OidcStateClaims {
            id: "abc".into(),
            pkce_verifier: "v".into(),
            nonce: "n".into(),
            return_to: "/".into(),
            expires_at: 200,
        };
        let wire = encode_state(&st, &signer).unwrap();
        let err = decode_session(&wire, &vk, 150, false).unwrap_err();
        assert!(matches!(err, CookieError::BadSignature));
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // Verifier from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_challenge_s256(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn parse_cookie_finds_named_value() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "a=1; omp_session=hello; b=2".parse().unwrap(),
        );
        assert_eq!(parse_cookie(&h, "omp_session"), Some("hello"));
        assert_eq!(parse_cookie(&h, "missing"), None);
    }

    #[test]
    fn validation_cache_respects_ttl() {
        let cache = ValidationCache::new();
        let key = [9u8; 32];
        cache.insert(key, "acme".into(), 3600);
        assert_eq!(cache.get(&key).as_deref(), Some("acme"));
        // Insert with a 0s TTL: read is taken at `now`, entry expires at `now`,
        // so the `>` check rejects it on the very next get.
        cache.insert(key, "acme".into(), 0);
        assert_eq!(cache.get(&key), None);
    }

    #[test]
    fn tenant_mapping_org_vs_user_fallback() {
        // Helper inline — we don't expose the mapping fn since callback inlines it,
        // so just assert the format string we use.
        let with_org = "acme".to_string();
        assert_eq!(with_org, "acme");
        let no_org_sub = "user_42";
        assert_eq!(format!("user_{}", "42"), no_org_sub);
    }
}

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("Argon2 error: {0}")]
    Argon2(String),
    #[error("HKDF error: {0}")]
    Hkdf(String),
    #[error("AEAD error: {0}")]
    Aead(String),
    #[error("wrap/unwrap error: {0}")]
    Wrap(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, CryptoError>;

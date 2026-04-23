use std::path::PathBuf;

use thiserror::Error;

/// Stable error codes exposed over CLI + HTTP, matching `06-api-surface.md`
/// plus the iteration-2 tenant-layer additions (`unauthorized`, `quota_exceeded`)
/// plus `encryption_mode_mismatch` from `13-end-to-end-encryption.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NotFound,
    SchemaValidationFailed,
    IngestValidationFailed,
    ProbeFailed,
    Conflict,
    InvalidPath,
    Unauthorized,
    QuotaExceeded,
    EncryptionModeMismatch,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::NotFound => "not_found",
            ErrorCode::SchemaValidationFailed => "schema_validation_failed",
            ErrorCode::IngestValidationFailed => "ingest_validation_failed",
            ErrorCode::ProbeFailed => "probe_failed",
            ErrorCode::Conflict => "conflict",
            ErrorCode::InvalidPath => "invalid_path",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::QuotaExceeded => "quota_exceeded",
            ErrorCode::EncryptionModeMismatch => "encryption_mode_mismatch",
            ErrorCode::Internal => "internal",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::NotFound => 404,
            ErrorCode::SchemaValidationFailed => 400,
            ErrorCode::IngestValidationFailed => 400,
            ErrorCode::ProbeFailed => 400,
            ErrorCode::Conflict => 409,
            ErrorCode::InvalidPath => 400,
            ErrorCode::Unauthorized => 401,
            ErrorCode::QuotaExceeded => 429,
            ErrorCode::EncryptionModeMismatch => 409,
            ErrorCode::Internal => 500,
        }
    }
}

#[derive(Debug, Error)]
pub enum OmpError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("schema validation failed: {0}")]
    SchemaValidation(String),
    #[error("ingest validation failed: {0}")]
    IngestValidation(String),
    #[error("probe failed: {probe}: {reason}")]
    ProbeFailed { probe: String, reason: String },
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("quota exceeded: {limit}")]
    QuotaExceeded { limit: String },
    #[error("encryption mode mismatch: {0}")]
    EncryptionModeMismatch(String),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("corrupt object: {0}")]
    Corrupt(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl OmpError {
    pub fn code(&self) -> ErrorCode {
        match self {
            OmpError::NotFound(_) => ErrorCode::NotFound,
            OmpError::SchemaValidation(_) => ErrorCode::SchemaValidationFailed,
            OmpError::IngestValidation(_) => ErrorCode::IngestValidationFailed,
            OmpError::ProbeFailed { .. } => ErrorCode::ProbeFailed,
            OmpError::Conflict(_) => ErrorCode::Conflict,
            OmpError::InvalidPath(_) => ErrorCode::InvalidPath,
            OmpError::Unauthorized(_) => ErrorCode::Unauthorized,
            OmpError::QuotaExceeded { .. } => ErrorCode::QuotaExceeded,
            OmpError::EncryptionModeMismatch(_) => ErrorCode::EncryptionModeMismatch,
            OmpError::Io { .. } | OmpError::Corrupt(_) | OmpError::Internal(_) => {
                ErrorCode::Internal
            }
        }
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        OmpError::Io {
            path: path.into(),
            source,
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        OmpError::Internal(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, OmpError>;

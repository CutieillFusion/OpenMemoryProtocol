//! Time helpers shared across the workspace. Centralized so every wire-emitted
//! timestamp uses the same RFC 3339 shape.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Current UTC time as RFC 3339. Falls back to the epoch on the unreachable
/// formatter error so callers never have to thread a `Result` for what is
/// effectively infallible.
pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

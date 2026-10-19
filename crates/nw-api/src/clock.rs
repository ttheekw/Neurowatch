//! Cloud timestamps.
//!
//! The node re-serialises every timestamp it receives and rejects anything that
//! is not canonical UTC RFC 3339 (`crates/nw-backend/src/cloud/mod.rs`
//! `canonical_cloud_time`), and it signs whole-second times because fractional
//! digits would break signature verification. This module is the single place
//! that produces and parses those strings, so the two sides cannot drift.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Now, truncated to whole seconds and in UTC.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
}

/// Format as canonical UTC RFC 3339 with whole seconds.
pub fn format(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    let at = at.replace_nanosecond(0).unwrap_or(at);
    at.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// Parse an RFC 3339 timestamp, requiring a UTC offset. A node that sends a
/// local-offset time is rejected rather than silently re-interpreted.
pub fn parse(raw: &str) -> Option<OffsetDateTime> {
    let parsed = OffsetDateTime::parse(raw, &Rfc3339).ok()?;
    (parsed.offset() == time::UtcOffset::UTC).then_some(parsed)
}

/// Now, formatted.
pub fn now_string() -> String {
    format(now())
}

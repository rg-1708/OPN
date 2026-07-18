//! One place to turn a Postgres `timestamptz` into the RFC 3339 string every
//! event/payload puts on the wire — so the format never drifts between
//! primitives.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub fn rfc3339(t: OffsetDateTime) -> String {
    // Formatting a valid OffsetDateTime as RFC 3339 cannot fail in practice;
    // an empty string on the impossible error beats an unwrap on a wire path.
    t.format(&Rfc3339).unwrap_or_default()
}

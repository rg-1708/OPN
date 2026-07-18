//! The one pagination idiom (OPN-CORE.md CDR-7). Every time-ordered read —
//! inbox now, feed/gallery/ledger later — keysets on `(created_at, id)` and
//! encodes its position with this. Opaque base64url of a `(micros, uuid)`
//! pair: the client round-trips it, never parses it.
//!
//! Seq-keyed reads (channel history) do NOT use this — seq is already public
//! in that contract, so the cursor is just the seq itself.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use contracts::ErrCode;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::primitives::Fail;

/// A decoded keyset position. `ts` is the row's `created_at`; `id` breaks ties
/// so two rows at the same microsecond still page deterministically.
pub struct Cursor {
    pub ts: OffsetDateTime,
    pub id: Uuid,
}

/// Encode a `(created_at, id)` position. Microsecond precision matches
/// Postgres `timestamptz`, so the decoded value compares exactly against the
/// stored column.
pub fn encode(ts: OffsetDateTime, id: Uuid) -> String {
    let micros = (ts.unix_timestamp_nanos() / 1_000) as i64;
    let payload: (i64, Uuid) = (micros, id);
    // A 2-tuple of a number and a uuid serializes infallibly.
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a client cursor. Any malformed input (bad base64, bad JSON, an
/// out-of-range timestamp) is `invalid` — never a panic (fuzzed in Sprint 9).
pub fn decode(s: &str) -> Result<Cursor, Fail> {
    let bytes = URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| Fail::Code(ErrCode::Invalid))?;
    let (micros, id): (i64, Uuid) =
        serde_json::from_slice(&bytes).map_err(|_| Fail::Code(ErrCode::Invalid))?;
    let ts = OffsetDateTime::from_unix_timestamp_nanos(micros as i128 * 1_000)
        .map_err(|_| Fail::Code(ErrCode::Invalid))?;
    Ok(Cursor { ts, id })
}

/// A page of results plus the cursor to fetch the next one (`None` = last page).
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

/// Turn an overfetched row set into a page. The caller queries `limit + 1`
/// rows; if the extra row is present there is a next page, and its cursor is
/// the keyset position of the last *returned* row (`key` extracts it).
pub fn page<T>(
    mut rows: Vec<T>,
    limit: usize,
    key: impl Fn(&T) -> (OffsetDateTime, Uuid),
) -> Page<T> {
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let next_cursor = if has_more {
        rows.last().map(|r| {
            let (ts, id) = key(r);
            encode(ts, id)
        })
    } else {
        None
    };
    Page {
        items: rows,
        next_cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let ts = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("cursor test")
            .replace_microsecond(123_456)
            .expect("cursor test");
        let id = Uuid::now_v7();
        let c = decode(&encode(ts, id)).expect("decodes");
        assert_eq!(c.ts, ts, "microsecond-exact round-trip");
        assert_eq!(c.id, id);
    }

    #[test]
    fn garbage_is_invalid_never_panics() {
        for s in ["", "!!!!", "Zm9v", "not-base64-@@", "YWJjZGVmZw"] {
            assert!(
                matches!(decode(s), Err(Fail::Code(ErrCode::Invalid))),
                "{s}"
            );
        }
    }

    #[test]
    fn page_truncates_and_emits_cursor() {
        // 3 rows fetched with limit 2 → one page of 2 + a next cursor.
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("cursor test");
        let rows: Vec<(OffsetDateTime, Uuid)> =
            (0..3).map(|i| (base, Uuid::from_u128(i))).collect();
        let last_kept = rows[1];
        let p = page(rows, 2, |r| (r.0, r.1));
        assert_eq!(p.items.len(), 2);
        let cur = decode(&p.next_cursor.expect("has next")).expect("cursor test");
        assert_eq!(
            cur.id, last_kept.1,
            "cursor points at the last returned row"
        );
    }

    #[test]
    fn page_no_overflow_no_cursor() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("cursor test");
        let rows: Vec<(OffsetDateTime, Uuid)> =
            (0..2).map(|i| (base, Uuid::from_u128(i))).collect();
        let p = page(rows, 2, |r| (r.0, r.1));
        assert_eq!(p.items.len(), 2);
        assert!(p.next_cursor.is_none(), "exactly `limit` rows → last page");
    }
}

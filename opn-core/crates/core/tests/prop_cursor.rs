//! Sprint 9 proptest layer for the pagination cursor (`infra::cursor`, CDR-7):
//! `encode`/`decode` round-trips microsecond-exact, and `decode` is total —
//! arbitrary strings and arbitrary bytes never panic, only `Ok` or the
//! `invalid` contract error.

use contracts::ErrCode;
use opn_core::infra::cursor::{decode, encode, Cursor};
use opn_core::primitives::Fail;
use proptest::prelude::*;

proptest! {
    /// 1. Round-trip: micro-aligned `ts` + any `id` survive encode→decode exactly.
    ///    The range stays well inside `OffsetDateTime` (~years 0002–9999); the
    ///    `prop_assume!` drops any micros that still fall out of range.
    #[test]
    fn round_trip(
        micros in -62_000_000_000_000_000i64..=253_000_000_000_000_000i64,
        id_bits in any::<u128>(),
    ) {
        let ts_result = time::OffsetDateTime::from_unix_timestamp_nanos(micros as i128 * 1_000);
        prop_assume!(ts_result.is_ok());
        let ts = ts_result.expect("assumed ok above");
        let id = uuid::Uuid::from_u128(id_bits);

        let c: Cursor = decode(&encode(ts, id)).expect("encode output must decode");
        prop_assert_eq!(c.ts, ts);
        prop_assert_eq!(c.id, id);
    }

    /// 2. Arbitrary strings are total: `Ok` or `Fail::Code(Invalid)`, never a panic.
    #[test]
    fn arbitrary_strings_never_panic(s in ".*") {
        if let Err(e) = decode(&s) {
            prop_assert!(matches!(e, Fail::Code(ErrCode::Invalid)));
        }
    }

    /// 3. Arbitrary bytes (lossy-decoded to a string) are total too — a second
    ///    angle on totality, no base64 dependency needed.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let s = String::from_utf8_lossy(&bytes);
        if let Err(e) = decode(&s) {
            prop_assert!(matches!(e, Fail::Code(ErrCode::Invalid)));
        }
    }
}

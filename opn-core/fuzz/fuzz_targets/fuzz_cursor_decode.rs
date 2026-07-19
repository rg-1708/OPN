#![no_main]
//! Fuzz the pagination cursor decoder (roadmap Sprint 9 item 2, CDR-7): the
//! base64url → JSON → `OffsetDateTime` chain is a classic panic trap. Every
//! paginated read decodes a client-supplied cursor through `cursor::decode`,
//! whose contract is `Ok`/`Fail::Code(Invalid)` and never a panic. A crash on
//! any input is a bug (the property proven generatively in `prop_cursor.rs`,
//! proven exhaustively here).

use libfuzzer_sys::fuzz_target;

use opn_core::infra::cursor;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = cursor::decode(s);
    }
});

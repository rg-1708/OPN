#![no_main]
//! Fuzz the tenant-link hello frame (roadmap Sprint 9 item 2, §5): the link's
//! first frame is `serde_json::from_str::<LinkHello>` on a WS Text frame
//! (already UTF-8). A malformed hello is a clean `BAD_HELLO` close, never a
//! panic (gateway/link.rs) — a crash on any input is a bug.

use libfuzzer_sys::fuzz_target;

use contracts::LinkHello;

fuzz_target!(|data: &[u8]| {
    // WS Text frames are UTF-8; feed the parser the same shape it sees live.
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = serde_json::from_str::<LinkHello>(s);
    }
});

#![no_main]
//! Fuzz the untrusted client-frame surface (roadmap Sprint 9 item 2, §15):
//! `serde_json::from_slice::<ClientFrame>` over arbitrary bytes, then — on a
//! successful parse — the *synchronous, DB-free* pre-handler validation the
//! dispatch loop runs before any handler: topic parse (§4.4) and body caps
//! (§10.2/§10.3). The dispatch contract is "a bad frame becomes an ack, never a
//! panic" (dispatch.rs), so a crash on ANY input here is a bug by definition.

use libfuzzer_sys::fuzz_target;

use contracts::{ClientFrame, Cmd};
use opn_core::gateway::topic::TopicKind;
use opn_core::primitives::{channels, feed};

fuzz_target!(|data: &[u8]| {
    // Layer 1: the parse itself is the primary attacker-controlled surface.
    let Ok(frame) = serde_json::from_slice::<ClientFrame>(data) else {
        return;
    };
    // Layer 2: mirror the pre-handler checks in `gateway::dispatch::run` that
    // touch neither the DB nor the registry — the only pure validation, and the
    // one place arbitrary payload shape reaches synchronous code.
    match frame.cmd {
        Cmd::Sub { topic, .. } | Cmd::Unsub { topic } => {
            let _ = TopicKind::parse(&topic);
        }
        Cmd::ChannelsSend { body, .. } => {
            let _ = channels::validate_body(&body);
        }
        Cmd::FeedPost { body, .. } | Cmd::FeedComment { body, .. } => {
            let _ = feed::validate_doc(&body);
        }
        _ => {}
    }
});

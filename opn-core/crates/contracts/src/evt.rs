use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Every server→client pushed event. Same tagging idiom as `Cmd`.
///
/// Variants land with their primitives (first ones in Sprint 2/3); the enum
/// exists from day one so the contracts drift gate and the coverage
/// match-test exist from day one.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "evt", content = "payload", rename_all = "snake_case")]
#[ts(export)]
pub enum Evt {}

/// Backpressure class (OPN-CORE.md §4.3): durable events close a slow
/// consumer when its queue is full; ephemeral events are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvtClass {
    Durable,
    Ephemeral,
}

impl Evt {
    /// Exhaustive by construction: every new event variant fails to compile
    /// until it declares its backpressure class here.
    pub fn class(&self) -> EvtClass {
        match *self {}
    }
}

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Every client→server command. Serde does the routing, `match` does the
/// dispatch, the compiler does the exhaustiveness (OPN-CORE.md §7).
///
/// Wire shape: `{ "cmd": "<snake_case name>", "payload": { ... } }`
/// (unit variants omit `payload`).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
#[ts(export)]
pub enum Cmd {
    Sub {
        topic: String,
        #[ts(type = "number | null")]
        last_seq: Option<i64>,
    },
    Unsub {
        topic: String,
    },
    AuthRefresh,
}

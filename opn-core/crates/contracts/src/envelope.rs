use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{Cmd, ErrBody, Evt};

/// Client→server frame. On the wire the command flattens into the frame:
/// `{ "id": 7, "cmd": "sub", "payload": { ... } }` (OPN-CORE.md §7).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ClientFrame {
    /// Client-chosen correlation id, echoed back as `reply_to` in the ack.
    /// JSON number on the wire (clients never approach 2^53 frames).
    #[ts(type = "number")]
    pub id: u64,
    #[serde(flatten)]
    #[ts(flatten)]
    pub cmd: Cmd,
}

/// Server→client frame. Untagged on the wire: an ack always carries
/// `reply_to`, a push always carries `topic` + `evt` — the client
/// distinguishes by field presence.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(untagged)]
#[ts(export)]
pub enum ServerMsg {
    Ack {
        #[ts(type = "number")]
        reply_to: u64,
        ok: bool,
        /// Typed per-command ack payload. `serde_json::Value` only until the
        /// command lands a concrete type (roadmap Sprint 0 rule) — never
        /// beyond the sprint that adds the command.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        payload: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        err: Option<ErrBody>,
    },
    Push {
        topic: String,
        #[serde(flatten)]
        #[ts(flatten)]
        evt: Evt,
    },
}

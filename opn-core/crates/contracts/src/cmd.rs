use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// Every client→server command. Serde does the routing, `match` does the
/// dispatch, the compiler does the exhaustiveness (OPN-CORE.md §7).
///
/// Wire shape: `{ "cmd": "<name>", "payload": { ... } }` (unit variants omit
/// `payload`). `sub`/`unsub` come from the enum-level `rename_all`; the
/// dotted `auth.refresh`/`identity.*` names are pinned per-variant to match
/// the design doc (OPN-CORE.md §10.1).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
#[ts(export)]
pub enum Cmd {
    /// First frame on a fresh WS connection (§4.1): carries the mint/refresh
    /// JWT. Sent again after auth it acks `conflict`.
    Auth {
        token: String,
    },
    Sub {
        topic: String,
        #[ts(type = "number | null")]
        last_seq: Option<i64>,
    },
    Unsub {
        topic: String,
    },
    #[serde(rename = "auth.refresh")]
    AuthRefresh,
    #[serde(rename = "identity.me")]
    IdentityMe,
    #[serde(rename = "identity.app_login")]
    IdentityAppLogin {
        app_id: String,
        account_id: Uuid,
    },
    #[serde(rename = "identity.get_settings")]
    IdentityGetSettings {
        scope: SettingsScope,
    },
    #[serde(rename = "identity.set_settings")]
    IdentitySetSettings {
        scope: SettingsScope,
        /// Whole-document replace, opaque to Core.
        #[ts(type = "unknown")]
        patch: serde_json::Value,
    },
    #[serde(rename = "identity.set_share_presence")]
    IdentitySetSharePresence {
        on: bool,
    },
}

/// Which settings document an `identity.*_settings` command targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SettingsScope {
    Device,
    Character,
}

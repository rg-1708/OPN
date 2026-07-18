//! Response / payload types for identity commands (OPN-CORE.md §10.1).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CharacterInfo {
    pub id: Uuid,
    pub framework_ref: String,
    /// Plain `Option`: serde emits `null` (not absent), so TS is `string | null`.
    pub number: Option<String>,
    pub share_presence: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeviceInfo {
    pub id: Uuid,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AppAccountInfo {
    pub id: Uuid,
    pub app_id: String,
    pub handle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionMintResponse {
    pub token: String,
    pub session_id: Uuid,
    pub character: CharacterInfo,
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MePayload {
    pub character: CharacterInfo,
    pub device: DeviceInfo,
    pub accounts: Vec<AppAccountInfo>,
    /// The session's `{ app_id: account_id }` map.
    #[ts(type = "Record<string, string>")]
    pub active_accounts: serde_json::Value,
}

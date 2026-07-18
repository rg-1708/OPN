use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The closed error-code set (OPN-CORE.md §7). Adding a variant is a
/// contracts-major event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ErrCode {
    Unauthorized,
    Forbidden,
    NotFound,
    Invalid,
    Conflict,
    RateLimited,
    TooLarge,
    Internal,
}

/// Wire error body: `{ "code": "...", "msg": "..." }`. `msg` is
/// developer-facing; UI copy is the app's job keyed off `code`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, rename = "Err")]
pub struct ErrBody {
    pub code: ErrCode,
    pub msg: String,
}

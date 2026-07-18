//! Primitives (OPN-CORE.md §10): stateless functions over the pools.
//! Handlers return `Result<T, Fail>`; dispatch (Sprint 2) turns that into
//! the wire ack.

use contracts::ErrCode;

pub mod channels;
pub mod directory;
pub mod identity;
pub mod notify;

/// Handler failure: a deliberate protocol error (acked with its code) or an
/// internal error (logged, acked `internal` with no detail — §7).
#[derive(Debug)]
pub enum Fail {
    Code(ErrCode),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Fail {
        Fail::Internal(e)
    }
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Fail {
        Fail::Internal(e.into())
    }
}

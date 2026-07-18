//! ALL wire types for OPN-CORE (OPN-CORE.md §2, §7).
//!
//! This crate is the public surface: serde defines the wire shape, ts-rs
//! exports the same types to `bindings/*.d.ts` (published as @opn/contracts).
//! It must never depend on `core`.

pub mod cmd;
pub mod envelope;
pub mod error;
pub mod evt;

pub use cmd::Cmd;
pub use envelope::{ClientFrame, ServerMsg};
pub use error::{ErrBody, ErrCode};
pub use evt::{Evt, EvtClass};

//! ALL wire types for OPN-CORE (OPN-CORE.md §2, §7).
//!
//! This crate is the public surface: serde defines the wire shape, ts-rs
//! exports the same types to `bindings/*.d.ts` (published as @opn/contracts).
//! It must never depend on `core`.

pub mod cmd;
pub mod envelope;
pub mod error;
pub mod evt;
pub mod types;

pub use cmd::{Cmd, SettingsScope};
pub use envelope::{ClientFrame, ServerMsg};
pub use error::{ErrBody, ErrCode};
pub use evt::{Evt, EvtClass};
pub use types::{
    ActiveCall, AppAccountInfo, CallKind, CallParticipant, CallParticipantState, CallSessionState,
    ChannelSummary, CharacterInfo, CommentItem, ContactItem, DeviceInfo, FeedActivityKind,
    GroupJoinAck, InboxItem, LinkHello, ListingItem, MePayload, MediaItem, MediaKind, MessageBody,
    MessageItem, MessagePreview, NotifyClass, PostItem, ReceiptKind, ResolveResult,
    SessionMintResponse, Topology, TransferItem, UploadTarget, UploadTicket, VoiceAction,
};

/// This crate's version, embedded at compile time from `Cargo.toml`
/// (`CARGO_PKG_VERSION`) — the single source of truth for the wire-contract
/// version. Surfaced at runtime in the `/healthz` body and the `/link` hello
/// ack, and the value the `@opn/contracts` npm publish tags (roadmap Sprint 11
/// item 6). Additive-only within a major (OPN.md §10.1).
pub const CONTRACTS_VERSION: &str = env!("CARGO_PKG_VERSION");

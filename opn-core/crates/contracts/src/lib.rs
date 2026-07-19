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
    ChannelSummary, CharacterInfo, ContactItem, DeviceInfo, FeedActivityKind, InboxItem, LinkHello,
    ListingItem, MePayload, MediaItem, MediaKind, MessageBody, MessageItem, MessagePreview,
    NotifyClass, ReceiptKind, ResolveResult, SessionMintResponse, TransferItem, UploadTarget,
    UploadTicket, VoiceAction,
};

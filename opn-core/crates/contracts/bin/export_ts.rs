//! Writes `bindings/*.ts` from the contracts types. Output is committed;
//! CI regenerates and fails on diff (drift gate, roadmap cross-cutting rule 1).

use ts_rs::TS;

fn main() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/bindings");
    contracts::ClientFrame::export_all_to(dir).expect("export ClientFrame");
    contracts::ServerMsg::export_all_to(dir).expect("export ServerMsg");
    contracts::Cmd::export_all_to(dir).expect("export Cmd");
    contracts::Evt::export_all_to(dir).expect("export Evt");
    contracts::ErrCode::export_all_to(dir).expect("export ErrCode");
    contracts::SessionMintResponse::export_all_to(dir).expect("export SessionMintResponse");
    contracts::MePayload::export_all_to(dir).expect("export MePayload");
    // Response payloads not reachable from the Cmd/Evt graph (they ride acks as
    // opaque values on the wire, or come back over HTTP), exported explicitly so
    // the TS SDK still types them.
    contracts::ChannelSummary::export_all_to(dir).expect("export ChannelSummary");
    contracts::InboxItem::export_all_to(dir).expect("export InboxItem");
    // History rides HTTP, not the Cmd/Evt graph (ReactionItem rides along as a
    // dependency of MessageItem).
    contracts::MessageItem::export_all_to(dir).expect("export MessageItem");
    // channels.members ack rides the WS ack as an opaque value, so export it
    // explicitly for the TS SDK.
    contracts::ChannelMember::export_all_to(dir).expect("export ChannelMember");
    // media: request_upload ack and the gallery row (UploadTarget rides along
    // as a dependency of UploadTicket).
    contracts::UploadTicket::export_all_to(dir).expect("export UploadTicket");
    contracts::MediaItem::export_all_to(dir).expect("export MediaItem");
    // directory: contact/listing rows and the opaque resolve result ride acks as
    // opaque values, so export them explicitly for the TS SDK.
    contracts::ContactItem::export_all_to(dir).expect("export ContactItem");
    contracts::ResolveResult::export_all_to(dir).expect("export ResolveResult");
    contracts::ListingItem::export_all_to(dir).expect("export ListingItem");
    // tenant link (§5): the hello handshake frame and the /calls/active re-sync
    // row — neither rides the Cmd/Evt graph (VoiceAction does, via calls.voice).
    contracts::LinkHello::export_all_to(dir).expect("export LinkHello");
    contracts::ActiveCall::export_all_to(dir).expect("export ActiveCall");
    // group calls (opn-group-calls.md G0): the join ack rides the ack as an
    // opaque value, so export it explicitly (Topology is already reachable via
    // Evt::CallsState / Evt::CallsGroupState).
    contracts::GroupJoinAck::export_all_to(dir).expect("export GroupJoinAck");
    // ledger (§10.5): the history row rides HTTP, not the Cmd/Evt graph.
    contracts::TransferItem::export_all_to(dir).expect("export TransferItem");
    // feed (§10.3): the read-surface rows ride HTTP, not the Cmd/Evt graph
    // (FeedActivityKind is already reachable via Evt::FeedActivity).
    contracts::PostItem::export_all_to(dir).expect("export PostItem");
    contracts::CommentItem::export_all_to(dir).expect("export CommentItem");
    println!("bindings written to {dir}");
}

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
    // History rides HTTP, not the Cmd/Evt graph.
    contracts::MessageItem::export_all_to(dir).expect("export MessageItem");
    // media: request_upload ack and the gallery row (UploadTarget rides along
    // as a dependency of UploadTicket).
    contracts::UploadTicket::export_all_to(dir).expect("export UploadTicket");
    contracts::MediaItem::export_all_to(dir).expect("export MediaItem");
    // directory: contact/listing rows and the opaque resolve result ride acks as
    // opaque values, so export them explicitly for the TS SDK.
    contracts::ContactItem::export_all_to(dir).expect("export ContactItem");
    contracts::ResolveResult::export_all_to(dir).expect("export ResolveResult");
    contracts::ListingItem::export_all_to(dir).expect("export ListingItem");
    println!("bindings written to {dir}");
}

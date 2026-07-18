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
    println!("bindings written to {dir}");
}

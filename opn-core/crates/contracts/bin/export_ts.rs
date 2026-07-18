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
    println!("bindings written to {dir}");
}

//! group-call control-plane probe (opn-group-calls.md G3): one client runs
//! `calls.group.create` -> `calls.group.join` and asserts Core acks both and
//! mints a LiveKit access token. Used by `chaos/livekit-down.sh` to prove the
//! group *control plane* is decoupled from SFU liveness: Core mints the token
//! in-process (HS256 over the shared secret, no synchronous LiveKit call), so
//! this passes even with the SFU killed. 1:1 calls and the data plane have their
//! own drills; this owns the "group control plane survives an SFU outage"
//! invariant — and is the regression guard against anyone making create/join
//! block on a live LiveKit API.
//!
//! Exit 0 = control plane healthy, 1 = it broke, 2 = setup failure.

use anyhow::{anyhow, bail, Context, Result};
use contracts::Cmd;
use std::process::ExitCode;
use uuid::Uuid;

use crate::driver::{await_ack, send};
use crate::http;
use crate::linkdrop::connect_auth;

/// `http://host:port[/...]` -> `host:port` (what `http::mint` wants — no scheme).
fn host_of_http(url: &str) -> Result<String> {
    let s = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| anyhow!("http target needs an http(s):// scheme: {url}"))?;
    Ok(s.split('/').next().unwrap_or(s).to_string())
}

pub async fn verify_group_probe(http_target: &str, ws_url: &str) -> Result<ExitCode> {
    let api_key = std::env::var("OPN_LOADGEN_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .context("OPN_LOADGEN_API_KEY must be set for --group-probe")?;
    match probe(http_target, ws_url, &api_key).await {
        Ok(()) => {
            println!("group-probe: OK (create+join acked, token minted)");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("group-probe: FAIL — {e:#}");
            Ok(ExitCode::from(1))
        }
    }
}

async fn probe(http_target: &str, ws_url: &str, api_key: &str) -> Result<()> {
    let host = host_of_http(http_target)?;
    let m = http::mint(&host, api_key, "grp:probe")
        .await
        .context("mint session")?;
    let mut ws = connect_auth(ws_url, &m.token)
        .await
        .context("connect+auth")?;

    // create -> ok ack with a call_id (the creator auto-joins the room).
    send(
        &mut ws,
        1,
        Cmd::CallsGroupCreate {
            label: Some("chaos".into()),
            max_participants: None,
        },
    )
    .await?;
    let (ok, payload) = await_ack(&mut ws, 1).await?;
    if !ok {
        bail!("calls.group.create not ok: {payload:?}");
    }
    let call_id: Uuid = payload
        .as_ref()
        .and_then(|v| v["call_id"].as_str())
        .ok_or_else(|| anyhow!("create ack missing call_id: {payload:?}"))?
        .parse()
        .context("call_id not a uuid")?;

    // join (creator rejoin is allowed) -> ok ack carrying sfu_url + a non-empty
    // token. The token is signed in-process from the shared secret — Core never
    // calls the LiveKit server — so this holds with the SFU dead. That is the
    // whole point of the drill.
    send(&mut ws, 2, Cmd::CallsGroupJoin { call_id }).await?;
    let (ok, payload) = await_ack(&mut ws, 2).await?;
    if !ok {
        bail!("calls.group.join not ok: {payload:?}");
    }
    let p = payload.ok_or_else(|| anyhow!("join ack missing payload"))?;
    let token = p["token"].as_str().unwrap_or_default();
    let sfu_url = p["sfu_url"].as_str().unwrap_or_default();
    if token.is_empty() || sfu_url.is_empty() {
        bail!("join ack missing sfu_url/token: {p}");
    }
    Ok(())
}

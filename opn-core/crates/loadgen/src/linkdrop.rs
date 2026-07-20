//! `--link-drop <http> <ws> [drop_gap_secs]` (roadmap Sprint 9 item 3, the
//! `link-drop` chaos drill).
//!
//! The tenant `/link` resource (a FiveM server, out-of-repo) is the down-only
//! consumer of `calls.voice` events (§5). This checker plays that resource: it
//! connects a `/link` consumer, drives a call to `active` so the link receives
//! `set_targets`, then **drops the link socket mid-call** (the resource
//! crashes) and reconnects. On reconnect it must (1) recover the still-active
//! call via `GET /v1/tenants/self/calls/active` — the re-sync route, since the
//! link never re-emits existing calls on connect (§5) — and (2) receive
//! `set_targets` again for a *subsequent* accept on the reconnected link (the
//! link delivers once more, the resubscribe analog of the redis drill).
//!
//! The fault is entirely resource-side (a client disconnect), so unlike the
//! redis/pg drills there is no infra fault for bash to inject — the checker
//! drops its own socket. Core is never touched.
//!
//! The post-reconnect call uses *fresh* parties: call #1's caller and callee are
//! still `joined` in the un-ended session, so `calls.start` would reject them
//! `busy` — a second call needs two un-busy numbers.
//!
//! Exit 0 = both invariants held; 1 = one broke; 2 = setup error.

use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use contracts::{Cmd, Evt, ServerMsg, VoiceAction};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use crate::driver::{await_ack, send};
use crate::http;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long to wait for `set_targets` to arrive on the link after an accept.
const SET_TARGETS_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn verify_linkdrop(http_target: &str, ws: &str, drop_gap_secs: u64) -> Result<ExitCode> {
    let api_key = std::env::var("OPN_LOADGEN_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .context("OPN_LOADGEN_API_KEY must be set for --link-drop")?;
    let host = host_of_http(http_target)?;
    // The tenant link lives at /link, not /ws; derive it from the /ws base.
    let link_ws = link_url(ws)?;

    // Two parties for the pre-drop call, two fresh for the post-reconnect call
    // (the pre-call parties stay busy in the still-active session).
    let caller1 = http::mint(&host, &api_key, "ld:caller1")
        .await
        .context("mint caller1")?;
    let callee1 = http::mint(&host, &api_key, "ld:callee1")
        .await
        .context("mint callee1")?;
    let caller2 = http::mint(&host, &api_key, "ld:caller2")
        .await
        .context("mint caller2")?;
    let callee2 = http::mint(&host, &api_key, "ld:callee2")
        .await
        .context("mint callee2")?;

    // Hold both pre-call sockets open through re-sync so the session stays live
    // (a WS disconnect does not end a call — §10.4 — but keeping them avoids any
    // ambiguity while the drill asserts the call is still active).
    let mut caller1_ws = connect_auth(ws, &caller1.token)
        .await
        .context("connect+auth caller1")?;
    let mut callee1_ws = connect_auth(ws, &callee1.token)
        .await
        .context("connect+auth callee1")?;

    let mut link = connect_link(&link_ws, &api_key)
        .await
        .context("connect link consumer")?;

    // ── PRE: accept a call → the link must receive set_targets ───────────────
    let call1 = start_call(&mut caller1_ws, &callee1.number)
        .await
        .context("start call1")?;
    accept_call(&mut callee1_ws, call1)
        .await
        .context("accept call1")?;
    if !await_set_targets(&mut link, call1).await? {
        eprintln!("link-drop: FAIL — link never received set_targets before the drop");
        return Ok(ExitCode::from(1));
    }
    println!("link-drop: PRE set_targets OK");

    // ── FAULT: the resource crashes — drop the link socket, wait a beat ──────
    drop(link);
    sleep(Duration::from_secs(drop_gap_secs)).await;

    // ── RECONNECT + RE-SYNC ──────────────────────────────────────────────────
    let mut link = connect_link(&link_ws, &api_key)
        .await
        .context("reconnect link consumer")?;
    let list = http::get(&host, &api_key, "/v1/tenants/self/calls/active")
        .await
        .context("re-sync GET /calls/active")?;
    let arr = list
        .as_array()
        .ok_or_else(|| anyhow!("active calls response was not an array"))?;
    if !active_has_call(arr, call1) {
        eprintln!("link-drop: FAIL — re-sync did not return the still-active call {call1}");
        return Ok(ExitCode::from(1));
    }
    println!("link-drop: re-sync returned the active call");

    // ── POST: a subsequent accept must reach the reconnected link ────────────
    let mut caller2_ws = connect_auth(ws, &caller2.token)
        .await
        .context("connect+auth caller2")?;
    let mut callee2_ws = connect_auth(ws, &callee2.token)
        .await
        .context("connect+auth callee2")?;
    let call2 = start_call(&mut caller2_ws, &callee2.number)
        .await
        .context("start call2")?;
    accept_call(&mut callee2_ws, call2)
        .await
        .context("accept call2")?;
    if !await_set_targets(&mut link, call2).await? {
        eprintln!(
            "link-drop: FAIL — reconnected link never received set_targets \
             (a subsequent accept was lost after the drop)"
        );
        return Ok(ExitCode::from(1));
    }
    println!("link-drop: POST set_targets OK");

    // Keep the pre-call sockets referenced until here so they cannot be dropped
    // early by an over-eager optimizer; then close them cleanly.
    let _ = caller1_ws.close(None).await;
    let _ = callee1_ws.close(None).await;

    eprintln!(
        "link-drop: PASS — re-sync recovered the active call and the reconnected \
         link received targets for a subsequent accept"
    );
    Ok(ExitCode::SUCCESS)
}

/// Is this frame a `calls.voice` `set_targets` push for `call_id`?
fn is_set_targets(text: &str, call_id: Uuid) -> bool {
    matches!(
        serde_json::from_str::<ServerMsg>(text),
        Ok(ServerMsg::Push {
            evt: Evt::CallsVoice { call_id: cid, action: VoiceAction::SetTargets, .. },
            ..
        }) if cid == call_id
    )
}

/// Does the `/calls/active` array contain `call_id` in the `active` state?
fn active_has_call(list: &[Value], call_id: Uuid) -> bool {
    let want = call_id.to_string();
    list.iter().any(|c| {
        c.get("call_id").and_then(Value::as_str) == Some(want.as_str())
            && c.get("state").and_then(Value::as_str) == Some("active")
    })
}

/// Drain the link until a matching `set_targets` arrives, or time out. A timeout
/// is a *lost delivery* (`Ok(false)`), the invariant break; a closed/errored
/// socket is operational (`Err`).
async fn await_set_targets(link: &mut Ws, call_id: Uuid) -> Result<bool> {
    let found = timeout(SET_TARGETS_TIMEOUT, async {
        loop {
            match link.next().await {
                None => bail!("link stream closed awaiting set_targets"),
                Some(Err(e)) => bail!("link ws error awaiting set_targets: {e}"),
                Some(Ok(Message::Text(t))) => {
                    if is_set_targets(&t, call_id) {
                        return Ok(());
                    }
                }
                Some(Ok(Message::Close(_))) => bail!("link closed awaiting set_targets"),
                Some(Ok(_)) => {} // ping/pong/binary
            }
        }
    })
    .await;
    match found {
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(false),
    }
}

/// Connect a `/ws` session and authenticate it. Shared with the call-churn
/// driver (`callchurn.rs`), which drives many of these party sockets.
pub(crate) async fn connect_auth(ws_url: &str, token: &str) -> Result<Ws> {
    let mut ws = connect_async(ws_url)
        .await
        .with_context(|| format!("ws connect {ws_url}"))?
        .0;
    send(
        &mut ws,
        1,
        Cmd::Auth {
            token: token.to_owned(),
        },
    )
    .await?;
    let (ok, _) = await_ack(&mut ws, 1).await?;
    if !ok {
        bail!("auth ack not ok");
    }
    Ok(ws)
}

/// Connect the tenant `/link` consumer: API-key `Authorization` header (§1),
/// send the `LinkHello`, await the hello ack (`{reply_to:0, ok:true}`). Shared
/// with the call-churn driver, whose one link consumer drains the tenant's
/// `set_targets`/`clear` events under load.
pub(crate) async fn connect_link(ws_url: &str, api_key: &str) -> Result<Ws> {
    let mut req = ws_url
        .into_client_request()
        .with_context(|| format!("build link request for {ws_url}"))?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {api_key}")
            .parse()
            .context("api key not a valid header value")?,
    );
    let mut link = connect_async(req)
        .await
        .with_context(|| format!("link connect {ws_url}"))?
        .0;

    let hello = json!({ "resource_version": "chaos-link/0", "contracts_version": "0" });
    link.send(Message::Text(hello.to_string().into()))
        .await
        .context("send link hello")?;
    await_hello_ack(&mut link).await?;
    Ok(link)
}

/// Read link frames until the hello ack (`reply_to:0, ok:true`). 5 s timeout.
async fn await_hello_ack(link: &mut Ws) -> Result<()> {
    let fut = async {
        loop {
            match link.next().await {
                None => bail!("link stream closed before hello ack"),
                Some(Err(e)) => bail!("link ws error before hello ack: {e}"),
                Some(Ok(Message::Text(t))) => {
                    if let Ok(ServerMsg::Ack {
                        reply_to: 0,
                        ok: true,
                        ..
                    }) = serde_json::from_str::<ServerMsg>(&t)
                    {
                        return Ok(());
                    }
                }
                Some(Ok(Message::Close(c))) => bail!("link closed before hello ack: {c:?}"),
                Some(Ok(_)) => {}
            }
        }
    };
    timeout(Duration::from_secs(5), fut)
        .await
        .context("timed out awaiting link hello ack")?
}

/// `calls.start { callee_number, video: false }` → the new `call_id`. Shared
/// with the call-churn driver. Frame id 2 is safe to reuse per socket because
/// every caller drives its socket strictly sequentially (send → await ack →
/// next), so there is never a second id-2 frame in flight.
pub(crate) async fn start_call(caller: &mut Ws, callee_number: &str) -> Result<Uuid> {
    send(
        caller,
        2,
        Cmd::CallsStart {
            callee_number: callee_number.to_owned(),
            video: false,
        },
    )
    .await?;
    let (ok, payload) = await_ack(caller, 2).await?;
    if !ok {
        bail!("calls.start ack not ok");
    }
    let cid = payload
        .as_ref()
        .and_then(|p| p.get("call_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("calls.start ack missing call_id"))?;
    Uuid::parse_str(cid).context("call_id not a uuid")
}

/// `calls.accept { call_id }` from the callee — the transition that lands the
/// session in `active` and emits `set_targets` on the link. Shared with the
/// call-churn driver.
pub(crate) async fn accept_call(callee: &mut Ws, call_id: Uuid) -> Result<()> {
    send(callee, 2, Cmd::CallsAccept { call_id }).await?;
    let (ok, _) = await_ack(callee, 2).await?;
    if !ok {
        bail!("calls.accept ack not ok");
    }
    Ok(())
}

/// `ws://127.0.0.1:8080/ws` → `ws://127.0.0.1:8080/link` — the tenant link
/// endpoint on the same Core. The harness always passes the `/ws` gateway URL.
/// Shared with the call-churn driver.
pub(crate) fn link_url(ws: &str) -> Result<String> {
    let base = ws
        .strip_suffix("/ws")
        .ok_or_else(|| anyhow!("ws url must end in /ws: {ws}"))?;
    Ok(format!("{base}/link"))
}

/// `http://127.0.0.1:8080/…` → `127.0.0.1:8080` for `TcpStream::connect`.
fn host_of_http(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("http target must start with http:// : {url}"))?;
    Ok(rest.split('/').next().unwrap_or(rest).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_targets_matches_only_its_call() {
        let call = Uuid::now_v7();
        let other = Uuid::now_v7();
        let frame = format!(
            r#"{{"topic":"link","evt":"calls.voice","payload":{{"call_id":"{call}","action":"set_targets","characters":["{other}"]}}}}"#
        );
        assert!(is_set_targets(&frame, call));
        // Right shape, wrong call id.
        assert!(!is_set_targets(&frame, other));
    }

    #[test]
    fn clear_is_not_set_targets() {
        let call = Uuid::now_v7();
        let frame = format!(
            r#"{{"topic":"link","evt":"calls.voice","payload":{{"call_id":"{call}","action":"clear","characters":[]}}}}"#
        );
        assert!(!is_set_targets(&frame, call));
    }

    #[test]
    fn active_has_call_needs_active_state() {
        let call = Uuid::now_v7();
        let active: Vec<Value> = serde_json::from_str(&format!(
            r#"[{{"call_id":"{call}","kind":"voice","state":"active","participants":[]}}]"#
        ))
        .expect("parse");
        assert!(active_has_call(&active, call));

        // Ringing (not yet accepted) must not count as a recovered active call.
        let ringing: Vec<Value> = serde_json::from_str(&format!(
            r#"[{{"call_id":"{call}","kind":"voice","state":"ringing","participants":[]}}]"#
        ))
        .expect("parse");
        assert!(!active_has_call(&ringing, call));
        assert!(!active_has_call(&[], call));
    }

    #[test]
    fn link_url_swaps_ws_for_link() {
        assert_eq!(
            link_url("ws://127.0.0.1:8080/ws").expect("link url"),
            "ws://127.0.0.1:8080/link"
        );
        assert!(link_url("ws://127.0.0.1:8080/link").is_err());
    }

    #[test]
    fn host_strips_scheme_and_path() {
        assert_eq!(
            host_of_http("http://127.0.0.1:8080").expect("host"),
            "127.0.0.1:8080"
        );
        assert_eq!(
            host_of_http("http://127.0.0.1:8080/v1/x").expect("host"),
            "127.0.0.1:8080"
        );
        assert!(host_of_http("ws://x").is_err());
    }
}

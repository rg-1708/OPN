//! `--xinstance <http> <ws_a> <ws_b> [settle_secs]` (roadmap Sprint 9 item 3,
//! the `redis-restart` drill's cross-instance delivery checker).
//!
//! Two Core instances (A, B) share one Redis with `OPN_REPLICAS > 1`. A message
//! sent on A reaches a subscriber on B *only* via Redis pub/sub fan-out
//! (§3, §8) — A has no local subscriber for the channel, so nothing but the
//! `opn:*` PUBLISH → B's listener → `publish_local` chain can carry it across.
//! This checker mints two members of one channel, holds a *sender on A* and a
//! *subscriber on B* open, and proves the cross-instance hop once *before* the
//! drill restarts Redis and once *after* — the second proof is the "pub/sub
//! resubscribed" invariant. Holding both connections across the window also
//! keeps a `presence:*` key alive on each Core (share_presence defaults on), so
//! the drill can watch the presence refresher rebuild them (asserted in bash via
//! redis-cli).
//!
//! Exit 0 = both deliveries crossed; 1 = a delivery was lost; 2 = setup error.

use std::io::Write;
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use contracts::{Cmd, Evt, MessageBody, ServerMsg};
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::time::{sleep_until, timeout, Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use crate::driver::{await_ack, send};
use crate::http;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long to wait for the pushed message to arrive on B after A acks the send.
/// Generous: the cross-instance hop adds a Redis round trip, and post-restart the
/// listener may still be reconnecting when the settle window opens.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn verify_xinstance(
    http_target: &str,
    ws_a: &str,
    ws_b: &str,
    settle_secs: u64,
) -> Result<ExitCode> {
    let api_key = std::env::var("OPN_LOADGEN_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .context("OPN_LOADGEN_API_KEY must be set for --xinstance")?;
    let host = host_of_http(http_target)?;

    // Two members of one DM channel. Fresh refs — `stack_up` gives the drill a
    // clean DB, so fixed refs never collide across runs.
    let sender = http::mint(&host, &api_key, "xinst:sender")
        .await
        .context("mint sender (M1)")?;
    let subscriber = http::mint(&host, &api_key, "xinst:subscriber")
        .await
        .context("mint subscriber (M2)")?;

    // Sender lives on A; subscriber lives on B. The channel exists in the shared
    // Postgres, so B authorizes the sub against the same membership rows.
    let mut a = connect(ws_a).await.context("connect sender on A")?;
    auth(&mut a, &sender.token)
        .await
        .context("auth sender on A")?;
    let channel = open_direct(&mut a, &subscriber.number)
        .await
        .context("sender open_direct subscriber on A")?;

    let mut b = connect(ws_b).await.context("connect subscriber on B")?;
    auth(&mut b, &subscriber.token)
        .await
        .context("auth subscriber on B")?;
    subscribe(&mut b, channel)
        .await
        .context("subscriber sub channel on B")?;

    // ── PRE: the A→B hop must work before the fault ─────────────────────────
    if !deliver(&mut a, &mut b, channel, 10).await? {
        eprintln!("xinstance: FAIL — pre-restart message did not cross A→B");
        return Ok(ExitCode::from(1));
    }
    // The drill greps stdout for this before injecting the redis restart.
    println!("xinstance: PRE delivery OK");
    std::io::stdout().flush().ok();

    // ── HOLD: keep both sockets alive across the restart window ──────────────
    // Draining answers server pings (so neither connection is closed for missed
    // pongs) and keeps both characters online, so each Core's presence refresher
    // has a live character to rewrite `presence:*` for after Redis comes back.
    let until = Instant::now() + Duration::from_secs(settle_secs);
    drain_both(&mut a, &mut b, until)
        .await
        .context("holding connections across the fault")?;

    // ── POST: the same hop must work again — the resubscribe proof ───────────
    if !deliver(&mut a, &mut b, channel, 11).await? {
        eprintln!(
            "xinstance: FAIL — post-restart message did not cross A→B \
             (pub/sub did not resubscribe after the redis restart)"
        );
        return Ok(ExitCode::from(1));
    }
    println!("xinstance: POST delivery OK");

    eprintln!("xinstance: PASS — cross-instance delivery held before and after the fault");
    Ok(ExitCode::SUCCESS)
}

/// Send one nonce-tagged message on A, then assert it arrives on B (via Redis).
async fn deliver(a: &mut Ws, b: &mut Ws, channel: Uuid, send_id: u64) -> Result<bool> {
    let nonce = Uuid::now_v7().to_string();
    let body = MessageBody {
        text: Some("xinstance".into()),
        media_ids: None,
        gif_url: None,
        meta: Some(json!({ "nonce": nonce })),
    };
    send(
        a,
        send_id,
        Cmd::ChannelsSend {
            channel_id: channel,
            client_uuid: Uuid::now_v7(),
            body,
        },
    )
    .await?;
    let (ok, _) = await_ack(a, send_id).await?;
    if !ok {
        bail!("send ack not ok on A");
    }
    await_nonce(b, &nonce).await
}

/// Drain B until a `channels.message` carrying `nonce` arrives, or time out.
/// A timeout is a *lost delivery* (Ok(false)), not an operational error.
async fn await_nonce(b: &mut Ws, nonce: &str) -> Result<bool> {
    let found = timeout(DELIVERY_TIMEOUT, async {
        loop {
            match b.next().await {
                None => bail!("B stream closed while awaiting delivery"),
                Some(Err(e)) => bail!("B ws error awaiting delivery: {e}"),
                Some(Ok(Message::Text(t))) => {
                    if let Ok(ServerMsg::Push {
                        evt: Evt::ChannelsMessage { body, .. },
                        ..
                    }) = serde_json::from_str::<ServerMsg>(&t)
                    {
                        let got = body
                            .get("meta")
                            .and_then(|m| m.get("nonce"))
                            .and_then(|n| n.as_str());
                        if got == Some(nonce) {
                            return Ok(());
                        }
                    }
                }
                Some(Ok(Message::Close(_))) => bail!("B closed while awaiting delivery"),
                Some(Ok(_)) => {} // ping/pong/binary
            }
        }
    })
    .await;
    // Inner Err = the socket died (operational); a timeout = the message never
    // crossed (a lost delivery, the invariant break we are testing for).
    match found {
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(false),
    }
}

/// Keep both sockets serviced until `until` — discard every frame, but a close
/// or stream end is a connection that did not survive the fault (an error).
async fn drain_both(a: &mut Ws, b: &mut Ws, until: Instant) -> Result<()> {
    loop {
        tokio::select! {
            _ = sleep_until(until) => return Ok(()),
            f = a.next() => guard(f, "A")?,
            f = b.next() => guard(f, "B")?,
        }
    }
}

fn guard(
    frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    who: &str,
) -> Result<()> {
    match frame {
        None => bail!("connection {who} ended during the hold window"),
        Some(Err(e)) => bail!("connection {who} errored during the hold window: {e}"),
        Some(Ok(Message::Close(_))) => bail!("connection {who} closed during the hold window"),
        Some(Ok(_)) => Ok(()),
    }
}

async fn connect(url: &str) -> Result<Ws> {
    Ok(connect_async(url)
        .await
        .with_context(|| format!("ws connect {url}"))?
        .0)
}

async fn auth(ws: &mut Ws, token: &str) -> Result<()> {
    send(
        ws,
        1,
        Cmd::Auth {
            token: token.to_owned(),
        },
    )
    .await?;
    let (ok, _) = await_ack(ws, 1).await?;
    if !ok {
        bail!("auth ack not ok");
    }
    Ok(())
}

async fn open_direct(ws: &mut Ws, number: &str) -> Result<Uuid> {
    send(
        ws,
        2,
        Cmd::ChannelsOpenDirect {
            number: number.to_owned(),
        },
    )
    .await?;
    let (ok, payload) = await_ack(ws, 2).await?;
    if !ok {
        bail!("open_direct ack not ok");
    }
    let cid = payload
        .as_ref()
        .and_then(|p| p.get("channel_id"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow!("open_direct ack missing channel_id"))?;
    Uuid::parse_str(cid).context("channel_id not a uuid")
}

async fn subscribe(ws: &mut Ws, channel: Uuid) -> Result<()> {
    send(
        ws,
        3,
        Cmd::Sub {
            topic: format!("ch:{channel}"),
            last_seq: None,
        },
    )
    .await?;
    let (ok, _) = await_ack(ws, 3).await?;
    if !ok {
        bail!("sub ack not ok — token is not a member of {channel}");
    }
    Ok(())
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
    fn host_strips_scheme_and_path() {
        assert_eq!(
            host_of_http("http://127.0.0.1:8080").expect("host"),
            "127.0.0.1:8080"
        );
        assert_eq!(
            host_of_http("http://127.0.0.1:8081/v1/x").expect("host"),
            "127.0.0.1:8081"
        );
        assert!(host_of_http("ws://x").is_err());
    }

    // A close frame during the hold must be reported as a failure, never
    // silently drained — the whole point of the hold is that a redis restart
    // does not drop the WS connections.
    #[test]
    fn guard_flags_a_close() {
        assert!(guard(Some(Ok(Message::Close(None))), "B").is_err());
        assert!(guard(None, "A").is_err());
        assert!(guard(Some(Ok(Message::Text("x".into()))), "B").is_ok());
    }
}

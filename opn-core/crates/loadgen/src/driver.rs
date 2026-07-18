//! One WebSocket connection's lifecycle and measurement (roadmap Sprint 4
//! item 9: per-conn behavior script + ack-RTT / delivery-latency measurement).
//!
//! Connections are paired: the `Left` half of a pair `open_direct`s the
//! `Right` half's number, hands the resulting `channel_id` over a `oneshot`,
//! and both subscribe to `ch:<channel_id>`. Then both send at the scenario
//! rate. Every send embeds a monotonic timestamp (`body.meta.t`, microseconds
//! from a process-wide epoch) so the *peer* — same process, same clock — can
//! compute true cross-connection delivery latency when it receives the
//! `channels.message` push. Ack RTT is the send→ack round trip.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use contracts::{ClientFrame, Cmd, Evt, MessageBody, ServerMsg};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::{interval_at, timeout, Duration, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A durable-class slow-consumer close (§4.3). The one failure the perf smoke
/// treats as fatal — a healthy client must never be closed under design load.
const CLOSE_SLOW_CONSUMER: u16 = 4409;

/// Whether this connection opens the pair channel or waits to be told its id.
pub enum Pairing {
    Left {
        peer_number: String,
        tx: oneshot::Sender<Uuid>,
    },
    Right {
        rx: oneshot::Receiver<Uuid>,
    },
}

pub struct ConnConfig {
    pub ws_url: String,
    pub token: String,
    pub char_id: Uuid,
    pub pairing: Pairing,
    /// Shared process epoch — embedded timestamps and their reader use the same
    /// `Instant`, so a delivery-latency delta is clock-safe across tasks.
    pub epoch: Instant,
    /// All connections begin their send loop at this instant (a warmup barrier
    /// without the deadlock risk of `tokio::sync::Barrier`).
    pub start_at: Instant,
    pub send_deadline: Instant,
    pub read_deadline: Instant,
    pub period: Duration,
    pub typing_every: u64,
    pub read_every: u64,
}

/// Per-connection tallies, merged into the run summary. Latencies in
/// microseconds for precision; converted to ms only at report time.
#[derive(Default)]
pub struct ConnStats {
    pub ack_rtts_us: Vec<u64>,
    pub deliveries_us: Vec<u64>,
    pub sends: u64,
    pub recvs: u64,
    pub rate_limited: u64,
    pub durable_closes: u64,
    pub other_closes: u64,
    pub errors: u64,
    pub error_detail: Option<String>,
}

impl ConnStats {
    fn errored(detail: String) -> Self {
        ConnStats {
            errors: 1,
            error_detail: Some(detail),
            ..Default::default()
        }
    }
}

/// Drive one connection to completion. Never panics: any setup failure is
/// folded into `ConnStats::errored` so a single bad socket cannot abort the run
/// (or, via a `oneshot`, deadlock its partner — see `Left`/`Right` below).
pub async fn run_connection(cfg: ConnConfig) -> ConnStats {
    match drive(cfg).await {
        Ok(stats) => stats,
        Err(e) => ConnStats::errored(format!("{e:#}")),
    }
}

async fn drive(cfg: ConnConfig) -> Result<ConnStats> {
    let mut ws: Ws = connect_async(&cfg.ws_url)
        .await
        .with_context(|| format!("ws connect {}", cfg.ws_url))?
        .0;

    let mut next_id = 0u64;

    // ── auth ──────────────────────────────────────────────────────────────
    next_id += 1;
    let auth_id = next_id;
    send(
        &mut ws,
        auth_id,
        Cmd::Auth {
            token: cfg.token.clone(),
        },
    )
    .await?;
    let (ok, _) = await_ack(&mut ws, auth_id).await?;
    if !ok {
        bail!("auth ack not ok");
    }

    // ── establish the pair channel ────────────────────────────────────────
    // Left opens it and publishes the id; Right waits for it. If Left dies
    // before sending, its `tx` drops and Right's `rx.await` errors cleanly
    // instead of hanging.
    let channel_id = match cfg.pairing {
        Pairing::Left { peer_number, tx } => {
            next_id += 1;
            let od_id = next_id;
            send(
                &mut ws,
                od_id,
                Cmd::ChannelsOpenDirect {
                    number: peer_number,
                },
            )
            .await?;
            let (ok, payload) = await_ack(&mut ws, od_id).await?;
            if !ok {
                bail!("open_direct ack not ok");
            }
            let cid = payload
                .as_ref()
                .and_then(|p| p.get("channel_id"))
                .and_then(|c| c.as_str())
                .ok_or_else(|| anyhow!("open_direct ack missing channel_id"))?;
            let cid = Uuid::parse_str(cid).context("channel_id not a uuid")?;
            let _ = tx.send(cid); // Right may have died; not our problem
            cid
        }
        Pairing::Right { rx } => rx.await.context("pair partner never opened a channel")?,
    };

    // ── subscribe ─────────────────────────────────────────────────────────
    let topic = format!("ch:{channel_id}");
    next_id += 1;
    let sub_id = next_id;
    send(
        &mut ws,
        sub_id,
        Cmd::Sub {
            topic: topic.clone(),
            last_seq: None,
        },
    )
    .await?;
    let (ok, _) = await_ack(&mut ws, sub_id).await?;
    if !ok {
        bail!("sub ack not ok");
    }

    // ── drive ─────────────────────────────────────────────────────────────
    let (mut write, mut read) = ws.split();
    let mut stats = ConnStats::default();
    let mut pending: HashMap<u64, Instant> = HashMap::new();
    let mut last_seen_seq: i64 = 0;
    let mut send_count: u64 = 0;

    // First tick fires exactly at the shared `start_at`, so every connection
    // starts sending together regardless of how long its setup took.
    let mut tick = interval_at(cfg.start_at, cfg.period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;

            // Overall stop: after the send window plus a drain grace for
            // in-flight deliveries.
            _ = tokio::time::sleep_until(cfg.read_deadline) => break,

            // Send a message (only while inside the send window).
            _ = tick.tick(), if Instant::now() < cfg.send_deadline => {
                let mid = next_id + 1; next_id = mid;
                let t_us = cfg.epoch.elapsed().as_micros() as u64;
                let body = MessageBody {
                    text: Some("x".into()),
                    media_ids: None,
                    gif_url: None,
                    meta: Some(json!({ "t": t_us })),
                };
                let frame = ClientFrame {
                    id: mid,
                    cmd: Cmd::ChannelsSend {
                        channel_id,
                        client_uuid: Uuid::now_v7(),
                        body,
                    },
                };
                if write.send(text_frame(&frame)?).await.is_err() {
                    break; // socket gone; the read side will have logged the close
                }
                pending.insert(mid, Instant::now());
                stats.sends += 1;
                send_count += 1;

                // read/typing mix — deterministic, no rng dependency.
                if cfg.typing_every > 0 && send_count.is_multiple_of(cfg.typing_every) {
                    let tid = next_id + 1; next_id = tid;
                    let f = ClientFrame { id: tid, cmd: Cmd::ChannelsTyping { channel_id } };
                    let _ = write.send(text_frame(&f)?).await;
                }
                if cfg.read_every > 0 && send_count.is_multiple_of(cfg.read_every) && last_seen_seq > 0 {
                    let rid = next_id + 1; next_id = rid;
                    let f = ClientFrame {
                        id: rid,
                        cmd: Cmd::ChannelsMarkRead { channel_id, up_to_seq: last_seen_seq },
                    };
                    let _ = write.send(text_frame(&f)?).await;
                }
            }

            // Read acks and pushes.
            frame = read.next() => {
                match frame {
                    None | Some(Err(_)) => break,
                    Some(Ok(Message::Text(t))) => {
                        handle_incoming(&t, cfg.char_id, cfg.epoch, &mut pending, &mut last_seen_seq, &mut stats);
                    }
                    Some(Ok(Message::Close(c))) => {
                        match c.map(|f| u16::from(f.code)) {
                            Some(CLOSE_SLOW_CONSUMER) => stats.durable_closes += 1,
                            _ => stats.other_closes += 1,
                        }
                        break;
                    }
                    // Ping/Pong/Binary: tungstenite auto-queues pongs; ignore.
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    Ok(stats)
}

/// Parse one server frame and fold it into the running stats.
fn handle_incoming(
    text: &str,
    me: Uuid,
    epoch: Instant,
    pending: &mut HashMap<u64, Instant>,
    last_seen_seq: &mut i64,
    stats: &mut ConnStats,
) {
    let Ok(msg) = serde_json::from_str::<ServerMsg>(text) else {
        return; // unknown frame shape — not our contract's problem to measure
    };
    match msg {
        ServerMsg::Ack {
            reply_to, ok, err, ..
        } => {
            // Only our `channels.send` frames are tracked in `pending`; typing
            // and mark_read acks fall through here and are ignored.
            if let Some(sent) = pending.remove(&reply_to) {
                if ok {
                    stats.ack_rtts_us.push(sent.elapsed().as_micros() as u64);
                } else if matches!(err, Some(e) if matches!(e.code, contracts::ErrCode::RateLimited))
                {
                    stats.rate_limited += 1;
                }
            }
        }
        ServerMsg::Push {
            evt: Evt::ChannelsMessage {
                sender, seq, body, ..
            },
            ..
        } => {
            stats.recvs += 1;
            *last_seen_seq = (*last_seen_seq).max(seq);
            // Delivery latency only for the peer's messages — self fan-out would
            // measure the loopback path, not cross-connection delivery.
            if sender != me {
                if let Some(t_us) = body
                    .get("meta")
                    .and_then(|m| m.get("t"))
                    .and_then(|t| t.as_u64())
                {
                    let now_us = epoch.elapsed().as_micros() as u64;
                    stats.deliveries_us.push(now_us.saturating_sub(t_us));
                }
            }
        }
        // Other pushes (receipts, typing, presence) aren't measured here.
        ServerMsg::Push { .. } => {}
    }
}

fn text_frame(frame: &ClientFrame) -> Result<Message> {
    Ok(Message::Text(serde_json::to_string(frame)?.into()))
}

/// Send a `Cmd` as a `ClientFrame` on the combined (pre-split) stream.
async fn send(ws: &mut Ws, id: u64, cmd: Cmd) -> Result<()> {
    let frame = ClientFrame { id, cmd };
    ws.send(text_frame(&frame)?).await.context("ws send")?;
    Ok(())
}

/// Read frames until the ack with `reply_to == want`, skipping pushes and
/// unrelated acks. 5 s timeout — a missing setup ack is a hard error, not a
/// hang.
async fn await_ack(ws: &mut Ws, want: u64) -> Result<(bool, Option<serde_json::Value>)> {
    let fut = async {
        loop {
            match ws.next().await {
                None => bail!("stream closed awaiting ack {want}"),
                Some(Err(e)) => bail!("ws error awaiting ack {want}: {e}"),
                Some(Ok(Message::Text(t))) => {
                    if let Ok(ServerMsg::Ack {
                        reply_to,
                        ok,
                        payload,
                        ..
                    }) = serde_json::from_str::<ServerMsg>(&t)
                    {
                        if reply_to == want {
                            return Ok((ok, payload));
                        }
                    }
                }
                Some(Ok(Message::Close(_))) => bail!("closed awaiting ack {want}"),
                Some(Ok(_)) => {}
            }
        }
    };
    timeout(Duration::from_secs(5), fut)
        .await
        .context("timed out awaiting ack")?
}

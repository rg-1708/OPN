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
    /// Record every ok-acked send's seq into `ConnStats::acked_seqs` and carry
    /// the channel id + token (for the kill9 chaos verifier). Off in every
    /// perf/soak scenario — set only when `OPN_LOADGEN_ACK_JOURNAL` is present.
    pub record_acks: bool,
}

/// Per-connection tallies, merged into the run summary. Latencies in
/// microseconds for precision; converted to ms only at report time.
#[derive(Default)]
pub struct ConnStats {
    pub ack_rtts_us: Vec<u64>,
    pub deliveries_us: Vec<u64>,
    pub sends: u64,
    pub recvs: u64,
    /// Missing seqs in every subscribed channel's received `channels.message`
    /// stream (roadmap Sprint 10 test-plan deliverable: the delivery guarantee
    /// as a continuously-checked property). The guarantee is "no acked message
    /// is lost", NOT in-order arrival — post-commit fan-out (`channels/mod.rs`)
    /// is fire-and-forget, so two concurrent sends can reach a subscriber out of
    /// seq order (OPN.md §5: the client reorders by seq). So this counts *holes*
    /// in the received seq set, tolerating transient reorder (`SeqTracker`).
    /// Nonzero under design load ⇒ a genuinely lost message ⇒ a delivery bug.
    pub seq_gaps: u64,
    pub rate_limited: u64,
    /// Non-ok acks that are *not* rate-limits (internal/not_found/…). The
    /// pg-restart drill's "error acks, not silence" signal: while its DB pool
    /// can't reach Postgres, Core still acks each send an `internal` rather than
    /// hanging, so a non-zero count is the proof the gap was answered, not silent.
    pub error_acks: u64,
    pub durable_closes: u64,
    pub other_closes: u64,
    pub errors: u64,
    pub error_detail: Option<String>,
    /// Seqs of every ok-acked `channels.send` on this connection — the kill9
    /// chaos verifier's ground truth (roadmap Sprint 9 item 3). Only populated
    /// when `ConnConfig::record_acks` is set, so the normal perf smoke and the
    /// Sprint 10 soak pay nothing for it. Survives a mid-run disconnect because
    /// the drive loop `break`s (not errors) on a dead socket, returning
    /// `Ok(stats)` with the acks collected before the kill.
    pub acked_seqs: Vec<i64>,
    /// This connection's pair channel, set once setup completes.
    pub channel_id: Option<Uuid>,
    /// A member token for the channel (recording mode only), so the verifier can
    /// resume-subscribe as a real member.
    pub token: Option<String>,
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

/// Per-channel seq-continuity tracker. Feeds every received `channels.message`
/// seq for one channel and, at the end of the run, reports how many seqs are
/// *missing* from the contiguous run `[first_seen .. max_seen]`.
///
/// It is deliberately order-insensitive: fan-out is post-commit and
/// fire-and-forget (`channels/mod.rs`), so seqs 1 and 2 from two concurrent
/// senders can arrive as `2, 1`. `frontier` is the highest seq below which the
/// stream is fully contiguous; anything above a hole waits in `pending` until
/// the hole fills. The reorder window is microseconds-to-single-digit-ms (bound
/// by command latency), while the run's 2 s drain grace dwarfs it, so at run end
/// `pending` is empty unless a seq was *genuinely* lost. Memory is
/// O(reorder-window), not O(messages) — soak-safe for the 24 h scenario.
#[derive(Default)]
pub struct SeqTracker {
    /// Highest seq S with every seq in `[first_seen ..= S]` received. `None`
    /// until the first message.
    frontier: Option<i64>,
    /// Received seqs above a not-yet-filled hole (i.e. `> frontier + 1`).
    pending: std::collections::BTreeSet<i64>,
}

impl SeqTracker {
    fn observe(&mut self, seq: i64) {
        match self.frontier {
            None => self.frontier = Some(seq),
            // Duplicate or a resent seq at/below the frontier — not a gap.
            Some(f) if seq <= f => {}
            // Fills the next hole: advance, then absorb any pending run behind it.
            Some(f) if seq == f + 1 => {
                let mut nf = seq;
                while self.pending.remove(&(nf + 1)) {
                    nf += 1;
                }
                self.frontier = Some(nf);
            }
            // Arrived ahead of a still-open hole — hold it.
            Some(_) => {
                self.pending.insert(seq);
            }
        }
    }

    /// Missing seqs in `[first_seen .. max_seen]` given the settled state.
    fn gaps(&self) -> u64 {
        match self.frontier {
            None => 0,
            Some(f) => {
                let max = self.pending.iter().next_back().copied().unwrap_or(f);
                // `[f+1 ..= max]` has `max - f` slots; `pending` fills its own
                // count of them; the rest (at least seq `f+1`) are real holes.
                (max - f).max(0) as u64 - self.pending.len() as u64
            }
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
    let record_acks = cfg.record_acks;
    let (mut write, mut read) = ws.split();
    let mut stats = ConnStats {
        channel_id: Some(channel_id),
        token: record_acks.then(|| cfg.token.clone()),
        ..Default::default()
    };
    let mut pending: HashMap<u64, Instant> = HashMap::new();
    let mut last_seen_seq: i64 = 0;
    // One seq-continuity tracker per subscribed channel. This connection subs a
    // single `ch:<id>`, so the map holds one entry; keying by channel id keeps
    // it correct if a future scenario subscribes to several (hot-channel groups).
    let mut trackers: HashMap<Uuid, SeqTracker> = HashMap::new();
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
                        handle_incoming(&t, cfg.char_id, cfg.epoch, &mut pending, &mut last_seen_seq, &mut trackers, &mut stats, record_acks);
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

    // Collapse per-channel trackers into the one summary counter now the stream
    // has drained (out-of-order seqs have had the drain grace to settle).
    stats.seq_gaps = trackers.values().map(SeqTracker::gaps).sum();

    Ok(stats)
}

/// Parse one server frame and fold it into the running stats.
#[allow(clippy::too_many_arguments)]
fn handle_incoming(
    text: &str,
    me: Uuid,
    epoch: Instant,
    pending: &mut HashMap<u64, Instant>,
    last_seen_seq: &mut i64,
    trackers: &mut HashMap<Uuid, SeqTracker>,
    stats: &mut ConnStats,
    record_acks: bool,
) {
    let Ok(msg) = serde_json::from_str::<ServerMsg>(text) else {
        return; // unknown frame shape — not our contract's problem to measure
    };
    match msg {
        ServerMsg::Ack {
            reply_to,
            ok,
            payload,
            err,
        } => {
            // Only our `channels.send` frames are tracked in `pending`; typing
            // and mark_read acks fall through here and are ignored.
            if let Some(sent) = pending.remove(&reply_to) {
                if ok {
                    stats.ack_rtts_us.push(sent.elapsed().as_micros() as u64);
                    // Persist-then-ack means an ok ack ⇒ the row is committed;
                    // record its seq as the kill9 verifier's must-survive set.
                    if record_acks {
                        if let Some(seq) = payload
                            .as_ref()
                            .and_then(|p| p.get("seq"))
                            .and_then(|s| s.as_i64())
                        {
                            stats.acked_seqs.push(seq);
                        }
                    }
                } else if matches!(err, Some(e) if matches!(e.code, contracts::ErrCode::RateLimited))
                {
                    stats.rate_limited += 1;
                } else {
                    // Any other non-ok ack (internal, not_found, …). During the
                    // pg-restart drill these are the DB-outage acks — Core
                    // answering rather than going silent.
                    stats.error_acks += 1;
                }
            }
        }
        ServerMsg::Push {
            evt:
                Evt::ChannelsMessage {
                    channel_id,
                    sender,
                    seq,
                    body,
                    ..
                },
            ..
        } => {
            stats.recvs += 1;
            *last_seen_seq = (*last_seen_seq).max(seq);
            // Track continuity per channel (order-insensitive; see `SeqTracker`).
            trackers.entry(channel_id).or_default().observe(seq);
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

/// Send a `Cmd` as a `ClientFrame` on the combined (pre-split) stream. Shared
/// with the `--verify-resume` path (`verify.rs`).
pub(crate) async fn send(ws: &mut Ws, id: u64, cmd: Cmd) -> Result<()> {
    let frame = ClientFrame { id, cmd };
    ws.send(text_frame(&frame)?).await.context("ws send")?;
    Ok(())
}

/// Read frames until the ack with `reply_to == want`, skipping pushes and
/// unrelated acks. 5 s timeout — a missing setup ack is a hard error, not a
/// hang. Shared with the `--verify-resume` path (`verify.rs`).
pub(crate) async fn await_ack(ws: &mut Ws, want: u64) -> Result<(bool, Option<serde_json::Value>)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed one server frame (with `reply_to` 7 pre-registered as a tracked
    /// send) through the classifier and return the resulting tallies.
    fn classify(text: &str) -> ConnStats {
        let mut pending = HashMap::new();
        pending.insert(7, Instant::now());
        let mut stats = ConnStats::default();
        let mut seen = 0i64;
        let mut trackers = HashMap::new();
        handle_incoming(
            text,
            Uuid::nil(),
            Instant::now(),
            &mut pending,
            &mut seen,
            &mut trackers,
            &mut stats,
            false,
        );
        stats
    }

    /// Final gap count after feeding `seqs` (in arrival order) to one tracker.
    fn gaps_of(seqs: &[i64]) -> u64 {
        let mut t = SeqTracker::default();
        for &s in seqs {
            t.observe(s);
        }
        t.gaps()
    }

    #[test]
    fn seq_in_order_no_gaps() {
        assert_eq!(gaps_of(&[4, 5, 6, 7]), 0); // first-seen need not be 1
    }

    #[test]
    fn seq_out_of_order_that_resolves_is_no_gap() {
        // The real fan-out reorder case: 2 arrives before 1, but both arrive.
        assert_eq!(gaps_of(&[1, 3, 2, 4]), 0);
        assert_eq!(gaps_of(&[5, 4, 6, 8, 7]), 0);
    }

    #[test]
    fn seq_genuine_hole_is_a_gap() {
        assert_eq!(gaps_of(&[1, 2, 4]), 1); // 3 lost
        assert_eq!(gaps_of(&[1, 2, 5]), 2); // 3 and 4 lost
        assert_eq!(gaps_of(&[10, 11, 13, 14, 17]), 3); // 12, 15, 16 lost
    }

    #[test]
    fn seq_duplicate_is_not_a_gap() {
        assert_eq!(gaps_of(&[1, 2, 2, 3]), 0);
        assert_eq!(gaps_of(&[3, 1, 2, 1]), 0); // late dup below frontier ignored
    }

    #[test]
    fn seq_gaps_surface_through_handle_incoming() {
        // Two messages on one channel with seq 1 then 3 → one lost (seq 2).
        // Built from the real wire types (not hand-written JSON) so the test
        // rides the actual `channels.message` serialization, not a guess at it.
        let cid = Uuid::now_v7();
        let mut pending = HashMap::new();
        let mut stats = ConnStats::default();
        let mut seen = 0i64;
        let mut trackers = HashMap::new();
        for seq in [1, 3] {
            let msg = ServerMsg::Push {
                topic: format!("ch:{cid}"),
                evt: Evt::ChannelsMessage {
                    channel_id: cid,
                    message_id: Uuid::now_v7(),
                    seq,
                    sender: Uuid::nil(),
                    body: json!({}),
                    at: "now".into(),
                },
            };
            let text = serde_json::to_string(&msg).expect("serialize push");
            handle_incoming(
                &text,
                Uuid::nil(),
                Instant::now(),
                &mut pending,
                &mut seen,
                &mut trackers,
                &mut stats,
                false,
            );
        }
        assert_eq!(stats.recvs, 2);
        assert_eq!(trackers.values().map(SeqTracker::gaps).sum::<u64>(), 1);
    }

    #[test]
    fn internal_ack_is_an_error_ack_not_a_ratelimit() {
        // The pg-restart gap's signal: a DB-outage `internal` ack.
        let s = classify(r#"{"reply_to":7,"ok":false,"err":{"code":"internal","msg":"x"}}"#);
        assert_eq!(s.error_acks, 1);
        assert_eq!(s.rate_limited, 0);
        assert!(s.ack_rtts_us.is_empty());
    }

    #[test]
    fn rate_limited_ack_is_not_an_error_ack() {
        let s = classify(r#"{"reply_to":7,"ok":false,"err":{"code":"rate_limited","msg":"x"}}"#);
        assert_eq!(s.rate_limited, 1);
        assert_eq!(s.error_acks, 0);
    }

    #[test]
    fn ok_ack_is_neither() {
        let s = classify(r#"{"reply_to":7,"ok":true,"payload":{"seq":3}}"#);
        assert_eq!(s.error_acks, 0);
        assert_eq!(s.rate_limited, 0);
        assert_eq!(s.ack_rtts_us.len(), 1);
    }
}

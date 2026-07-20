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
use hdrhistogram::Histogram;
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
/// Shared with the call-churn link consumer (`callchurn.rs`), which treats a
/// 4409 on the tenant `/link` as the link-under-load failure.
pub(crate) const CLOSE_SLOW_CONSUMER: u16 = 4409;

/// hot-channel group setup: `channels.create`'s member list is capped at this in
/// Core (`MEMBERS_MAX`, §10.2). A larger group is built by `member_add`ing the
/// remainder (which has no total cap). Mirror of the core constant so the creator
/// splits its list exactly the way Core will accept it.
const GROUP_CREATE_MAX: usize = 32;

/// Pace between `channels.member_add`s during group setup. member_add is the
/// Social rate class (5/s sustained, burst 20, §12); one add per 200 ms sits
/// exactly at the sustained rate, so even a 100-member group builds without ever
/// tripping a rate-limit ack — the burst headroom absorbs scheduling jitter.
const MEMBER_ADD_PERIOD: Duration = Duration::from_millis(200);

/// How this connection obtains the channel every message flows through. `Left`
/// opens a pair thread (`open_direct`) and publishes its id; `Right` waits to be
/// told an id and joins — used both as a pair's other half and as a group member
/// (a member does exactly the same thing: wait for the id, sub, send).
/// `GroupCreator` (hot-channel, roadmap Sprint 10 item 1) creates one group
/// channel, adds every member, and broadcasts the id to all of them.
pub enum Pairing {
    Left {
        peer_number: String,
        tx: oneshot::Sender<Uuid>,
    },
    Right {
        rx: oneshot::Receiver<Uuid>,
    },
    /// hot-channel: create a group, `member_add` everyone, then hand the channel
    /// id to every member's `oneshot` — the fan-out topology (one channel, N
    /// subscribers). `member_ids` are the char ids to add; `txs` the one sender
    /// per member.
    GroupCreator {
        member_ids: Vec<Uuid>,
        txs: Vec<oneshot::Sender<Uuid>>,
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
    /// reconnect-storm (roadmap Sprint 10 item 1): the instant this connection
    /// drops its socket and reconnects. `None` (every other scenario) ⇒ exactly
    /// one connection epoch, byte-identical to the pre-storm behavior.
    pub reconnect_at: Option<Instant>,
    /// Per-connection stagger before the reconnect — the 0–3 s jitter of
    /// OPN.md §7's thundering herd, spread deterministically across connections
    /// in `main` (no rng dependency). Ignored when `reconnect_at` is `None`.
    pub reconnect_delay: Duration,
    /// Record every ok-acked send's seq into `ConnStats::acked_seqs` and carry
    /// the channel id + token (for the kill9 chaos verifier). Off in every
    /// perf/soak scenario — set only when `OPN_LOADGEN_ACK_JOURNAL` is present.
    pub record_acks: bool,
}

/// A fresh latency histogram: 1 µs .. 60 s range, 3 significant figures. Fixed
/// memory regardless of sample count (the whole point vs the old unbounded Vec)
/// — soak-safe. Bounds are static, so the construction is infallible; `expect`
/// documents that (it is not an `unwrap` on request-path data).
pub(crate) fn new_hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 60_000_000, 3).expect("static histogram bounds are valid")
}

/// Per-connection tallies, merged into the run summary. Latencies recorded in
/// microseconds into fixed-memory `hdrhistogram`s; converted to ms at report
/// time. `Default` is hand-written (not derived) because `Histogram` has no
/// `Default` — every construction goes through `new_hist`.
pub struct ConnStats {
    pub ack: Histogram<u64>,
    pub delivery: Histogram<u64>,
    /// Resume round-trip (reconnect-storm only): time from re-subscribe to the
    /// sub ack after a storm reconnect — i.e. how long the client waited for its
    /// gap replay to drain. Empty (n=0) in every non-storm scenario. The roadmap
    /// item-6 "resumed within 60 s" target is *measured* here, not hard-gated —
    /// item 7 tightens perf thresholds after the fix loop.
    pub resume: Histogram<u64>,
    /// How many times this connection performed a storm reconnect (0 or 1 with
    /// today's single-storm scenario). The non-vacuity signal: a reconnect-storm
    /// run that reports 0 total reconnects never fired its storm — `assert_reconnected`
    /// turns that into a failure so the drill can't pass without doing its job.
    pub reconnects: u64,
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
    /// `calls.voice` `set_targets` events received on the tenant `/link` (the
    /// call-churn driver's one link consumer increments this; every other
    /// connection leaves it 0). The non-vacuity proof that the link relay
    /// actually fired under load — `assert_calls` fails a run that completed
    /// calls but never delivered a target. Zero in every non-calls scenario.
    pub set_targets: u64,
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

impl Default for ConnStats {
    fn default() -> Self {
        ConnStats {
            ack: new_hist(),
            delivery: new_hist(),
            resume: new_hist(),
            reconnects: 0,
            sends: 0,
            recvs: 0,
            seq_gaps: 0,
            set_targets: 0,
            rate_limited: 0,
            error_acks: 0,
            durable_closes: 0,
            other_closes: 0,
            errors: 0,
            error_detail: None,
            acked_seqs: Vec::new(),
            channel_id: None,
            token: None,
        }
    }
}

impl ConnStats {
    pub(crate) fn errored(detail: String) -> Self {
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

    /// Highest seq below which the stream is fully contiguous — the resume
    /// watermark. Resuming a `sub` from this replays everything after it,
    /// including any seq stuck in `pending` above an open hole (so a pre-storm
    /// hole is re-requested, not skipped). `None` before the first message ⇒
    /// resume from 0 (replay all). Order-insensitive by construction (see
    /// `observe`), so it is safe against fan-out reorder.
    pub(crate) fn frontier(&self) -> Option<i64> {
        self.frontier
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

/// Why an epoch's send/read loop stopped.
enum EpochOutcome {
    /// The run's read deadline (or a socket close) — the connection is finished.
    Done,
    /// A reconnect-storm hit its `reconnect_at` instant: tear down this socket,
    /// stagger, reconnect, and resume in the next epoch.
    Reconnect,
}

/// Open a socket and complete the `auth` handshake, returning the live stream.
/// Shared by initial setup and every storm reconnect — the reconnected socket
/// re-auths with the same (still-valid, 10-min JWT) session token, so no re-mint
/// is needed for a resume within token lifetime. Auth is always frame id 1 on a
/// fresh socket (reply_to is matched per-socket, so id reuse across sockets is
/// fine).
async fn connect_and_auth(ws_url: &str, token: &str) -> Result<Ws> {
    let mut ws: Ws = connect_async(ws_url)
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

async fn drive(cfg: ConnConfig) -> Result<ConnStats> {
    let mut ws = connect_and_auth(&cfg.ws_url, &cfg.token).await?;
    let mut next_id = 1u64; // auth consumed id 1

    // ── establish the channel (once) ──────────────────────────────────────
    // Left opens a pair thread and publishes its id; Right (a pair half OR a
    // group member) waits for one. GroupCreator creates one group and adds
    // everyone before releasing the id. If the opener dies before sending, its
    // `tx`/`txs` drop and every waiting `rx.await` errors cleanly instead of
    // hanging. A storm reconnect keeps this channel — only the subscription is
    // re-established, never the channel.
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
        Pairing::GroupCreator { member_ids, txs } => {
            // Create the group with the first up-to-cap members (creator is
            // auto-added by Core, §10.2); the rest are added below. `member_add`
            // has no total cap, so this is how the group grows past 33.
            let (in_create, to_add) = split_group_members(&member_ids, GROUP_CREATE_MAX);
            next_id += 1;
            let cr_id = next_id;
            send(
                &mut ws,
                cr_id,
                Cmd::ChannelsCreate {
                    name: Some("hot-channel".into()),
                    members: in_create.to_vec(),
                },
            )
            .await?;
            let (ok, payload) = await_ack(&mut ws, cr_id).await?;
            if !ok {
                bail!("channels.create ack not ok");
            }
            let cid = payload
                .as_ref()
                .and_then(|p| p.get("channel_id"))
                .and_then(|c| c.as_str())
                .ok_or_else(|| anyhow!("create ack missing channel_id"))?;
            let cid = Uuid::parse_str(cid).context("channel_id not a uuid")?;

            // Add the remainder, paced under the Social budget so no add is ever
            // rate-limited. Every target must be a real member BEFORE it is told
            // the channel id (so its `sub` authorizes), which is exactly why the
            // adds all complete before the broadcast below.
            for m in to_add {
                tokio::time::sleep(MEMBER_ADD_PERIOD).await;
                next_id += 1;
                let add_id = next_id;
                send(
                    &mut ws,
                    add_id,
                    Cmd::ChannelsMemberAdd {
                        channel_id: cid,
                        character_id: *m,
                    },
                )
                .await?;
                let (ok, _) = await_ack(&mut ws, add_id).await?;
                if !ok {
                    bail!("channels.member_add ack not ok");
                }
            }

            // Everyone is a member now — release the id to every member task.
            for tx in txs {
                let _ = tx.send(cid); // a member task may have died; not our problem
            }
            cid
        }
        Pairing::Right { rx } => rx.await.context("channel opener never published an id")?,
    };

    let topic = format!("ch:{channel_id}");
    let record_acks = cfg.record_acks;
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
    // The tracker persists ACROSS a storm reconnect — that is exactly what makes
    // `seq_gaps` a per-client continuity check *through* the reconnect.
    let mut trackers: HashMap<Uuid, SeqTracker> = HashMap::new();
    let mut send_count: u64 = 0;

    // First tick fires at this connection's `start_at` — the shared warmup
    // barrier plus a per-conn phase stagger (see main.rs), so the aggregate is
    // a uniform stream, not one aligned burst per period. It persists across
    // epochs; `MissedTickBehavior::Delay` means the ticks missed while a
    // connection was offline don't burst on reconnect.
    let mut tick = interval_at(cfg.start_at, cfg.period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut reconnected = false;

    // Epoch loop: exactly one pass for every non-storm scenario (`reconnect_at`
    // is `None`); reconnect-storm adds a second pass after the storm. On the
    // reconnect pass the sub carries `last_seq = frontier`, so the server replays
    // the gap and the tracker sees a continuous stream — a genuinely lost message
    // shows as a hole in `seq_gaps`.
    loop {
        // ── (re)subscribe; drain any resume replay into the trackers ──────
        next_id += 1;
        let sub_id = next_id;
        let last_seq = reconnected.then(|| {
            trackers
                .get(&channel_id)
                .and_then(SeqTracker::frontier)
                .unwrap_or(0)
        });
        let resume_started = Instant::now();
        send(
            &mut ws,
            sub_id,
            Cmd::Sub {
                topic: topic.clone(),
                last_seq,
            },
        )
        .await?;
        // Resume replay arrives as `channels.message` pushes *before* the sub ack
        // (§4.4), so a plain `await_ack` (which skips pushes) would discard the
        // gap and manufacture false holes. `drain_sub` feeds those pushes to the
        // trackers instead — and skips delivery-latency recording for them, since
        // a replayed message's embedded timestamp is stale (pre-storm) and would
        // pollute the live-delivery histogram with reconnect-gap-sized samples.
        let sub_ok = drain_sub(
            &mut ws,
            sub_id,
            cfg.char_id,
            cfg.epoch,
            &mut pending,
            &mut last_seen_seq,
            &mut trackers,
            &mut stats,
        )
        .await?;
        if !sub_ok {
            bail!("sub ack not ok");
        }
        if reconnected {
            stats
                .resume
                .saturating_record(resume_started.elapsed().as_micros() as u64);
            stats.reconnects += 1;
        }

        // ── drive this epoch ──────────────────────────────────────────────
        // The inner loop is an expression: each terminal arm `break`s the
        // outcome, non-terminal arms (send, ping) fall through and keep looping.
        let (mut write, mut read) = ws.split();
        let outcome = loop {
            tokio::select! {
                biased;

                // Overall stop: after the send window plus a drain grace for
                // in-flight deliveries.
                _ = tokio::time::sleep_until(cfg.read_deadline) => break EpochOutcome::Done,

                // Storm trigger: fire once, only when configured and not yet
                // reconnected. When `reconnect_at` is `None` the guard disables
                // this arm entirely (the `unwrap_or` value is then never awaited).
                _ = tokio::time::sleep_until(cfg.reconnect_at.unwrap_or(cfg.read_deadline)),
                    if cfg.reconnect_at.is_some() && !reconnected =>
                break EpochOutcome::Reconnect,

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
                        break EpochOutcome::Done; // socket gone
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
                        None | Some(Err(_)) => break EpochOutcome::Done,
                        Some(Ok(Message::Text(t))) => {
                            handle_incoming(&t, cfg.char_id, cfg.epoch, &mut pending, &mut last_seen_seq, &mut trackers, &mut stats, record_acks, true);
                        }
                        Some(Ok(Message::Close(c))) => {
                            match c.map(|f| u16::from(f.code)) {
                                Some(CLOSE_SLOW_CONSUMER) => stats.durable_closes += 1,
                                _ => stats.other_closes += 1,
                            }
                            break EpochOutcome::Done;
                        }
                        // Ping/Pong/Binary: tungstenite auto-queues pongs; ignore.
                        Some(Ok(_)) => {}
                    }
                }
            }
        };

        match outcome {
            EpochOutcome::Done => break,
            EpochOutcome::Reconnect => {
                // Hard drop (dropping the split halves closes the TCP socket) —
                // the realistic resource/network failure the storm models, no
                // graceful close frame. Then stagger, reconnect, and loop back to
                // the re-subscribe above.
                drop(write);
                drop(read);
                tokio::time::sleep(cfg.reconnect_delay).await;
                // In-flight sends on the dead socket will never ack — drop their
                // RTT samples. Any that committed are still replayed on resume, so
                // the seq-continuity check is unaffected.
                pending.clear();
                ws = connect_and_auth(&cfg.ws_url, &cfg.token).await?;
                reconnected = true;
            }
        }
    }

    // Collapse per-channel trackers into the one summary counter now the stream
    // has drained (out-of-order seqs have had the drain grace to settle).
    stats.seq_gaps = trackers.values().map(SeqTracker::gaps).sum();

    Ok(stats)
}

/// Send a `sub` and drain frames until its ack, feeding any resume-replay
/// `channels.message` pushes (which arrive *before* the sub ack, §4.4) to the
/// seq trackers so continuity is checked *through* a reconnect. Returns the ack's
/// `ok`. Replayed messages are recorded with `record_delivery = false`: their
/// timestamps are pre-storm and would otherwise inflate the live-delivery
/// histogram by the whole reconnect gap. 15 s timeout — a missing sub ack is a
/// hard error, not a hang (same bound as the kill9 resume verifier).
#[allow(clippy::too_many_arguments)]
async fn drain_sub(
    ws: &mut Ws,
    sub_id: u64,
    me: Uuid,
    epoch: Instant,
    pending: &mut HashMap<u64, Instant>,
    last_seen_seq: &mut i64,
    trackers: &mut HashMap<Uuid, SeqTracker>,
    stats: &mut ConnStats,
) -> Result<bool> {
    let fut = async {
        loop {
            match ws.next().await {
                None => bail!("stream closed during sub {sub_id}"),
                Some(Err(e)) => bail!("ws error during sub {sub_id}: {e}"),
                Some(Ok(Message::Text(t))) => {
                    // The sub ack ends the replay window; everything before it is
                    // a replayed push to fold into the trackers.
                    if let Ok(ServerMsg::Ack { reply_to, ok, .. }) =
                        serde_json::from_str::<ServerMsg>(&t)
                    {
                        if reply_to == sub_id {
                            return Ok(ok);
                        }
                    }
                    handle_incoming(
                        &t,
                        me,
                        epoch,
                        pending,
                        last_seen_seq,
                        trackers,
                        stats,
                        false,
                        false,
                    );
                }
                Some(Ok(Message::Close(_))) => bail!("closed during sub {sub_id}"),
                Some(Ok(_)) => {}
            }
        }
    };
    timeout(Duration::from_secs(15), fut)
        .await
        .context("timed out awaiting sub ack")?
}

/// Parse one server frame and fold it into the running stats. `record_delivery`
/// is false only while draining a resume replay (`drain_sub`): those messages
/// carry stale pre-storm timestamps, so their "latency" is the reconnect gap,
/// not a live delivery — counting them would poison the delivery histogram.
/// Continuity (`SeqTracker`) and `recvs` are still recorded, since a replayed
/// seq closing a gap is exactly what proves no message was lost.
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
    record_delivery: bool,
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
                    stats
                        .ack
                        .saturating_record(sent.elapsed().as_micros() as u64);
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
            // Delivery latency only for the peer's *live* messages — self
            // fan-out measures the loopback path, and a replayed message
            // (`record_delivery = false`) measures the reconnect gap, not delivery.
            if record_delivery && sender != me {
                if let Some(t_us) = body
                    .get("meta")
                    .and_then(|m| m.get("t"))
                    .and_then(|t| t.as_u64())
                {
                    let now_us = epoch.elapsed().as_micros() as u64;
                    stats
                        .delivery
                        .saturating_record(now_us.saturating_sub(t_us));
                }
            }
        }
        // Other pushes (receipts, typing, presence) aren't measured here.
        ServerMsg::Push { .. } => {}
    }
}

/// Split a group's member ids into the initial `channels.create` list (capped at
/// `create_max` = Core's `MEMBERS_MAX`) and the remainder to `member_add`. Pure,
/// so the boundary (empty / under / exactly / over the cap) is unit-tested.
fn split_group_members(ids: &[Uuid], create_max: usize) -> (&[Uuid], &[Uuid]) {
    ids.split_at(ids.len().min(create_max))
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
            true,
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

    /// Final `frontier()` after feeding `seqs` — the reconnect resume watermark.
    fn frontier_of(seqs: &[i64]) -> Option<i64> {
        let mut t = SeqTracker::default();
        for &s in seqs {
            t.observe(s);
        }
        t.frontier()
    }

    #[test]
    fn frontier_is_the_resume_watermark() {
        assert_eq!(frontier_of(&[]), None); // nothing seen → resume from 0
        assert_eq!(frontier_of(&[1, 2, 3]), Some(3)); // fully contiguous
        assert_eq!(frontier_of(&[5, 6, 7]), Some(7)); // first-seen need not be 1
                                                      // A pre-storm hole (2 missing) pins the frontier below it, so resuming
                                                      // from `frontier` re-requests seq 2 — the hole is refilled, not skipped.
        assert_eq!(frontier_of(&[1, 3]), Some(1));
        // Out-of-order that resolves advances the frontier past the whole run.
        assert_eq!(frontier_of(&[1, 3, 2, 4]), Some(4));
    }

    #[test]
    fn group_members_split_at_the_create_cap() {
        let ids: Vec<Uuid> = (0..40).map(|_| Uuid::now_v7()).collect();
        // empty group → nothing to create, nothing to add.
        let (c, a) = split_group_members(&[], 32);
        assert!(c.is_empty() && a.is_empty());
        // under the cap → all go in the create, none added.
        let (c, a) = split_group_members(&ids[..10], 32);
        assert_eq!((c.len(), a.len()), (10, 0));
        // exactly the cap → still one create, no adds.
        let (c, a) = split_group_members(&ids[..32], 32);
        assert_eq!((c.len(), a.len()), (32, 0));
        // over the cap → first 32 created, the rest member_add'd.
        let (c, a) = split_group_members(&ids, 32);
        assert_eq!((c.len(), a.len()), (32, 8));
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
                true,
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
        assert_eq!(s.ack.len(), 0);
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
        assert_eq!(s.ack.len(), 1);
    }
}

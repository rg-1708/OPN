//! call-churn driver mode (roadmap Sprint 10 item 1): "50 concurrent calls
//! starting/ending at 1 Hz with signaling — exercises the call FSM + the /link
//! relay under load."
//!
//! Topology: N caller/callee **pairs**, one task per pair owning BOTH sockets
//! (the linkdrop pattern), plus ONE shared tenant `/link` consumer. Each pair
//! runs a full call lifecycle per `calls_per_sec` tick — `calls.start` →
//! `calls.accept` → `calls.signal` both directions → both `calls.hangup` — so
//! 50 pairs at 1 Hz churn the FSM at ~50 calls/s, and every accept/hangup emits
//! a `set_targets`/`clear` the single link consumer must drain without a
//! slow-consumer close. That link is the fan-out-under-load target; the pairs
//! are the FSM-under-load target.
//!
//! Reuse: the call frames (`connect_auth`, `connect_link`, `start_call`,
//! `accept_call`, `link_url`) come straight from `linkdrop.rs` — the `--link-drop`
//! chaos checker already speaks them. Only `calls.signal`/`calls.hangup` and the
//! churn loop are new here.
//!
//! Two deliberate cuts (ponytail, documented so the next reader keeps them):
//!  - **The parties do not `sub call:<id>`.** Signals are sent and ok-acked (the
//!    relay authz + publish path is exercised every round), but subscribing each
//!    party to every round's fresh `call:<id>` would accumulate hundreds of dead
//!    topic subs per connection in the registry over a run (parties never unsub
//!    a call that has Ended) and add a per-round socket-drain burden — for no
//!    load coverage the link (the real fan-out target) doesn't already give.
//!  - **The callee is told the `call_id` in-process, not via a notify ring sub.**
//!    Both sockets live in one task, so `start_call` returns the id directly.
//!    Core still *emits* the ring (`notify::route(class=ring)` runs on every
//!    start); it simply has no local subscriber to deliver to — the emit path is
//!    under load, the consume path (a device's notify sub) is not this scenario's
//!    concern.

use anyhow::{bail, Context, Result};
use contracts::{Cmd, Evt, ServerMsg, VoiceAction};
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::time::{interval_at, Duration, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use crate::driver::{await_ack, send, ConnStats, CLOSE_SLOW_CONSUMER};
use crate::http::Minted;
use crate::linkdrop::{accept_call, connect_auth, connect_link, link_url, start_call};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Opaque WebRTC-shaped signal payload (≤ 16 KB, never inspected by Core). A
/// tiny fixed blob — the relay path, not payload size, is what this stresses.
fn signal_payload() -> serde_json::Value {
    json!({ "sdp": "call-churn" })
}

/// Spawn the whole call-churn population and collect every task's stats. One
/// `/link` consumer plus one task per caller/callee pair; the odd session (if
/// `sessions.len()` is somehow odd) is dropped, matching the paired-DM path.
/// Returns the same `Vec<Result<ConnStats, JoinError>>` the message path hands
/// to `Summary::merge`, so reporting/gating is identical.
pub(crate) async fn run_calls(
    sessions: Vec<Minted>,
    ws_url: String,
    api_key: String,
    start_at: Instant,
    send_deadline: Instant,
    read_deadline: Instant,
    calls_per_sec: f64,
) -> Result<Vec<Result<ConnStats, tokio::task::JoinError>>> {
    let period = Duration::from_secs_f64(1.0 / calls_per_sec);
    let link_ws = link_url(&ws_url)?;

    let mut handles = Vec::with_capacity(sessions.len() / 2 + 1);

    // One link consumer for the whole tenant (the /link is last-writer-wins, so
    // exactly one). Spawned first, during warmup, so it is connected before the
    // first accept emits a set_targets (a disconnected link drops events, §5).
    handles.push(tokio::spawn(run_link_consumer(
        link_ws,
        api_key,
        read_deadline,
    )));

    // Stagger pair phases across one period — same de-phasing as the message
    // driver: a shared start_at fires every pair's call lifecycle on the same
    // tick, one aligned burst per period instead of a steady calls/s stream.
    let pairs = (sessions.len() / 2).max(1);
    let mut it = sessions.into_iter();
    let mut pair_idx = 0usize;
    while let (Some(caller), Some(callee)) = (it.next(), it.next()) {
        handles.push(tokio::spawn(run_call_pair(PairConfig {
            ws_url: ws_url.clone(),
            caller_token: caller.token,
            callee_token: callee.token,
            callee_number: callee.number,
            caller_char: caller.char_id,
            callee_char: callee.char_id,
            start_at: start_at + period.mul_f64(pair_idx as f64 / pairs as f64),
            send_deadline,
            period,
        })));
        pair_idx += 1;
    }

    Ok(futures_util::future::join_all(handles).await)
}

struct PairConfig {
    ws_url: String,
    caller_token: String,
    callee_token: String,
    callee_number: String,
    caller_char: Uuid,
    callee_char: Uuid,
    start_at: Instant,
    send_deadline: Instant,
    period: Duration,
}

/// Drive one caller/callee pair to completion. Never panics: any failure folds
/// into `ConnStats::errored` so one bad pair cannot abort the run.
async fn run_call_pair(cfg: PairConfig) -> ConnStats {
    match drive_pair(cfg).await {
        Ok(stats) => stats,
        Err(e) => ConnStats::errored(format!("{e:#}")),
    }
}

async fn drive_pair(cfg: PairConfig) -> Result<ConnStats> {
    let mut caller = connect_auth(&cfg.ws_url, &cfg.caller_token)
        .await
        .context("connect+auth caller")?;
    let mut callee = connect_auth(&cfg.ws_url, &cfg.callee_token)
        .await
        .context("connect+auth callee")?;

    let mut stats = ConnStats::default();

    // First tick fires at the shared `start_at`, so every pair begins churning
    // together after warmup. `Delay` keeps a slow lifecycle from bursting missed
    // ticks — the churn rate stays the cap, never a catch-up spike.
    let mut tick = interval_at(cfg.start_at, cfg.period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        if Instant::now() >= cfg.send_deadline {
            break;
        }
        call_round(
            &mut caller,
            &mut callee,
            &cfg.callee_number,
            cfg.caller_char,
            cfg.callee_char,
            &mut stats,
        )
        .await?;
    }

    Ok(stats)
}

/// One full call lifecycle. Every step is send-then-await-ack on the acting
/// socket, so both sockets stay drained (the parties subscribe to nothing, so
/// they only ever receive their own command acks — see the module cuts). Records
/// the start→active round trip as the `ack` (call-setup) latency and counts one
/// completed call in `sends`.
async fn call_round(
    caller: &mut Ws,
    callee: &mut Ws,
    callee_number: &str,
    caller_char: Uuid,
    callee_char: Uuid,
    stats: &mut ConnStats,
) -> Result<()> {
    let t0 = Instant::now();

    // start → ring (emitted, unconsumed) → accept lands the session `active` and
    // fires set_targets on the link.
    let call_id = start_call(caller, callee_number)
        .await
        .context("calls.start")?;
    accept_call(callee, call_id).await.context("calls.accept")?;
    stats.ack.saturating_record(t0.elapsed().as_micros() as u64);

    // Signal both directions: sender and `to` are both live participants of the
    // now-active call, so the relay authorizes and publishes each. Opaque, ≤16KB.
    signal(caller, call_id, callee_char)
        .await
        .context("calls.signal caller→callee")?;
    signal(callee, call_id, caller_char)
        .await
        .context("calls.signal callee→caller")?;

    // Both hang up: the caller → Left (session still active while the callee is
    // joined), then the callee's hangup is the *last* one → session Ended → link
    // `clear`. Ending the session frees both numbers so the next round's start is
    // never rejected `busy`.
    hangup(caller, call_id)
        .await
        .context("calls.hangup caller")?;
    hangup(callee, call_id)
        .await
        .context("calls.hangup callee")?;

    stats.sends += 1;
    Ok(())
}

/// `calls.signal { call_id, to, payload }` → its ok ack. Frame id 2 is safe to
/// reuse (the socket is driven strictly sequentially — see `linkdrop::start_call`).
async fn signal(ws: &mut Ws, call_id: Uuid, to: Uuid) -> Result<()> {
    send(
        ws,
        2,
        Cmd::CallsSignal {
            call_id,
            to,
            payload: signal_payload(),
        },
    )
    .await?;
    let (ok, _) = await_ack(ws, 2).await?;
    if !ok {
        bail!("calls.signal ack not ok");
    }
    Ok(())
}

/// `calls.hangup { call_id }` → its ok ack.
async fn hangup(ws: &mut Ws, call_id: Uuid) -> Result<()> {
    send(ws, 2, Cmd::CallsHangup { call_id }).await?;
    let (ok, _) = await_ack(ws, 2).await?;
    if !ok {
        bail!("calls.hangup ack not ok");
    }
    Ok(())
}

/// The tenant's single `/link` consumer: connect, then drain until the run's
/// read deadline, counting `set_targets` (the non-vacuity proof the link relay
/// fired) and treating a 4409 as the link-under-load failure. Every non-calls
/// scenario leaves `set_targets` at 0.
async fn run_link_consumer(link_ws: String, api_key: String, read_deadline: Instant) -> ConnStats {
    match drive_link(&link_ws, &api_key, read_deadline).await {
        Ok(stats) => stats,
        Err(e) => ConnStats::errored(format!("{e:#}")),
    }
}

async fn drive_link(link_ws: &str, api_key: &str, read_deadline: Instant) -> Result<ConnStats> {
    let mut link = connect_link(link_ws, api_key)
        .await
        .context("connect link consumer")?;
    let mut stats = ConnStats::default();

    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(read_deadline) => break,
            frame = link.next() => {
                match frame {
                    None | Some(Err(_)) => break, // link stream ended
                    Some(Ok(Message::Text(t))) => {
                        stats.recvs += 1;
                        if is_set_targets(&t) {
                            stats.set_targets += 1;
                        }
                    }
                    Some(Ok(Message::Close(c))) => {
                        match c.map(|f| u16::from(f.code)) {
                            Some(CLOSE_SLOW_CONSUMER) => stats.durable_closes += 1,
                            _ => stats.other_closes += 1,
                        }
                        break;
                    }
                    Some(Ok(_)) => {} // ping/pong/binary
                }
            }
        }
    }

    Ok(stats)
}

/// Is this link frame a `calls.voice` `set_targets` push (for any call)? The
/// link only ever carries `calls.voice`, so this is the "the relay delivered a
/// target" signal.
fn is_set_targets(text: &str) -> bool {
    matches!(
        serde_json::from_str::<ServerMsg>(text),
        Ok(ServerMsg::Push {
            evt: Evt::CallsVoice {
                action: VoiceAction::SetTargets,
                ..
            },
            ..
        })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_targets_counts_only_set_targets_voice_frames() {
        let call = Uuid::now_v7();
        let set = format!(
            r#"{{"topic":"link","evt":"calls.voice","payload":{{"call_id":"{call}","action":"set_targets","characters":["{call}"]}}}}"#
        );
        assert!(is_set_targets(&set));

        // A clear (session ended) is a link frame but not a set_targets.
        let clear = format!(
            r#"{{"topic":"link","evt":"calls.voice","payload":{{"call_id":"{call}","action":"clear","characters":[]}}}}"#
        );
        assert!(!is_set_targets(&clear));

        // A non-voice push (or garbage) is not counted.
        assert!(!is_set_targets(r#"{"reply_to":2,"ok":true,"payload":{}}"#));
        assert!(!is_set_targets("not json"));
    }
}

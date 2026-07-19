//! `--verify-resume <journal> <ws_url>` (roadmap Sprint 9 item 3, the
//! `kill9-mid-send` verifier). Reads the ack journal loadgen writes under
//! `OPN_LOADGEN_ACK_JOURNAL`, then for each channel resumes from seq 0 with a
//! member token and asserts every acked `(channel, seq)` is replayed.
//!
//! The invariant under test: a `kill -9` of Core between commit and restart
//! loses no *acked* message. Core acks a send only after the row commits
//! (persist-then-ack, §8), so every acked seq is durable; resume replays
//! `seq > 0` before the sub ack (§4.4). If any acked seq is absent from the
//! replay, an acked message was lost across the crash — the drill fails.
//!
//! Exit 0 = all acked seqs replayed; 1 = a gap (or no data recorded); 2 = an
//! operational failure (couldn't connect/auth) — same code convention as the
//! load run.

use std::collections::BTreeSet;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use contracts::{Cmd, Evt, ServerMsg};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use crate::driver;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One channel's ground truth, as written by `main::write_journal`.
#[derive(Deserialize)]
struct ChannelJournal {
    channel_id: Uuid,
    token: String,
    acked_seqs: Vec<i64>,
}

pub async fn verify_resume(journal_path: &str, ws_url: &str) -> Result<ExitCode> {
    let raw = std::fs::read_to_string(journal_path)
        .with_context(|| format!("read journal {journal_path}"))?;
    let channels: Vec<ChannelJournal> = serde_json::from_str(&raw).context("parse journal json")?;

    let total_acked: usize = channels.iter().map(|c| c.acked_seqs.len()).sum();
    // No acked messages ⇒ the drill never sent/acked anything before the kill
    // (mis-timed kill, failed seed). That is a failed drill, not a pass.
    if total_acked == 0 {
        eprintln!(
            "verify: FAIL — journal recorded 0 acked messages across {} channel(s); \
             drill produced no data to verify",
            channels.len()
        );
        return Ok(ExitCode::from(1));
    }

    let mut missing_total = 0usize;
    for ch in &channels {
        let replayed = resume_channel(ws_url, ch)
            .await
            .with_context(|| format!("resume channel {}", ch.channel_id))?;
        let acked: BTreeSet<i64> = ch.acked_seqs.iter().copied().collect();
        let missing = missing_seqs(&acked, &replayed);
        if missing.is_empty() {
            eprintln!(
                "verify: channel {} OK — all {} acked seq(s) replayed",
                ch.channel_id,
                acked.len()
            );
        } else {
            missing_total += missing.len();
            let shown = &missing[..missing.len().min(20)];
            eprintln!(
                "verify: channel {} FAIL — {} of {} acked seq(s) missing from replay: {:?}",
                ch.channel_id,
                missing.len(),
                acked.len(),
                shown
            );
        }
    }

    if missing_total == 0 {
        // Shared by every chaos drill (kill9, pg-restart, …), so name the class
        // of guarantee, not one specific fault.
        eprintln!(
            "verify: PASS — {} acked message(s) across {} channel(s) survived the fault and replayed",
            total_acked,
            channels.len()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("verify: FAIL — {missing_total} acked message(s) lost across the restart");
        Ok(ExitCode::from(1))
    }
}

/// Acked seqs absent from the replay — the lost-message set. The drill's whole
/// verdict rests on this being non-empty exactly when an acked message vanished.
fn missing_seqs(acked: &BTreeSet<i64>, replayed: &BTreeSet<i64>) -> Vec<i64> {
    acked
        .iter()
        .copied()
        .filter(|s| !replayed.contains(s))
        .collect()
}

/// Connect as a channel member, resume from seq 0, drain the replay (pushed
/// *before* the sub ack, §4.4), and return the set of replayed seqs.
async fn resume_channel(ws_url: &str, ch: &ChannelJournal) -> Result<BTreeSet<i64>> {
    let mut ws: Ws = connect_async(ws_url)
        .await
        .with_context(|| format!("ws connect {ws_url}"))?
        .0;

    driver::send(
        &mut ws,
        1,
        Cmd::Auth {
            token: ch.token.clone(),
        },
    )
    .await?;
    let (ok, _) = driver::await_ack(&mut ws, 1).await?;
    if !ok {
        bail!("auth ack not ok (session lost across restart?)");
    }

    // `last_seq: Some(0)` replays every committed message (seq > 0) as
    // `channels.message` events, terminated by the sub ack.
    let topic = format!("ch:{}", ch.channel_id);
    driver::send(
        &mut ws,
        2,
        Cmd::Sub {
            topic,
            last_seq: Some(0),
        },
    )
    .await?;

    let mut replayed = BTreeSet::new();
    let sub_ok = timeout(Duration::from_secs(15), async {
        loop {
            match ws.next().await {
                None => bail!("stream closed during replay"),
                Some(Err(e)) => bail!("ws error during replay: {e}"),
                Some(Ok(Message::Text(t))) => {
                    match serde_json::from_str::<ServerMsg>(&t) {
                        Ok(ServerMsg::Push {
                            evt: Evt::ChannelsMessage { seq, .. },
                            ..
                        }) => {
                            replayed.insert(seq);
                        }
                        // A full 500-row page means the drill outran one replay
                        // window — the verifier can't see the whole set, so it
                        // can't prove the invariant. Fail loud, shorten the run.
                        Ok(ServerMsg::Push {
                            evt: Evt::ChannelsResumeOverflow { .. },
                            ..
                        }) => bail!(
                            "replay hit RESUME_MAX (500) — drill sent too many messages per \
                             channel to verify in one resume; shorten the send window"
                        ),
                        Ok(ServerMsg::Ack {
                            reply_to: 2, ok, ..
                        }) => return Ok(ok),
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) => bail!("closed during replay"),
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .context("timed out draining replay")??;

    if !sub_ok {
        bail!(
            "sub ack not ok — token is not a member of {}",
            ch.channel_id
        );
    }
    Ok(replayed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_seqs_flags_a_lost_message() {
        let acked: BTreeSet<i64> = [1, 2, 3, 4].into_iter().collect();
        // Replay dropped seq 3 → the drill must report exactly that gap.
        let replayed: BTreeSet<i64> = [1, 2, 4].into_iter().collect();
        assert_eq!(missing_seqs(&acked, &replayed), vec![3]);
    }

    #[test]
    fn missing_seqs_empty_when_all_replayed() {
        let acked: BTreeSet<i64> = [1, 2, 3].into_iter().collect();
        // Superset replay (extra live messages) still counts as no loss.
        let replayed: BTreeSet<i64> = [1, 2, 3, 4, 5].into_iter().collect();
        assert!(missing_seqs(&acked, &replayed).is_empty());
    }
}

//! `opn-loadgen` v0 (roadmap Sprint 4 item 9): a load generator that reuses the
//! `contracts` wire types, seeds a population over the real mint API, drives N
//! paired WebSocket connections at a target message rate, and reports ack-RTT
//! and delivery-latency percentiles plus drops/closes as one JSON summary line
//! (for CI assertion) and a human table (to stderr).
//!
//! Scenario is JSON, not the roadmap's TOML: `serde_json` is already a
//! workspace dependency, the config is six fields, and a committed named
//! scenario file — the actual point — works identically. No `toml` crate.
//! Percentiles come from `hdrhistogram` (roadmap Sprint 4 item 9's named tool):
//! fixed-memory bucketed quantiles that stay bounded no matter how many samples
//! land — the v0 sorted-`Vec` was fine to ~1M samples but Sprint 10's 24 h soak
//! records tens of millions, which the Vec would OOM on. Bucketing costs a
//! little precision (3 sig figs) for constant memory; the delivery/latency
//! gates live far above that resolution.

mod callchurn;
mod driver;
mod http;
mod linkdrop;
mod verify;
mod xinstance;

use std::collections::{BTreeSet, HashMap};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::stream::{StreamExt, TryStreamExt};
use hdrhistogram::Histogram;
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio::time::{Duration, Instant};
use uuid::Uuid;

use driver::{run_connection, ConnConfig, ConnStats, Pairing};

/// Seed-phase mint concurrency. Held near the server's connection pool
/// (`max_connections = 20`) so we saturate it without piling up acquire waits;
/// higher just queues on `pg` acquire, lower leaves the pool idle. Turns the
/// 3000-conn soak/hot-channel seed from a minute of serial round trips into
/// seconds, without changing the index-aligned order the drivers depend on.
const SEED_CONCURRENCY: usize = 20;

/// A load scenario. Committed as a file (e.g. `scenarios/smoke.json`) so CI and
/// Sprint 10 reference named, reviewable scenarios rather than ad-hoc flags.
#[derive(Debug, Deserialize)]
struct Scenario {
    /// `http://ip:port` — the seed (mint) endpoint.
    target_http: String,
    /// `ws://ip:port/ws` — the gateway endpoint.
    target_ws: String,
    /// Tenant API key. Overridden by `OPN_LOADGEN_API_KEY` when set, so the
    /// committed file holds `""` and CI injects the runtime-generated key.
    #[serde(default)]
    api_key: String,
    connections: usize,
    duration_secs: u64,
    /// Aggregate send rate across all connections.
    total_msgs_per_sec: f64,
    /// Setup barrier: all connections align their first send this long after
    /// launch. Must exceed the slowest connection's connect+auth+sub time.
    #[serde(default = "default_warmup")]
    warmup_secs: u64,
    /// Every Nth send from a connection also emits a typing ping (0 = never).
    #[serde(default)]
    typing_every: u64,
    /// Every Nth send also advances the read watermark (0 = never).
    #[serde(default)]
    read_every: u64,
    /// hot-channel topology (roadmap Sprint 10 item 1): when true, all
    /// `connections` join ONE group channel — connection 0 creates it and
    /// `member_add`s the rest (paced under the Social budget), everyone
    /// subscribes to that single channel and sends, so each send fans out to
    /// every member (the fan-out stress shape). `false` (default) is the
    /// paired-DM graph every other scenario uses.
    #[serde(default)]
    group: bool,
    /// call-churn topology (roadmap Sprint 10 item 1): when true, the paired
    /// message driver is replaced by the calls driver — every two connections
    /// form a caller/callee pair that churns a full call lifecycle
    /// (`calls.start` → `calls.accept` → `calls.signal` both ways → `calls.hangup`)
    /// at `calls_per_sec`, and one shared `/link` consumer drains the tenant's
    /// voice-target events. Exercises the call FSM + the `/link` relay under load.
    /// Mutually exclusive with `group` and `reconnect_at_secs`. `total_msgs_per_sec`
    /// is ignored in this mode (call pacing comes from `calls_per_sec`).
    #[serde(default)]
    calls: bool,
    /// Per-pair call rate for the call-churn driver (calls/second/pair). 50 pairs
    /// at 1.0 ≈ 50 calls/s aggregate — the roadmap's "1 Hz" churn. Unused unless
    /// `calls` is set.
    #[serde(default = "default_calls_per_sec")]
    calls_per_sec: f64,
    /// Non-vacuity gate (call-churn): fail if zero calls completed OR the link
    /// consumer received zero `set_targets` — i.e. the FSM never ran or the relay
    /// never fired. Mirrors reconnect-storm's `assert_reconnected`.
    #[serde(default)]
    assert_calls: bool,
    /// reconnect-storm (roadmap Sprint 10 item 1): seconds after send-start when
    /// every connection drops and reconnects. `None` (every other scenario) ⇒ no
    /// storm, a single connection epoch. Must be < `duration_secs` so there is a
    /// post-storm send window to prove resume continuity.
    #[serde(default)]
    reconnect_at_secs: Option<u64>,
    /// Max reconnect stagger — OPN.md §7's 0–3 s thundering-herd jitter, spread
    /// deterministically across connections (no rng). Only used with a storm.
    #[serde(default = "default_reconnect_jitter")]
    reconnect_jitter_secs: f64,
    /// Non-vacuity gate (reconnect-storm): fail if the run reported 0 reconnects,
    /// i.e. the storm never fired. Mirrors pg-restart's `assert_error_acks`.
    #[serde(default)]
    assert_reconnected: bool,
    /// CI gate: fail (exit 1) if ack RTT p99 exceeds this. `null` = no gate.
    #[serde(default)]
    assert_ack_p99_ms: Option<f64>,
    /// CI gate: fail if any connection was closed 4409 (slow consumer).
    #[serde(default)]
    assert_no_durable_closes: bool,
    /// Chaos gate (pg-restart drill): fail if the run saw zero error acks —
    /// the "the DB-outage gap produced error acks, not silence" invariant.
    #[serde(default)]
    assert_error_acks: bool,
    /// Delivery gate (roadmap Sprint 10 test plan): fail if any subscribed
    /// channel's received seq stream had a hole — turning the "no acked message
    /// is lost" guarantee into a continuously-checked property under load.
    #[serde(default)]
    assert_no_seq_gaps: bool,
}

fn default_warmup() -> u64 {
    3
}

fn default_reconnect_jitter() -> f64 {
    3.0
}

fn default_calls_per_sec() -> f64 {
    1.0
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("loadgen: {e:#}");
            ExitCode::from(2) // operational failure (couldn't seed/connect)
        }
    }
}

async fn run() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // kill9 chaos verifier: resume every channel in the ack journal and
        // assert no acked message was lost across the restart.
        Some("--verify-resume") => {
            let journal = args
                .get(1)
                .ok_or_else(|| anyhow!("--verify-resume needs <journal.json> <ws_url>"))?;
            let ws = args
                .get(2)
                .ok_or_else(|| anyhow!("--verify-resume needs <journal.json> <ws_url>"))?;
            verify::verify_resume(journal, ws).await
        }
        // redis-restart chaos checker: prove a message sent on Core A crosses to
        // a subscriber on Core B before and after a Redis restart (pub/sub
        // resubscribe), holding both connections open across the window.
        Some("--xinstance") => {
            let http = args
                .get(1)
                .ok_or_else(|| anyhow!("--xinstance needs <http> <ws_a> <ws_b> [settle_secs]"))?;
            let ws_a = args
                .get(2)
                .ok_or_else(|| anyhow!("--xinstance needs <http> <ws_a> <ws_b> [settle_secs]"))?;
            let ws_b = args
                .get(3)
                .ok_or_else(|| anyhow!("--xinstance needs <http> <ws_a> <ws_b> [settle_secs]"))?;
            let settle = match args.get(4) {
                Some(s) => s.parse().context("settle_secs must be a whole number")?,
                None => 50,
            };
            xinstance::verify_xinstance(http, ws_a, ws_b, settle).await
        }
        // link-drop chaos checker: drive a call so the /link consumer gets
        // set_targets, drop the link mid-call, reconnect, re-sync the active
        // call over HTTP, and prove a subsequent accept reaches the new link.
        Some("--link-drop") => {
            let http = args
                .get(1)
                .ok_or_else(|| anyhow!("--link-drop needs <http> <ws> [drop_gap_secs]"))?;
            let ws = args
                .get(2)
                .ok_or_else(|| anyhow!("--link-drop needs <http> <ws> [drop_gap_secs]"))?;
            let gap = match args.get(3) {
                Some(s) => s.parse().context("drop_gap_secs must be a whole number")?,
                None => 3,
            };
            linkdrop::verify_linkdrop(http, ws, gap).await
        }
        Some("--scenario") => {
            let path = args
                .get(1)
                .ok_or_else(|| anyhow!("--scenario needs a path"))?;
            run_scenario(path).await
        }
        _ => bail!(
            "usage: opn-loadgen --scenario <path.json> \
             | --verify-resume <journal.json> <ws_url> \
             | --xinstance <http> <ws_a> <ws_b> [settle_secs] \
             | --link-drop <http> <ws> [drop_gap_secs]"
        ),
    }
}

async fn run_scenario(path: &str) -> Result<ExitCode> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read scenario {path}"))?;
    let mut scenario: Scenario = serde_json::from_str(&raw).context("parse scenario json")?;

    // When set, every ok-acked send's seq is journaled here for the kill9 chaos
    // verifier; unset (every perf/soak run) means zero recording overhead.
    let journal_path = std::env::var("OPN_LOADGEN_ACK_JOURNAL")
        .ok()
        .filter(|s| !s.is_empty());
    let record_acks = journal_path.is_some();

    if let Ok(key) = std::env::var("OPN_LOADGEN_API_KEY") {
        if !key.is_empty() {
            scenario.api_key = key;
        }
    }
    validate(&scenario)?;

    // Connection count. The group topology uses every connection (one creator +
    // N-1 members); the paired topology rounds down to an even count.
    let conns = if scenario.group {
        scenario.connections
    } else {
        let even = scenario.connections & !1;
        if even < scenario.connections {
            eprintln!(
                "loadgen: rounding {} connections down to {even} (paired)",
                scenario.connections
            );
        }
        even
    };

    // ── seed: mint one session per connection over the real HTTP API ────────
    // Bounded-concurrent (`buffered`) so the seed scales to soak/hot-channel
    // connection counts. `buffered` yields in input order, so `sessions` stays
    // index-aligned — the paired topology (conns 2i/2i+1) and the group creator
    // (index 0) both depend on that; unordered `buffer_unordered` would silently
    // shuffle pairs. `try_collect` short-circuits on the first mint error.
    let host = host_of(&scenario.target_http)?;
    eprintln!("loadgen: seeding {conns} sessions via {host} …");
    let sessions: Vec<http::Minted> = futures_util::stream::iter(0..conns)
        .map(|i| {
            let host = host.clone();
            let key = scenario.api_key.clone();
            async move {
                http::mint(&host, &key, &format!("lg:{i}"))
                    .await
                    .with_context(|| format!("mint session {i}"))
            }
        })
        .buffered(SEED_CONCURRENCY)
        .try_collect()
        .await?;

    // ── launch: paired connections, all aligned to one start instant ────────
    let epoch = Instant::now();
    let start_at = epoch + Duration::from_secs(scenario.warmup_secs);
    let send_deadline = start_at + Duration::from_secs(scenario.duration_secs);
    let read_deadline = send_deadline + Duration::from_secs(2); // drain grace

    // ── call-churn: a wholly separate driver (calls, not messages) ──────────
    // Every two sessions form a caller/callee pair; one shared /link consumer
    // drains the tenant's voice-target events. The message machinery below is
    // never reached in this mode, so it stays byte-identical for every other
    // scenario.
    if scenario.calls {
        eprintln!(
            "loadgen: {conns} conns ({} pairs), {:.1} calls/s/pair, {}s run …",
            conns / 2,
            scenario.calls_per_sec,
            scenario.duration_secs
        );
        let results = callchurn::run_calls(
            sessions,
            scenario.target_ws.clone(),
            scenario.api_key.clone(),
            start_at,
            send_deadline,
            read_deadline,
            scenario.calls_per_sec,
        )
        .await?;
        let summary = Summary::merge(results);
        summary.report(&scenario);
        return Ok(summary.exit_code(&scenario));
    }

    let period = Duration::from_secs_f64(conns as f64 / scenario.total_msgs_per_sec);

    // reconnect-storm: one aligned storm instant for every connection, each
    // reconnecting after a per-connection stagger spread evenly over
    // `[0, reconnect_jitter_secs)` — the thundering herd without an rng.
    let reconnect_at = scenario
        .reconnect_at_secs
        .map(|s| start_at + Duration::from_secs(s));
    let reconnect_delay = |i: usize| {
        Duration::from_secs_f64(scenario.reconnect_jitter_secs * (i as f64 / conns as f64))
    };

    eprintln!(
        "loadgen: {conns} conns, {:.0} msg/s aggregate ({:.2}s per conn), {}s run …",
        scenario.total_msgs_per_sec,
        period.as_secs_f64(),
        scenario.duration_secs
    );

    let base = |token, char_id, pairing, rc_delay| ConnConfig {
        ws_url: scenario.target_ws.clone(),
        token,
        char_id,
        pairing,
        epoch,
        start_at,
        send_deadline,
        read_deadline,
        period,
        typing_every: scenario.typing_every,
        read_every: scenario.read_every,
        reconnect_at,
        reconnect_delay: rc_delay,
        record_acks,
    };

    let mut handles = Vec::with_capacity(conns);
    if scenario.group {
        // hot-channel: one group of every connection. Conn 0 creates the channel
        // and member_adds the rest; each member waits on its own `oneshot` for the
        // channel id, then subs and sends. No storm here, so `reconnect_delay` is
        // unused (reconnect_at is None).
        let mut sessions = sessions.into_iter();
        let creator = sessions
            .next()
            .ok_or_else(|| anyhow!("group needs >= 2 connections"))?;
        let members: Vec<_> = sessions.collect();
        let member_ids: Vec<Uuid> = members.iter().map(|m| m.char_id).collect();
        let (txs, rxs): (Vec<_>, Vec<_>) = members.iter().map(|_| oneshot::channel()).unzip();
        handles.push(tokio::spawn(run_connection(base(
            creator.token,
            creator.char_id,
            Pairing::GroupCreator { member_ids, txs },
            reconnect_delay(0),
        ))));
        for (i, (m, rx)) in members.into_iter().zip(rxs).enumerate() {
            handles.push(tokio::spawn(run_connection(base(
                m.token,
                m.char_id,
                Pairing::Right { rx },
                reconnect_delay(i + 1),
            ))));
        }
    } else {
        // paired DMs: Left opens the thread to Right's number, both sub and send.
        let mut sessions = sessions.into_iter();
        let mut idx = 0usize;
        while let (Some(left), Some(right)) = (sessions.next(), sessions.next()) {
            let (tx, rx) = oneshot::channel();
            handles.push(tokio::spawn(run_connection(base(
                left.token,
                left.char_id,
                Pairing::Left {
                    peer_number: right.number.clone(),
                    tx,
                },
                reconnect_delay(idx),
            ))));
            handles.push(tokio::spawn(run_connection(base(
                right.token,
                right.char_id,
                Pairing::Right { rx },
                reconnect_delay(idx + 1),
            ))));
            idx += 2;
        }
    }

    let results = futures_util::future::join_all(handles).await;

    // Write the ack journal (borrow) before merge consumes `results`.
    if let Some(path) = &journal_path {
        write_journal(path, &results)?;
    }

    let summary = Summary::merge(results);
    summary.report(&scenario);

    Ok(summary.exit_code(&scenario))
}

/// Collapse per-connection acked seqs into one entry per channel (both pair
/// members send to the same channel; union their seqs, keep one member token)
/// and write the kill9 verifier's ground-truth journal.
fn write_journal(path: &str, results: &[Result<ConnStats, tokio::task::JoinError>]) -> Result<()> {
    let mut by_channel: HashMap<Uuid, (String, BTreeSet<i64>)> = HashMap::new();
    for r in results.iter().flatten() {
        if let (Some(cid), Some(tok)) = (r.channel_id, r.token.as_ref()) {
            let e = by_channel
                .entry(cid)
                .or_insert_with(|| (tok.clone(), BTreeSet::new()));
            e.1.extend(r.acked_seqs.iter().copied());
        }
    }
    let total: usize = by_channel.values().map(|(_, s)| s.len()).sum();
    let entries: Vec<_> = by_channel
        .into_iter()
        .map(|(cid, (tok, seqs))| {
            serde_json::json!({
                "channel_id": cid,
                "token": tok,
                "acked_seqs": seqs.into_iter().collect::<Vec<_>>(),
            })
        })
        .collect();
    std::fs::write(path, serde_json::to_string(&entries)?)
        .with_context(|| format!("write ack journal {path}"))?;
    eprintln!(
        "loadgen: wrote ack journal — {} channel(s), {total} acked seq(s) -> {path}",
        entries.len()
    );
    Ok(())
}

fn validate(s: &Scenario) -> Result<()> {
    if s.api_key.is_empty() {
        bail!("no api key: set it in the scenario or OPN_LOADGEN_API_KEY");
    }
    if s.connections < 2 {
        bail!("connections must be >= 2 (a pair, or a group creator + >= 1 member)");
    }
    if s.total_msgs_per_sec <= 0.0 {
        bail!("total_msgs_per_sec must be > 0");
    }
    if s.group && s.reconnect_at_secs.is_some() {
        bail!("group and reconnect_at_secs are mutually exclusive (no group storm scenario)");
    }
    if s.calls {
        if s.group || s.reconnect_at_secs.is_some() {
            bail!("calls is mutually exclusive with group and reconnect_at_secs");
        }
        if s.calls_per_sec <= 0.0 {
            bail!("calls_per_sec must be > 0");
        }
    }
    if let Some(r) = s.reconnect_at_secs {
        if r >= s.duration_secs {
            bail!(
                "reconnect_at_secs ({r}) must be < duration_secs ({}) so there is \
                 a post-storm window to prove resume continuity",
                s.duration_secs
            );
        }
    }
    if s.reconnect_jitter_secs < 0.0 {
        bail!("reconnect_jitter_secs must be >= 0");
    }
    Ok(())
}

/// `http://127.0.0.1:8080/…` -> `127.0.0.1:8080` for `TcpStream::connect`.
fn host_of(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("target_http must start with http:// : {url}"))?;
    Ok(rest.split('/').next().unwrap_or(rest).to_owned())
}

/// The merged run result and its derived percentiles.
struct Summary {
    connections: u64,
    sends: u64,
    recvs: u64,
    seq_gaps: u64,
    set_targets: u64,
    rate_limited: u64,
    error_acks: u64,
    durable_closes: u64,
    other_closes: u64,
    errors: u64,
    reconnects: u64,
    ack: Percentiles,
    delivery: Percentiles,
    resume: Percentiles,
    first_error: Option<String>,
}

struct Percentiles {
    count: u64,
    p50_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

impl Summary {
    fn merge(results: Vec<Result<ConnStats, tokio::task::JoinError>>) -> Summary {
        let mut all = ConnStats::default();
        let mut connections = 0u64;
        for r in results {
            connections += 1;
            match r {
                Ok(s) => {
                    // Same fixed bounds on every histogram, so `add` never
                    // errors (it only can if the source out-grew the dest max,
                    // impossible here) — the merge is lossless.
                    let _ = all.ack.add(&s.ack);
                    let _ = all.delivery.add(&s.delivery);
                    let _ = all.resume.add(&s.resume);
                    all.sends += s.sends;
                    all.recvs += s.recvs;
                    all.seq_gaps += s.seq_gaps;
                    all.set_targets += s.set_targets;
                    all.rate_limited += s.rate_limited;
                    all.error_acks += s.error_acks;
                    all.durable_closes += s.durable_closes;
                    all.other_closes += s.other_closes;
                    all.reconnects += s.reconnects;
                    all.errors += s.errors;
                    if all.error_detail.is_none() {
                        all.error_detail = s.error_detail;
                    }
                }
                Err(e) => {
                    all.errors += 1;
                    if all.error_detail.is_none() {
                        all.error_detail = Some(format!("task panicked: {e}"));
                    }
                }
            }
        }
        Summary {
            connections,
            sends: all.sends,
            recvs: all.recvs,
            seq_gaps: all.seq_gaps,
            set_targets: all.set_targets,
            rate_limited: all.rate_limited,
            error_acks: all.error_acks,
            durable_closes: all.durable_closes,
            other_closes: all.other_closes,
            errors: all.errors,
            reconnects: all.reconnects,
            ack: Percentiles::of(&all.ack),
            delivery: Percentiles::of(&all.delivery),
            resume: Percentiles::of(&all.resume),
            first_error: all.error_detail,
        }
    }

    fn report(&self, scenario: &Scenario) {
        // JSON summary line to stdout — the CI-parseable contract. One line.
        let json = serde_json::json!({
            "connections": self.connections,
            "sends": self.sends,
            "recvs": self.recvs,
            "seq_gaps": self.seq_gaps,
            "set_targets": self.set_targets,
            "rate_limited": self.rate_limited,
            "error_acks": self.error_acks,
            "durable_closes": self.durable_closes,
            "other_closes": self.other_closes,
            "errors": self.errors,
            "reconnects": self.reconnects,
            "ack_count": self.ack.count,
            "ack_p50_ms": round2(self.ack.p50_ms),
            "ack_p99_ms": round2(self.ack.p99_ms),
            "ack_max_ms": round2(self.ack.max_ms),
            "delivery_count": self.delivery.count,
            "delivery_p50_ms": round2(self.delivery.p50_ms),
            "delivery_p99_ms": round2(self.delivery.p99_ms),
            "delivery_max_ms": round2(self.delivery.max_ms),
            "resume_count": self.resume.count,
            "resume_p50_ms": round2(self.resume.p50_ms),
            "resume_p99_ms": round2(self.resume.p99_ms),
            "resume_max_ms": round2(self.resume.max_ms),
        });
        println!("{json}");

        // Human table to stderr.
        eprintln!("──────────────── loadgen summary ────────────────");
        eprintln!("connections     {}", self.connections);
        eprintln!("sends / recvs   {} / {}", self.sends, self.recvs);
        eprintln!(
            "ack RTT ms      p50 {:.2}  p99 {:.2}  max {:.2}  (n={})",
            self.ack.p50_ms, self.ack.p99_ms, self.ack.max_ms, self.ack.count
        );
        eprintln!(
            "delivery ms     p50 {:.2}  p99 {:.2}  max {:.2}  (n={})",
            self.delivery.p50_ms, self.delivery.p99_ms, self.delivery.max_ms, self.delivery.count
        );
        if self.resume.count > 0 || self.reconnects > 0 {
            eprintln!(
                "resume ms       p50 {:.2}  p99 {:.2}  max {:.2}  (reconnects={})",
                self.resume.p50_ms, self.resume.p99_ms, self.resume.max_ms, self.reconnects
            );
        }
        if scenario.calls {
            // In calls mode: sends = completed calls, recvs = /link frames, ack =
            // call-setup latency, set_targets = link deliveries.
            eprintln!("set_targets     {}", self.set_targets);
        }
        eprintln!("rate_limited    {}", self.rate_limited);
        eprintln!("error_acks      {}", self.error_acks);
        eprintln!("seq_gaps        {}", self.seq_gaps);
        eprintln!(
            "closes          durable {}  other {}",
            self.durable_closes, self.other_closes
        );
        eprintln!("errors          {}", self.errors);
        if let Some(e) = &self.first_error {
            eprintln!("first error     {e}");
        }
        if let Some(gate) = scenario.assert_ack_p99_ms {
            eprintln!("gate ack p99    {:.2} < {:.2} ?", self.ack.p99_ms, gate);
        }
        eprintln!("─────────────────────────────────────────────────");
    }

    /// Exit code: 0 pass, 1 an assertion breached, 2 an operational error.
    fn exit_code(&self, scenario: &Scenario) -> ExitCode {
        if self.errors > 0 {
            eprintln!("loadgen: FAIL — {} connection error(s)", self.errors);
            return ExitCode::from(2);
        }
        let mut failed = false;
        if scenario.assert_no_durable_closes && self.durable_closes > 0 {
            eprintln!(
                "loadgen: FAIL — {} durable (4409) close(s) under design load",
                self.durable_closes
            );
            failed = true;
        }
        if let Some(gate) = scenario.assert_ack_p99_ms {
            if self.ack.p99_ms > gate {
                eprintln!(
                    "loadgen: FAIL — ack p99 {:.2} ms > {:.2} ms gate",
                    self.ack.p99_ms, gate
                );
                failed = true;
            }
        }
        if scenario.assert_error_acks && self.error_acks == 0 {
            eprintln!(
                "loadgen: FAIL — assert_error_acks set but 0 error acks seen \
                 (the DB-outage gap should have produced error acks, not silence)"
            );
            failed = true;
        }
        if scenario.assert_no_seq_gaps && self.seq_gaps > 0 {
            eprintln!(
                "loadgen: FAIL — {} seq gap(s): an acked message was lost from a \
                 subscriber's channel stream (delivery guarantee breached)",
                self.seq_gaps
            );
            failed = true;
        }
        if scenario.assert_reconnected && self.reconnects == 0 {
            eprintln!(
                "loadgen: FAIL — assert_reconnected set but 0 reconnects happened \
                 (the storm never fired — the resume path was never exercised)"
            );
            failed = true;
        }
        if scenario.assert_calls && (self.sends == 0 || self.set_targets == 0) {
            eprintln!(
                "loadgen: FAIL — assert_calls set but {} calls completed and {} \
                 set_targets seen (the FSM never ran or the /link relay never fired)",
                self.sends, self.set_targets
            );
            failed = true;
        }
        if failed {
            ExitCode::from(1)
        } else {
            eprintln!("loadgen: PASS");
            ExitCode::SUCCESS
        }
    }
}

impl Percentiles {
    /// Percentiles from a fixed-memory histogram (µs buckets → ms). Empty
    /// histogram → all zero (`value_at_quantile`/`max` return 0 at count 0),
    /// preserving the old `Vec`-empty behavior.
    fn of(h: &Histogram<u64>) -> Percentiles {
        Percentiles {
            count: h.len(),
            p50_ms: h.value_at_quantile(0.50) as f64 / 1000.0,
            p99_ms: h.value_at_quantile(0.99) as f64 / 1000.0,
            max_ms: h.max() as f64 / 1000.0,
        }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_from_histogram() {
        // 1..=100 us recorded -> p50 ~= 50 us, p99 ~= 99 us, max = 100 us (ms).
        // All values < 1000 with 3 sig figs sit in their own bucket, so the
        // quantiles are exact here (bucketing only rounds larger magnitudes).
        let mut h = driver::new_hist();
        for v in 1..=100u64 {
            h.saturating_record(v);
        }
        let p = Percentiles::of(&h);
        assert_eq!(p.count, 100);
        assert!((p.p50_ms - 0.050).abs() < 0.002, "p50 {}", p.p50_ms);
        assert!((p.p99_ms - 0.099).abs() < 0.002, "p99 {}", p.p99_ms);
        assert!((p.max_ms - 0.100).abs() < 1e-9, "max {}", p.max_ms);
    }

    #[test]
    fn percentiles_empty_is_zero() {
        let p = Percentiles::of(&driver::new_hist());
        assert_eq!(p.count, 0);
        assert_eq!(p.p99_ms, 0.0);
        assert_eq!(p.max_ms, 0.0);
    }

    #[test]
    fn host_strips_scheme_and_path() {
        assert_eq!(
            host_of("http://127.0.0.1:8080").expect("host"),
            "127.0.0.1:8080"
        );
        assert_eq!(
            host_of("http://127.0.0.1:8080/v1/x").expect("host"),
            "127.0.0.1:8080"
        );
        assert!(host_of("ws://x").is_err());
    }
}

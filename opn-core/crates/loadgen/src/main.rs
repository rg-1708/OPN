//! `opn-loadgen` v0 (roadmap Sprint 4 item 9): a load generator that reuses the
//! `contracts` wire types, seeds a population over the real mint API, drives N
//! paired WebSocket connections at a target message rate, and reports ack-RTT
//! and delivery-latency percentiles plus drops/closes as one JSON summary line
//! (for CI assertion) and a human table (to stderr).
//!
//! Scenario is JSON, not the roadmap's TOML: `serde_json` is already a
//! workspace dependency, the config is six fields, and a committed named
//! scenario file — the actual point — works identically. No `toml` crate.
//! Percentiles are exact from a sorted `Vec`, not `hdrhistogram`: the v0 smoke
//! is ~9k samples where exact beats bucketed and needs no dependency.
//! ponytail: the Vec is fine to ~1M samples; Sprint 10's 24 h soak wants
//! hdrhistogram or reservoir sampling before it records hundreds of millions.

mod driver;
mod http;

use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio::time::{Duration, Instant};

use driver::{run_connection, ConnConfig, ConnStats, Pairing};

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
    /// CI gate: fail (exit 1) if ack RTT p99 exceeds this. `null` = no gate.
    #[serde(default)]
    assert_ack_p99_ms: Option<f64>,
    /// CI gate: fail if any connection was closed 4409 (slow consumer).
    #[serde(default)]
    assert_no_durable_closes: bool,
}

fn default_warmup() -> u64 {
    3
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
    let path = scenario_path()?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read scenario {path}"))?;
    let mut scenario: Scenario = serde_json::from_str(&raw).context("parse scenario json")?;

    if let Ok(key) = std::env::var("OPN_LOADGEN_API_KEY") {
        if !key.is_empty() {
            scenario.api_key = key;
        }
    }
    validate(&scenario)?;

    // Even connection count — connections are driven in pairs.
    let conns = scenario.connections & !1;
    if conns < scenario.connections {
        eprintln!(
            "loadgen: rounding {} connections down to {conns} (paired)",
            scenario.connections
        );
    }

    // ── seed: mint one session per connection over the real HTTP API ────────
    let host = host_of(&scenario.target_http)?;
    eprintln!("loadgen: seeding {conns} sessions via {host} …");
    let mut sessions = Vec::with_capacity(conns);
    for i in 0..conns {
        let m = http::mint(&host, &scenario.api_key, &format!("lg:{i}"))
            .await
            .with_context(|| format!("mint session {i}"))?;
        sessions.push(m);
    }

    // ── launch: paired connections, all aligned to one start instant ────────
    let epoch = Instant::now();
    let start_at = epoch + Duration::from_secs(scenario.warmup_secs);
    let send_deadline = start_at + Duration::from_secs(scenario.duration_secs);
    let read_deadline = send_deadline + Duration::from_secs(2); // drain grace
    let period = Duration::from_secs_f64(conns as f64 / scenario.total_msgs_per_sec);

    eprintln!(
        "loadgen: {conns} conns, {:.0} msg/s aggregate ({:.2}s per conn), {}s run …",
        scenario.total_msgs_per_sec,
        period.as_secs_f64(),
        scenario.duration_secs
    );

    let mut handles = Vec::with_capacity(conns);
    let mut sessions = sessions.into_iter();
    while let (Some(left), Some(right)) = (sessions.next(), sessions.next()) {
        let (tx, rx) = oneshot::channel();
        let base = |token, char_id, pairing| ConnConfig {
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
        };
        handles.push(tokio::spawn(run_connection(base(
            left.token,
            left.char_id,
            Pairing::Left {
                peer_number: right.number.clone(),
                tx,
            },
        ))));
        handles.push(tokio::spawn(run_connection(base(
            right.token,
            right.char_id,
            Pairing::Right { rx },
        ))));
    }

    let results = futures_util::future::join_all(handles).await;
    let summary = Summary::merge(results);
    summary.report(&scenario);

    Ok(summary.exit_code(&scenario))
}

fn scenario_path() -> Result<String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--scenario") => args
            .next()
            .ok_or_else(|| anyhow!("--scenario needs a path")),
        _ => bail!("usage: opn-loadgen --scenario <path.json>"),
    }
}

fn validate(s: &Scenario) -> Result<()> {
    if s.api_key.is_empty() {
        bail!("no api key: set it in the scenario or OPN_LOADGEN_API_KEY");
    }
    if s.connections < 2 {
        bail!("connections must be >= 2 (they run in pairs)");
    }
    if s.total_msgs_per_sec <= 0.0 {
        bail!("total_msgs_per_sec must be > 0");
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
    rate_limited: u64,
    durable_closes: u64,
    other_closes: u64,
    errors: u64,
    ack: Percentiles,
    delivery: Percentiles,
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
        let mut acks = Vec::new();
        let mut dels = Vec::new();
        let mut connections = 0u64;
        for r in results {
            connections += 1;
            match r {
                Ok(s) => {
                    acks.extend_from_slice(&s.ack_rtts_us);
                    dels.extend_from_slice(&s.deliveries_us);
                    all.sends += s.sends;
                    all.recvs += s.recvs;
                    all.rate_limited += s.rate_limited;
                    all.durable_closes += s.durable_closes;
                    all.other_closes += s.other_closes;
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
            rate_limited: all.rate_limited,
            durable_closes: all.durable_closes,
            other_closes: all.other_closes,
            errors: all.errors,
            ack: Percentiles::of(&mut acks),
            delivery: Percentiles::of(&mut dels),
            first_error: all.error_detail,
        }
    }

    fn report(&self, scenario: &Scenario) {
        // JSON summary line to stdout — the CI-parseable contract. One line.
        let json = serde_json::json!({
            "connections": self.connections,
            "sends": self.sends,
            "recvs": self.recvs,
            "rate_limited": self.rate_limited,
            "durable_closes": self.durable_closes,
            "other_closes": self.other_closes,
            "errors": self.errors,
            "ack_count": self.ack.count,
            "ack_p50_ms": round2(self.ack.p50_ms),
            "ack_p99_ms": round2(self.ack.p99_ms),
            "ack_max_ms": round2(self.ack.max_ms),
            "delivery_count": self.delivery.count,
            "delivery_p50_ms": round2(self.delivery.p50_ms),
            "delivery_p99_ms": round2(self.delivery.p99_ms),
            "delivery_max_ms": round2(self.delivery.max_ms),
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
        eprintln!("rate_limited    {}", self.rate_limited);
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
        if failed {
            ExitCode::from(1)
        } else {
            eprintln!("loadgen: PASS");
            ExitCode::SUCCESS
        }
    }
}

impl Percentiles {
    /// Exact nearest-rank percentiles over the samples (sorts in place).
    fn of(samples_us: &mut [u64]) -> Percentiles {
        samples_us.sort_unstable();
        Percentiles {
            count: samples_us.len() as u64,
            p50_ms: pct_ms(samples_us, 50.0),
            p99_ms: pct_ms(samples_us, 99.0),
            max_ms: samples_us.last().map(|u| *u as f64 / 1000.0).unwrap_or(0.0),
        }
    }
}

/// Nearest-rank percentile of a sorted slice, in milliseconds.
fn pct_ms(sorted_us: &[u64], q: f64) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let n = sorted_us.len();
    let rank = (q / 100.0 * (n as f64 - 1.0)).round() as usize;
    sorted_us[rank.min(n - 1)] as f64 / 1000.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_nearest_rank() {
        // 1..=100 us -> p50 ~= 50 us, p99 ~= 99 us, max = 100 us (in ms).
        let mut s: Vec<u64> = (1..=100).collect();
        let p = Percentiles::of(&mut s);
        assert_eq!(p.count, 100);
        assert!((p.p50_ms - 0.050).abs() < 0.002, "p50 {}", p.p50_ms);
        assert!((p.p99_ms - 0.099).abs() < 0.002, "p99 {}", p.p99_ms);
        assert!((p.max_ms - 0.100).abs() < 1e-9, "max {}", p.max_ms);
    }

    #[test]
    fn percentiles_empty_is_zero() {
        let p = Percentiles::of(&mut []);
        assert_eq!(p.count, 0);
        assert_eq!(p.p99_ms, 0.0);
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

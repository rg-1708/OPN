//! Sprint 2 exit criterion: 300 idle authenticated connections hold
//! steady-state RSS over 10 minutes (manual this sprint; automated with
//! loadgen in Sprint 10). Ignored by default — run with:
//! `cargo test --test idle_soak -- --ignored --nocapture` (~11 min).

mod common;

use std::time::Duration;

use common::ws::{connect_and_auth, mint_token, spawn_server};
use common::{app_pool, seed_world_tenant, test_config, test_state};
use sqlx::PgPool;

const CONNS: usize = 300;
const SETTLE: Duration = Duration::from_secs(60);
const WINDOW: Duration = Duration::from_secs(9 * 60);
/// "≈ 0 growth" with slack for allocator noise; a leak per connection or per
/// heartbeat tick blows well past this over 9 minutes.
const MAX_GROWTH_KB: i64 = 32 * 1024;

fn rss_kb() -> i64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|l| l.trim().trim_end_matches(" kB").trim().parse().ok())
        .expect("VmRSS in /proc/self/status")
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
#[ignore = "manual 10-minute soak (sprint 2 exit criterion)"]
async fn idle_300_connections_rss_steady(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;
    let server = spawn_server(test_state(app.clone(), test_config()).await).await;

    // Sequential connect keeps each socket inside the per-IP pre-auth cap.
    let mut holds = Vec::with_capacity(CONNS);
    for i in 0..CONNS {
        let (token, _) = mint_token(&app, tenant, world, &format!("soak-{i}")).await;
        let client = connect_and_auth(server.addr, &token).await;
        holds.push(tokio::spawn(client.hold_until_close()));
    }
    println!("{CONNS} connections authenticated; settling {SETTLE:?}");

    tokio::time::sleep(SETTLE).await;
    let start_kb = rss_kb();
    println!("RSS after settle: {start_kb} kB — holding {WINDOW:?}");
    tokio::time::sleep(WINDOW).await;
    let end_kb = rss_kb();
    println!(
        "RSS after window: {end_kb} kB (growth {} kB)",
        end_kb - start_kb
    );

    let closed = holds.iter().filter(|h| h.is_finished()).count();
    assert_eq!(closed, 0, "{closed} connections dropped during idle soak");
    assert!(
        end_kb - start_kb < MAX_GROWTH_KB,
        "RSS grew {} kB over the window (cap {MAX_GROWTH_KB} kB)",
        end_kb - start_kb
    );
}

//! Metric names are defined once, here, at startup — even at zero — so
//! dashboards never chase renames (roadmap Sprint 0 item 6).

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

pub fn register_metrics() {
    describe_gauge!("opn_connections", "Live authenticated WS connections");
    describe_counter!("opn_commands_total", "Commands processed, by cmd + outcome");
    describe_histogram!("opn_command_seconds", "Command handler latency, by cmd");
    describe_gauge!("opn_sendq_depth", "Aggregate send-queue depth");
    describe_counter!("opn_sendq_drops_total", "Send-queue drops/closes, by class");
    describe_gauge!("opn_pg_pool_in_use", "Postgres pool connections in use");
    describe_counter!("opn_inbox_inserts_total", "Notify inbox rows inserted");
    describe_counter!(
        "opn_janitor_runs_total",
        "Janitor task runs, by task + outcome"
    );

    // Touch each series once so the names render on /metrics from boot.
    // Labelled series use a "none" placeholder until real emitters land.
    gauge!("opn_connections").set(0.0);
    counter!("opn_commands_total", "cmd" => "none", "outcome" => "none").absolute(0);
    histogram!("opn_command_seconds", "cmd" => "none").record(0.0);
    gauge!("opn_sendq_depth").set(0.0);
    counter!("opn_sendq_drops_total", "class" => "none").absolute(0);
    gauge!("opn_pg_pool_in_use").set(0.0);
    counter!("opn_inbox_inserts_total").absolute(0);
    counter!("opn_janitor_runs_total", "task" => "none", "outcome" => "none").absolute(0);
}

# Prometheus alerts for OPN-CORE

`alerts.yml` holds the six §14 alerting rules. Six alerts total: the first four
fire on Core's own metrics (page severity), the last two are ops-level and
depend on external exporters.

## Wiring into Prometheus

Reference the rule file from your `prometheus.yml`:

```yaml
rule_files:
  - /etc/prometheus/alerts.yml   # mount alerts.yml here
```

Reload Prometheus (`SIGHUP` or `POST /-/reload`) after changing the rules.

## Where Core's metrics come from

Core exposes `/metrics` on a **separate listener** bound to `OPN_METRICS_BIND`,
on an internal interface only — it is NOT the public API listener and must not
be exposed externally. Scrape config:

```yaml
scrape_configs:
  - job_name: opn-core
    static_configs:
      - targets: ["<OPN_METRICS_BIND host:port>"]
```

Metric names are exact and code-verified (`opn_command_seconds_bucket`,
`opn_sendq_drops_total`, `opn_pg_pool_in_use`, `opn_janitor_runs_total`, …).

## §14 anti-goal: no dashboards-for-dashboards

One overview dashboard only. Do not spawn a dashboard per metric or per alert.
Alerts page; the single overview dashboard is for triage context. Keep it that way.

## Alerts 5–6 need external exporters (Core does not ship these)

- **HealthzDown** needs `blackbox_exporter` probing Core's `/healthz`
  (job `opn-healthz`). Without that probe the rule never fires.
- **DiskAlmostFull** needs `node_exporter` (`node_filesystem_*`) on the host.

Deploy those alongside Prometheus if you want alerts 5–6 to be live.

## Runbooks

Alert descriptions point at `docs/runbooks/incident-triage.md`; ledger-drift /
frozen-account cases point at `docs/runbooks/frozen-account.md`.

## Validation

```sh
promtool check rules alerts.yml
```

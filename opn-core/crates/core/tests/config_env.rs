//! Lives in its own integration binary: it mutates process env vars, which
//! would race any parallel test reading them.

use opn_core::config::Config;

const ALL: &[(&str, &str)] = &[
    ("OPN_BIND", "127.0.0.1:8080"),
    ("OPN_METRICS_BIND", "127.0.0.1:9090"),
    ("DATABASE_URL", "postgres://opn_app:opn@localhost:5432/opn"),
    (
        "OPN_MIGRATE_DATABASE_URL",
        "postgres://opn_migrate:opn@localhost:5432/opn",
    ),
    ("REDIS_URL", "redis://localhost:6379"),
    ("S3_ENDPOINT", "http://localhost:9000"),
    ("S3_BUCKET", "opn"),
    ("S3_KEY", "opn"),
    ("S3_SECRET", "opnsecret"),
    ("OPN_JWT_SECRET", "test-secret"),
];

#[test]
fn missing_var_error_names_the_var() {
    for (k, v) in ALL {
        std::env::set_var(k, v);
    }
    std::env::remove_var("OPN_SESSION_TTL_SECS");
    std::env::remove_var("OPN_REPLICAS");

    let cfg = Config::from_env().expect("all vars set");
    assert_eq!(cfg.session_ttl_secs, 600, "documented default");
    assert_eq!(cfg.replicas, 1, "documented default");
    assert_eq!(cfg.reconcile_hour, 3, "documented default");

    // An out-of-range reconcile hour must fail fast, not silently disable the
    // corruption detector (§10.5, adversarial review Sprint 7A).
    std::env::set_var("OPN_RECONCILE_HOUR", "24");
    let err = Config::from_env().expect_err("hour 24 is invalid");
    assert!(
        format!("{err:#}").contains("OPN_RECONCILE_HOUR"),
        "out-of-range reconcile hour must be rejected, got: {err:#}"
    );
    std::env::remove_var("OPN_RECONCILE_HOUR");

    for (missing, _) in ALL {
        for (k, v) in ALL {
            std::env::set_var(k, v);
        }
        std::env::remove_var(missing);
        let err = Config::from_env().expect_err("must fail on missing var");
        assert!(
            format!("{err:#}").contains(missing),
            "error for missing {missing} must name it, got: {err:#}"
        );
    }
}

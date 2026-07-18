//! Env-only config (OPN-CORE.md §13): read once at startup, fail-fast on the
//! first missing var, naming it. No config files, no reload.

use std::net::SocketAddr;

use anyhow::{Context, Result};

#[derive(Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub metrics_bind: SocketAddr,
    /// Runtime pool URL — the non-BYPASSRLS `opn_app` role.
    pub database_url: String,
    /// Owner-role URL, used only to run migrations at startup (the app role
    /// cannot own tables, or FORCE RLS would be meaningless).
    pub migrate_database_url: String,
    pub redis_url: String,
    pub s3_endpoint: String,
    pub s3_bucket: String,
    pub s3_key: String,
    pub s3_secret: String,
    pub jwt_secret: String,
    pub session_ttl_secs: u64,
    /// >1 enables the Redis pub/sub fan-out path.
    pub replicas: u32,
    /// Per-connection send-queue depth (§4.3). Prod default 256; tests set it
    /// tiny to exercise backpressure without generating thousands of events.
    pub sendq_capacity: usize,
    /// Pre-auth connection caps (§4.1): sockets that have not yet sent a
    /// valid `auth` frame.
    pub preauth_global_max: u32,
    pub preauth_per_ip_max: u8,
    /// WS ping interval; close after 2 missed pongs (§4.1). Configurable so
    /// the missed-pong test does not take a minute.
    pub heartbeat_secs: u64,
}

fn req(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing required env var {name}"))
}

fn parse<T: std::str::FromStr>(name: &str, raw: String) -> Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    raw.parse()
        .with_context(|| format!("invalid value for env var {name}"))
}

/// Optional var with a documented default.
fn opt<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(name) {
        Ok(v) => parse(name, v),
        Err(_) => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> Result<Config> {
        Ok(Config {
            bind: parse("OPN_BIND", req("OPN_BIND")?)?,
            metrics_bind: parse("OPN_METRICS_BIND", req("OPN_METRICS_BIND")?)?,
            database_url: req("DATABASE_URL")?,
            migrate_database_url: req("OPN_MIGRATE_DATABASE_URL")?,
            redis_url: req("REDIS_URL")?,
            s3_endpoint: req("S3_ENDPOINT")?,
            s3_bucket: req("S3_BUCKET")?,
            s3_key: req("S3_KEY")?,
            s3_secret: req("S3_SECRET")?,
            jwt_secret: req("OPN_JWT_SECRET")?,
            session_ttl_secs: match std::env::var("OPN_SESSION_TTL_SECS") {
                Ok(v) => parse("OPN_SESSION_TTL_SECS", v)?,
                Err(_) => 600,
            },
            replicas: match std::env::var("OPN_REPLICAS") {
                Ok(v) => parse("OPN_REPLICAS", v)?,
                Err(_) => 1,
            },
            sendq_capacity: opt("OPN_SENDQ_CAPACITY", 256)?,
            preauth_global_max: opt("OPN_PREAUTH_GLOBAL_MAX", 1000)?,
            preauth_per_ip_max: opt("OPN_PREAUTH_PER_IP_MAX", 5)?,
            heartbeat_secs: opt("OPN_HEARTBEAT_SECS", 30)?,
        })
    }
}

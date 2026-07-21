//! Env-only config (OPN-CORE.md §13): read once at startup, fail-fast on the
//! first missing var, naming it. No config files, no reload.

use std::net::SocketAddr;

use anyhow::{Context, Result};

/// Admin panel surface (opn-panel-roadmap.md Sprint P0). `Some` only when both
/// secrets are set — otherwise the admin router is disabled and existing
/// deploys keep working unchanged.
#[derive(Debug)]
pub struct AdminConfig {
    /// Private bind, loopback/VPN-only. Startup refuses a value equal to the
    /// public or metrics bind (cross-cutting rule 1).
    pub bind: SocketAddr,
    /// argon2id PHC string; the login password is verified against it.
    pub password_hash: String,
    /// Signing key for admin JWTs — SEPARATE from `jwt_secret` so an admin
    /// token and a tenant session token can never verify as each other.
    pub jwt_secret: String,
}

/// LiveKit SFU config (opn-group-calls.md G1). `Some` only when the three
/// LIVEKIT_* vars are set — otherwise group calls are disabled (the four
/// `calls.group.*` commands fail `forbidden` and the webhook 404s), so a deploy
/// without an SFU keeps 1:1 calls and the data plane working unchanged.
#[derive(Debug)]
pub struct LivekitConfig {
    /// SFU URL handed to the client in the join ack (dev: `ws://localhost:7880`).
    pub url: String,
    /// API key — the `iss` of every minted access token and the webhook's
    /// signing identity.
    pub api_key: String,
    /// API secret — HS256 signing key for access tokens AND webhook signature
    /// verification (the shared secret is the trust boundary, §G1).
    pub api_secret: String,
    /// Janitor reaps an active SFU room empty this long (default 300 s).
    pub empty_room_reap_secs: i64,
    /// Server cap on group-room participants (default 32, matching the channel
    /// member cap). A `calls.group.create` `max_participants` above this is
    /// clamped, not rejected.
    pub max_participants_default: i64,
    /// Anti-abuse ceiling on concurrent active group rooms per tenant (world).
    /// `calls.group.create` past this answers `conflict` — end a room to free a
    /// slot. Default 50; tune down on small hosts (env `LIVEKIT_MAX_ROOMS`).
    pub max_rooms: i64,
}

#[derive(Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub metrics_bind: SocketAddr,
    /// `Some` enables the admin router on its own bind (Sprint P0).
    pub admin: Option<AdminConfig>,
    /// `Some` enables group calls against the LiveKit sidecar (Sprint G1).
    pub livekit: Option<LivekitConfig>,
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
    /// SigV4 region label. MinIO ignores it but still requires it in the
    /// signature scope; real S3 needs the bucket's region.
    pub s3_region: String,
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
    /// `u32`, not `u8`: many pre-auth sockets can share one source IP behind a
    /// reverse proxy / NAT — and the perf smoke drives 300 loadgen connections
    /// from localhost — so the ceiling must exceed 255 (both the config value and
    /// the per-IP counter in `ws.rs` are `u32`).
    pub preauth_per_ip_max: u32,
    /// WS ping interval; close after 2 missed pongs (§4.1). Configurable so
    /// the missed-pong test does not take a minute.
    pub heartbeat_secs: u64,
    /// Static WebRTC ICE servers echoed into every `calls.state` snapshot (§5,
    /// §10.4). A JSON array of `RTCIceServer` objects (`OPN_ICE_SERVERS`);
    /// defaults to `[]` (P2P/STUN-less). Parsed once at startup — malformed JSON
    /// aborts.
    pub ice_servers: serde_json::Value,
    /// Hour-of-day (UTC, 0..=23) the nightly ledger reconciliation runs (§10.5).
    /// The janitor still ticks every 30 s; this gates the per-account recompute to
    /// one hour a day. Default 3 (03:00 UTC).
    pub reconcile_hour: u32,
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
        // Validate the reconcile hour up front: `hour()` only ever returns 0..=23,
        // so a value ≥ 24 (a typo, or "24" meaning midnight) would make the gate
        // never fire — silently disabling the ONLY silent-corruption detector.
        // Fail fast instead (§10.5, adversarial review Sprint 7A).
        let reconcile_hour = opt("OPN_RECONCILE_HOUR", 3)?;
        if reconcile_hour > 23 {
            anyhow::bail!("OPN_RECONCILE_HOUR must be 0..=23, got {reconcile_hour}");
        }
        let bind: SocketAddr = parse("OPN_BIND", req("OPN_BIND")?)?;
        let metrics_bind: SocketAddr = parse("OPN_METRICS_BIND", req("OPN_METRICS_BIND")?)?;

        // Admin panel (Sprint P0): enabled only when both secrets are present.
        // Absent → None → router disabled (main logs one line), so deploys that
        // never set these keep running exactly as before. Empty counts as
        // absent — compose `${VAR:-}` expansions must not half-enable this.
        let nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let admin = match (
            nonempty("ADMIN_PASSWORD_HASH"),
            nonempty("ADMIN_JWT_SECRET"),
        ) {
            (Some(password_hash), Some(jwt_secret)) => {
                let admin_bind: SocketAddr =
                    opt("ADMIN_BIND", SocketAddr::from(([127, 0, 0, 1], 9091)))?;
                // Cross-cutting rule 1: the admin surface never rides the public
                // or metrics bind. Fail fast, do not silently co-locate it.
                if admin_bind == bind || admin_bind == metrics_bind {
                    anyhow::bail!(
                        "ADMIN_BIND {admin_bind} must differ from OPN_BIND and OPN_METRICS_BIND"
                    );
                }
                Some(AdminConfig {
                    bind: admin_bind,
                    password_hash,
                    jwt_secret,
                })
            }
            _ => None,
        };

        // LiveKit / group calls (Sprint G1): enabled only when all three vars
        // are present. Absent → None → group calls disabled (main logs one
        // line), 1:1 calls and the data plane unaffected.
        let livekit = match (
            nonempty("LIVEKIT_URL"),
            nonempty("LIVEKIT_API_KEY"),
            nonempty("LIVEKIT_API_SECRET"),
        ) {
            (Some(url), Some(api_key), Some(api_secret)) => Some(LivekitConfig {
                url,
                api_key,
                api_secret,
                empty_room_reap_secs: opt("LIVEKIT_EMPTY_ROOM_REAP_SECS", 300)?,
                max_participants_default: opt("LIVEKIT_MAX_PARTICIPANTS", 32)?,
                max_rooms: opt("LIVEKIT_MAX_ROOMS", 50)?,
            }),
            _ => None,
        };

        Ok(Config {
            bind,
            metrics_bind,
            admin,
            livekit,
            database_url: req("DATABASE_URL")?,
            migrate_database_url: req("OPN_MIGRATE_DATABASE_URL")?,
            redis_url: req("REDIS_URL")?,
            s3_endpoint: req("S3_ENDPOINT")?,
            s3_bucket: req("S3_BUCKET")?,
            s3_key: req("S3_KEY")?,
            s3_secret: req("S3_SECRET")?,
            s3_region: std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
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
            ice_servers: match std::env::var("OPN_ICE_SERVERS") {
                Ok(v) => serde_json::from_str(&v).context("invalid JSON in OPN_ICE_SERVERS")?,
                Err(_) => serde_json::json!([]),
            },
            reconcile_hour,
        })
    }
}

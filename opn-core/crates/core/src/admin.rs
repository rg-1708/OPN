//! Admin CLI subcommands (roadmap Sprint 1 item 2). `create-tenant` mints a
//! world (or reuses one) and a tenant, prints the raw API key exactly once,
//! and stores only its sha256 hash — the key is never recoverable after.

use anyhow::{anyhow, bail, Context, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use crate::infra::auth::{api_key_hash, generate_api_key};
use crate::infra::ids::new_id;

const USAGE: &str = "usage: admin create-tenant --name <tenant-name> \
                     (--world <uuid> | --new-world <world-name>)";
const UNFREEZE_USAGE: &str = "usage: admin unfreeze --world <uuid> --account <uuid>";

/// Entry point for `argv[1] == "admin"`. Hand-parsed (no clap).
pub async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create-tenant") => create_tenant(&args[1..]).await,
        Some("unfreeze") => unfreeze(&args[1..]).await,
        _ => bail!("{USAGE}\n{UNFREEZE_USAGE}"),
    }
}

/// argon2id with default params — the same verifier config
/// `http::admin::verify_password` uses, so a hash minted here always verifies.
/// Used by the panel's first-launch setup endpoint (`POST /admin/v1/setup`).
pub fn hash_admin_password(password: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use rand::RngCore;
    // Salt via the workspace `rand` (0.9) — argon2's own OsRng re-export rides
    // rand_core 0.6, which this crate doesn't pull in.
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow!("encode salt: {e}"))?;
    let hash = argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("hash password: {e}"))?;
    Ok(hash.to_string())
}

async fn create_tenant(args: &[String]) -> Result<()> {
    let mut name: Option<String> = None;
    let mut world: Option<String> = None;
    let mut new_world: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1);
        match args[i].as_str() {
            "--name" => name = Some(val.ok_or_else(|| anyhow!("{USAGE}"))?.clone()),
            "--world" => world = Some(val.ok_or_else(|| anyhow!("{USAGE}"))?.clone()),
            "--new-world" => new_world = Some(val.ok_or_else(|| anyhow!("{USAGE}"))?.clone()),
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
        i += 2;
    }

    let name = name.ok_or_else(|| anyhow!("{USAGE}"))?;
    if world.is_some() == new_world.is_some() {
        // both or neither
        bail!("{USAGE}");
    }

    // Owner role: opn_app cannot INSERT into tenants/worlds. No migrations here.
    let url = std::env::var("OPN_MIGRATE_DATABASE_URL")
        .context("missing required env var OPN_MIGRATE_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .context("connect owner pool")?;

    // One transaction for world + tenant: a failed tenant insert must not
    // leave an orphan world behind.
    let mut tx = pool.begin().await.context("begin tenant create")?;
    let world_id: Uuid = if let Some(world_name) = new_world {
        create_world(&mut *tx, &world_name).await?
    } else {
        let raw = world.ok_or_else(|| anyhow!("{USAGE}"))?;
        let id = Uuid::parse_str(&raw).context("--world is not a valid uuid")?;
        let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM worlds WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .context("world lookup")?;
        if exists.is_none() {
            bail!("no such world: {id}");
        }
        // One tenant per world (§5): the tenant link is keyed by world, so a
        // second tenant on the same world would silently take over its link.
        // Enforce the invariant where tenants are born; multi-tenant hosting
        // (§17) will need a different link keying before lifting this.
        let has_tenant: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM tenants WHERE world_id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .context("world tenant lookup")?;
        if has_tenant.is_some() {
            bail!("world {id} already has a tenant (one tenant per world, §5)");
        }
        id
    };

    let created = insert_tenant(&mut *tx, &name, world_id).await?;
    tx.commit().await.context("commit tenant create")?;

    println!("tenant id: {}", created.tenant_id);
    println!("world id:  {world_id}");
    println!("api key:   {}", created.raw_key);
    println!("^ shown once — only its sha256 hash is stored, save it now.");
    Ok(())
}

/// A freshly-created tenant. `raw_key` is the ONE-TIME plaintext API key — the
/// caller surfaces it exactly once (CLI stdout / HTTP response body) and must
/// never log it or write it to audit. `fingerprint` (first 8 hex of the hash)
/// is the safe-to-persist handle (cross-cutting rule 2).
pub struct CreatedTenant {
    pub tenant_id: Uuid,
    pub raw_key: String,
    pub fingerprint: String,
}

/// Insert a new world (owner pool), returning its id. Shared by CLI
/// `--new-world` and the admin-panel create path (one tenant per world, §5).
pub async fn create_world(exec: impl sqlx::PgExecutor<'_>, name: &str) -> Result<Uuid> {
    let (id,): (Uuid,) =
        sqlx::query_as("INSERT INTO worlds (id, name) VALUES ($1, $2) RETURNING id")
            .bind(new_id())
            .bind(name)
            .fetch_one(exec)
            .await
            .context("insert world")?;
    Ok(id)
}

/// Mint an API key and insert the tenant row for `world_id` (owner pool). The
/// SINGLE key-minting + tenant-insert path — CLI `create-tenant` and the admin
/// panel POST both call it, so key generation/hashing/insert never diverge.
/// Returns the raw key exactly once (see [`CreatedTenant`]).
pub async fn insert_tenant(
    exec: impl sqlx::PgExecutor<'_>,
    name: &str,
    world_id: Uuid,
) -> Result<CreatedTenant> {
    let raw_key = generate_api_key();
    let hash = api_key_hash(&raw_key);
    // sha256 hex is always 64 chars, so this slice never panics.
    let fingerprint = hash[..8].to_string();
    let tenant_id = new_id();
    sqlx::query("INSERT INTO tenants (id, name, api_key_hash, world_id) VALUES ($1, $2, $3, $4)")
        .bind(tenant_id)
        .bind(name)
        .bind(&hash)
        .bind(world_id)
        .execute(exec)
        .await
        .context("insert tenant")?;
    Ok(CreatedTenant {
        tenant_id,
        raw_key,
        fingerprint,
    })
}

/// The plaintext + fingerprint of a rotated key. Same one-time-key contract as
/// [`CreatedTenant`]: `raw_key` is surfaced once and never logged.
pub struct RotatedKey {
    pub fingerprint: String,
    pub raw_key: String,
}

/// Rotate a tenant's API key: mint a new key, overwrite `api_key_hash` (owner
/// pool). Immediate-cut — the old key stops authenticating the instant the row
/// updates (grace-period dual-key is gated, roadmap §Admin API surface). Live
/// sessions are unaffected: they hold JWTs verified against the `sessions` row,
/// not the key, so they run until they expire (v1 known limit — no session kill).
/// Returns `None` if no tenant has `id` (→ 404). Admin-panel only; the CLI has
/// no rotate command.
pub async fn rotate_tenant_key(pool: &PgPool, id: Uuid) -> Result<Option<RotatedKey>> {
    let raw_key = generate_api_key();
    let hash = api_key_hash(&raw_key);
    let fingerprint = hash[..8].to_string();
    let res = sqlx::query("UPDATE tenants SET api_key_hash = $2 WHERE id = $1")
        .bind(id)
        .bind(&hash)
        .execute(pool)
        .await
        .context("rotate tenant key")?;
    if res.rows_affected() == 0 {
        return Ok(None);
    }
    Ok(Some(RotatedKey {
        fingerprint,
        raw_key,
    }))
}

/// Set/clear a tenant's freeze (owner pool). `frozen = true` sets `frozen_at =
/// now()`, refusing NEW session mints (enforced in `identity::mint_session`);
/// `false` clears it. Returns rows affected — 0 means no such tenant (→ 404).
/// v1 does not kill a frozen tenant's already-live sessions (known limit).
/// Distinct from `unfreeze_account` below, which thaws a per-account balance
/// freeze, not the tenant key.
pub async fn set_tenant_frozen(pool: &PgPool, id: Uuid, frozen: bool) -> Result<u64> {
    // Set to now() only when not already frozen (idempotent-ish); clear to NULL.
    let sql = if frozen {
        "UPDATE tenants SET frozen_at = now() WHERE id = $1"
    } else {
        "UPDATE tenants SET frozen_at = NULL WHERE id = $1"
    };
    let res = sqlx::query(sql)
        .bind(id)
        .execute(pool)
        .await
        .context("set tenant frozen")?;
    Ok(res.rows_affected())
}

/// Outcome of a tenant delete — the three cases the handler maps to 200/404/409.
pub enum DeleteOutcome {
    NotFound,
    /// The tenant has ≥ 1 live session (`revoked_at IS NULL AND expires_at >
    /// now()`) — refuse, so a live client can't be nuked by accident. Freeze it
    /// and let the sessions expire, then delete.
    HasLiveSessions,
    /// Deleted. Carries safe handles (never the raw key) for the audit row.
    Deleted {
        name: String,
        fingerprint: String,
    },
}

/// Hard-delete a tenant and its key (owner pool, one transaction). Refuses if the
/// tenant has any live session. Expired/revoked session rows are cleared first
/// (they FK-block the delete, sessions.tenant_id NOT NULL), and this tenant's
/// `admin_audit` rows are unlinked (`target_tenant` → NULL) so the operator trail
/// survives. Irreversible — the key hash is gone; issue a fresh tenant to restore
/// a client. Admin-panel only; the CLI has no delete.
pub async fn delete_tenant(pool: &PgPool, id: Uuid) -> Result<DeleteOutcome> {
    let mut tx = pool.begin().await.context("begin delete tenant")?;

    // Lock the row + read safe handles; absent → NotFound. FOR UPDATE serialises
    // against a concurrent rotate/freeze on the same tenant.
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT name, api_key_hash FROM tenants WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .context("lock tenant for delete")?;
    let Some((name, hash)) = row else {
        return Ok(DeleteOutcome::NotFound);
    };

    // Guard: any live session blocks the delete.
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sessions \
         WHERE tenant_id = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .context("count live sessions")?;
    if live > 0 {
        return Ok(DeleteOutcome::HasLiveSessions);
    }

    // Preserve the audit trail (unlink), clear FK-blocking session rows, drop it.
    sqlx::query("UPDATE admin_audit SET target_tenant = NULL WHERE target_tenant = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("unlink audit rows")?;
    sqlx::query("DELETE FROM sessions WHERE tenant_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("delete tenant sessions")?;
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("delete tenant")?;

    // Audit the deletion in the same tx. target_tenant is NULL — the tenant is
    // gone, so its id/name/fingerprint live in `detail` (fingerprint, never the
    // raw key — cross-cutting rule 2).
    let fingerprint = hash[..8].to_string();
    sqlx::query(
        "INSERT INTO admin_audit (action, target_tenant, detail) \
         VALUES ('tenant.delete', NULL, $1)",
    )
    .bind(serde_json::json!({ "id": id, "name": &name, "fingerprint": &fingerprint }))
    .execute(&mut *tx)
    .await
    .context("audit tenant.delete")?;

    tx.commit().await.context("commit delete tenant")?;
    Ok(DeleteOutcome::Deleted { name, fingerprint })
}

/// `admin unfreeze --world <uuid> --account <uuid>` (roadmap Sprint 7 item 7 /
/// Sprint 11 item 5). The deliberate human gate: nightly reconciliation freezes
/// a drifted account (`accounts.frozen_at`, rejecting outgoing ops); after an
/// operator confirms the true balance (see `docs/runbooks/frozen-account.md`),
/// this clears the freeze. Runs as the owner role (bypasses RLS) so the world is
/// scoped explicitly, and only ever thaws a *currently-frozen* account.
async fn unfreeze(args: &[String]) -> Result<()> {
    let mut world: Option<String> = None;
    let mut account: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1);
        match args[i].as_str() {
            "--world" => world = Some(val.ok_or_else(|| anyhow!("{UNFREEZE_USAGE}"))?.clone()),
            "--account" => account = Some(val.ok_or_else(|| anyhow!("{UNFREEZE_USAGE}"))?.clone()),
            other => bail!("unknown argument {other}\n{UNFREEZE_USAGE}"),
        }
        i += 2;
    }

    let world_id = Uuid::parse_str(&world.ok_or_else(|| anyhow!("{UNFREEZE_USAGE}"))?)
        .context("--world is not a valid uuid")?;
    let account_id = Uuid::parse_str(&account.ok_or_else(|| anyhow!("{UNFREEZE_USAGE}"))?)
        .context("--account is not a valid uuid")?;

    // Owner role: opn_app cannot UPDATE another world's accounts under RLS, and
    // this is an operator break-glass, not a request path. Same role as migrations.
    let url = std::env::var("OPN_MIGRATE_DATABASE_URL")
        .context("missing required env var OPN_MIGRATE_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .context("connect owner pool")?;

    if unfreeze_account(&pool, world_id, account_id).await? == 0 {
        bail!("no frozen account {account_id} in world {world_id} (already thawed or absent)");
    }
    println!("unfroze account {account_id} in world {world_id}");
    Ok(())
}

/// Clear `frozen_at` on a currently-frozen account, returning rows affected
/// (0 = not frozen or not found). The DB half of `admin unfreeze`, split out so
/// `#[sqlx::test]` can drive it against a seeded account without the CLI. Owner
/// role assumed (RLS-bypassing), so the world is matched explicitly, not via
/// `app.world_id`.
pub async fn unfreeze_account(pool: &PgPool, world_id: Uuid, account_id: Uuid) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE accounts SET frozen_at = NULL \
         WHERE world_id = $1 AND id = $2 AND frozen_at IS NOT NULL",
    )
    .bind(world_id)
    .bind(account_id)
    .execute(pool)
    .await
    .context("unfreeze account")?;
    Ok(res.rows_affected())
}

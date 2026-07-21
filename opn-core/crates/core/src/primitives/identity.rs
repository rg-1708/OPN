//! identity primitive (OPN-CORE.md §10.1): session mint (character upsert +
//! number assignment + device + session row), settings, me, app_login.
//!
//! WS command handlers here are plain `pub async fn`s; dispatch wires them in
//! Sprint 2. Everything runs inside `world_tx` (RLS convention).

use anyhow::anyhow;
use contracts::types::{AppAccountInfo, CharacterInfo, DeviceInfo, MePayload};
use contracts::{cmd::SettingsScope, ErrCode};
use rand::Rng;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::Fail;
use crate::infra::auth::Identity;
use crate::infra::db::world_tx;
use crate::infra::ids::new_id;

pub struct Minted {
    pub identity: Identity,
    pub character: CharacterInfo,
    pub device: DeviceInfo,
}

#[derive(sqlx::FromRow)]
struct CharRow {
    id: Uuid,
    framework_ref: String,
    number: Option<String>,
    share_presence: bool,
}

#[derive(sqlx::FromRow)]
struct DeviceRow {
    id: Uuid,
    kind: String,
}

/// The whole mint (§6, OPN.md §3) in one transaction: upsert character by
/// `(world_id, framework_ref)`, assign a number on first sight, resolve or
/// create the device, insert the session row.
pub async fn mint_session(
    pool: &PgPool,
    tenant_id: Uuid,
    world_id: Uuid,
    framework_ref: &str,
    device_id: Option<Uuid>,
    session_ttl_secs: u64,
) -> Result<Minted, Fail> {
    let mut tx = world_tx(pool, world_id).await?;

    // Frozen tenants (admin panel, opn-panel-roadmap.md P1) cannot mint NEW
    // sessions. Checked before any row is written; already-live sessions are
    // unaffected — they hold JWTs verified against the `sessions` row, not the
    // tenant, so they run until they expire (v1 known limit — no session kill).
    let frozen: bool = sqlx::query_scalar("SELECT frozen_at IS NOT NULL FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_one(&mut *tx)
        .await?;
    if frozen {
        return Err(Fail::Code(ErrCode::Forbidden));
    }

    let ch: CharRow = sqlx::query_as(
        "INSERT INTO characters (id, world_id, framework_ref) VALUES ($1, $2, $3) \
         ON CONFLICT (world_id, framework_ref) DO UPDATE SET last_seen_at = now() \
         RETURNING id, framework_ref, number, share_presence",
    )
    .bind(new_id())
    .bind(world_id)
    .bind(framework_ref)
    .fetch_one(&mut *tx)
    .await?;

    let number = match ch.number {
        Some(n) => n,
        None => assign_number(&mut tx, world_id, ch.id).await?,
    };

    let device: DeviceRow = match device_id {
        Some(d) => {
            sqlx::query_as("SELECT id, kind FROM devices WHERE id = $1 AND owner_character = $2")
                .bind(d)
                .bind(ch.id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Fail::Code(ErrCode::NotFound))?
        }
        // No device given: reuse the character's first device, create one on
        // first sight — mint must not spawn a device per login.
        None => {
            let existing: Option<DeviceRow> = sqlx::query_as(
                "SELECT id, kind FROM devices WHERE owner_character = $1 ORDER BY created_at LIMIT 1",
            )
            .bind(ch.id)
            .fetch_optional(&mut *tx)
            .await?;
            match existing {
                Some(d) => d,
                None => {
                    sqlx::query_as(
                        "INSERT INTO devices (id, world_id, owner_character, kind) \
                     VALUES ($1, $2, $3, 'phone') RETURNING id, kind",
                    )
                    .bind(new_id())
                    .bind(world_id)
                    .bind(ch.id)
                    .fetch_one(&mut *tx)
                    .await?
                }
            }
        }
    };

    let session_id = new_id();
    sqlx::query(
        "INSERT INTO sessions (id, tenant_id, world_id, character_id, device_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))",
    )
    .bind(session_id)
    .bind(tenant_id)
    .bind(world_id)
    .bind(ch.id)
    .bind(device.id)
    .bind(session_ttl_secs as f64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Minted {
        identity: Identity::for_new_session(session_id, tenant_id, world_id, ch.id, device.id),
        character: CharacterInfo {
            id: ch.id,
            framework_ref: ch.framework_ref,
            number: Some(number),
            share_presence: ch.share_presence,
        },
        device: DeviceInfo {
            id: device.id,
            kind: device.kind,
        },
    })
}

/// Number assignment (roadmap Sprint 1 item 3): candidate from the world
/// pattern, one-statement conditional UPDATE, retry on collision, max 10.
///
/// The partial unique index `(world_id, number) WHERE number IS NOT NULL`
/// catches concurrent duplicates; a unique violation aborts the statement,
/// so each attempt runs under a SAVEPOINT to keep the enclosing tx alive.
// ponytail: pattern hardcoded 555-XXXX (10k numbers/world) — worlds get a
// pattern column when a real deployment outgrows it.
async fn assign_number(
    tx: &mut Transaction<'_, Postgres>,
    world_id: Uuid,
    character_id: Uuid,
) -> Result<String, Fail> {
    for _ in 0..10 {
        let cand = format!("555-{:04}", rand::rng().random_range(0..10_000));
        sqlx::query("SAVEPOINT assign_number")
            .execute(&mut **tx)
            .await?;
        let res = sqlx::query(
            "UPDATE characters SET number = $2 \
             WHERE id = $1 AND number IS NULL \
               AND NOT EXISTS (SELECT 1 FROM retired_numbers r \
                               WHERE r.world_id = $3 AND r.number = $2 \
                                 AND r.freed_at > now() - interval '30 days')",
        )
        .bind(character_id)
        .bind(&cand)
        .bind(world_id)
        .execute(&mut **tx)
        .await;

        match res {
            Ok(r) if r.rows_affected() == 1 => {
                sqlx::query("RELEASE SAVEPOINT assign_number")
                    .execute(&mut **tx)
                    .await?;
                return Ok(cand);
            }
            Ok(_) => {
                sqlx::query("RELEASE SAVEPOINT assign_number")
                    .execute(&mut **tx)
                    .await?;
                // Zero rows: either a concurrent mint already assigned this
                // character a number (done), or the candidate is in cooldown
                // (retry with a fresh one).
                let existing: Option<String> =
                    sqlx::query_scalar("SELECT number FROM characters WHERE id = $1")
                        .bind(character_id)
                        .fetch_one(&mut **tx)
                        .await?;
                if let Some(n) = existing {
                    return Ok(n);
                }
            }
            Err(e) if is_unique_violation(&e) => {
                sqlx::query("ROLLBACK TO SAVEPOINT assign_number")
                    .execute(&mut **tx)
                    .await?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    // A full number space is an operator problem, not a retry problem (§10.1).
    tracing::error!(%world_id, "number assignment exhausted 10 attempts — number space likely full");
    Err(Fail::Internal(anyhow!(
        "number assignment exhausted for world {world_id}"
    )))
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

/// 16 KB cap on the opaque settings document (§10.1) — Core validates size
/// only; the schema belongs to the shell/SDK.
const SETTINGS_MAX_BYTES: usize = 16 * 1024;

pub async fn get_settings(
    pool: &PgPool,
    who: &Identity,
    scope: SettingsScope,
) -> Result<serde_json::Value, Fail> {
    let mut tx = world_tx(pool, who.world_id).await?;
    let (sql, id) = match scope {
        SettingsScope::Device => ("SELECT settings FROM devices WHERE id = $1", who.device_id),
        SettingsScope::Character => (
            "SELECT settings FROM characters WHERE id = $1",
            who.character_id,
        ),
    };
    let settings: serde_json::Value = sqlx::query_scalar(sql).bind(id).fetch_one(&mut *tx).await?;
    Ok(settings)
}

/// Whole-document replace, not a merge patch — the client owns the document.
pub async fn set_settings(
    pool: &PgPool,
    who: &Identity,
    scope: SettingsScope,
    patch: serde_json::Value,
) -> Result<(), Fail> {
    let size = serde_json::to_vec(&patch)
        .map_err(|e| Fail::Internal(e.into()))?
        .len();
    if size > SETTINGS_MAX_BYTES {
        return Err(Fail::Code(ErrCode::TooLarge));
    }
    let mut tx = world_tx(pool, who.world_id).await?;
    let (sql, id) = match scope {
        SettingsScope::Device => (
            "UPDATE devices SET settings = $2 WHERE id = $1",
            who.device_id,
        ),
        SettingsScope::Character => (
            "UPDATE characters SET settings = $2 WHERE id = $1",
            who.character_id,
        ),
    };
    sqlx::query(sql)
        .bind(id)
        .bind(&patch)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Dedicated command because `share_presence` gates server behavior (§10.1)
/// — it never rides in the opaque settings blob.
pub async fn set_share_presence(pool: &PgPool, who: &Identity, on: bool) -> Result<(), Fail> {
    let mut tx = world_tx(pool, who.world_id).await?;
    sqlx::query("UPDATE characters SET share_presence = $2 WHERE id = $1")
        .bind(who.character_id)
        .bind(on)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn me(pool: &PgPool, who: &Identity) -> Result<MePayload, Fail> {
    let mut tx = world_tx(pool, who.world_id).await?;
    let ch: CharRow = sqlx::query_as(
        "SELECT id, framework_ref, number, share_presence FROM characters WHERE id = $1",
    )
    .bind(who.character_id)
    .fetch_one(&mut *tx)
    .await?;
    let device: DeviceRow = sqlx::query_as("SELECT id, kind FROM devices WHERE id = $1")
        .bind(who.device_id)
        .fetch_one(&mut *tx)
        .await?;
    let accounts: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, app_id, handle FROM app_accounts WHERE character_id = $1 ORDER BY created_at",
    )
    .bind(who.character_id)
    .fetch_all(&mut *tx)
    .await?;
    let active_accounts: serde_json::Value =
        sqlx::query_scalar("SELECT app_accounts FROM sessions WHERE id = $1")
            .bind(who.session_id)
            .fetch_one(&mut *tx)
            .await?;
    Ok(MePayload {
        character: CharacterInfo {
            id: ch.id,
            framework_ref: ch.framework_ref,
            number: ch.number,
            share_presence: ch.share_presence,
        },
        device: DeviceInfo {
            id: device.id,
            kind: device.kind,
        },
        accounts: accounts
            .into_iter()
            .map(|(id, app_id, handle)| AppAccountInfo { id, app_id, handle })
            .collect(),
        active_accounts,
    })
}

/// Switch the active app account — stored on the *session row*, per session,
/// not per app code (OPN.md §3).
pub async fn app_login(
    pool: &PgPool,
    who: &Identity,
    app_id: &str,
    account_id: Uuid,
) -> Result<(), Fail> {
    let mut tx = world_tx(pool, who.world_id).await?;
    let owned: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM app_accounts WHERE id = $1 AND character_id = $2 AND app_id = $3",
    )
    .bind(account_id)
    .bind(who.character_id)
    .bind(app_id)
    .fetch_optional(&mut *tx)
    .await?;
    if owned.is_none() {
        return Err(Fail::Code(ErrCode::Forbidden));
    }
    sqlx::query(
        "UPDATE sessions SET app_accounts = app_accounts || jsonb_build_object($2::text, $3::text) \
         WHERE id = $1",
    )
    .bind(who.session_id)
    .bind(app_id)
    .bind(account_id.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

//! Admin CLI subcommands (roadmap Sprint 1 item 2). `create-tenant` mints a
//! world (or reuses one) and a tenant, prints the raw API key exactly once,
//! and stores only its sha256 hash — the key is never recoverable after.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::infra::auth::api_key_hash;
use crate::infra::ids::new_id;

const USAGE: &str = "usage: admin create-tenant --name <tenant-name> \
                     (--world <uuid> | --new-world <world-name>)";

/// Entry point for `argv[1] == "admin"`. Hand-parsed (no clap) — one command.
pub async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create-tenant") => create_tenant(&args[1..]).await,
        _ => bail!("{USAGE}"),
    }
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

    let world_id: Uuid = if let Some(world_name) = new_world {
        let (id,): (Uuid,) =
            sqlx::query_as("INSERT INTO worlds (id, name) VALUES ($1, $2) RETURNING id")
                .bind(new_id())
                .bind(&world_name)
                .fetch_one(&pool)
                .await
                .context("insert world")?;
        id
    } else {
        let raw = world.ok_or_else(|| anyhow!("{USAGE}"))?;
        let id = Uuid::parse_str(&raw).context("--world is not a valid uuid")?;
        let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM worlds WHERE id = $1")
            .bind(id)
            .fetch_optional(&pool)
            .await
            .context("world lookup")?;
        if exists.is_none() {
            bail!("no such world: {id}");
        }
        id
    };

    // 32 bytes of entropy → url-safe base64 (43 chars). High-entropy key, so
    // the stored sha256 is the whole auth (auth.rs / §11).
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf);
    let key = format!("opn_{b64}");

    let tenant_id = new_id();
    sqlx::query("INSERT INTO tenants (id, name, api_key_hash, world_id) VALUES ($1, $2, $3, $4)")
        .bind(tenant_id)
        .bind(&name)
        .bind(api_key_hash(&key))
        .bind(world_id)
        .execute(&pool)
        .await
        .context("insert tenant")?;

    println!("tenant id: {tenant_id}");
    println!("world id:  {world_id}");
    println!("api key:   {key}");
    println!("^ shown once — only its sha256 hash is stored, save it now.");
    Ok(())
}

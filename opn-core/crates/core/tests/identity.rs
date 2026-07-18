//! Sprint 1 test plan: character upsert idempotency, number uniqueness under
//! 32 concurrent mints, cooldown exclusion, session revocation, JWT
//! rejection paths, settings/me/app_login handlers. All store assertions run
//! as `opn_app` (RLS on — cross-cutting rule 4).

mod common;

use common::{app_pool, seed_world_tenant};
use contracts::cmd::SettingsScope;
use contracts::ErrCode;
use opn_core::infra::auth::{mint_jwt, verify, VerifyError};
use opn_core::infra::db::world_tx;
use opn_core::primitives::identity::{self, Minted};
use opn_core::primitives::Fail;
use sqlx::PgPool;
use uuid::Uuid;

const TTL: u64 = 600;

async fn mint(app: &PgPool, tenant: Uuid, world: Uuid, fref: &str) -> Result<Minted, Fail> {
    identity::mint_session(app, tenant, world, fref, None, TTL).await
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn mint_is_idempotent_and_reuses_number(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;

    let first = mint(&app, tenant, world, "char-1")
        .await
        .expect("first mint");
    let second = mint(&app, tenant, world, "char-1")
        .await
        .expect("second mint");

    assert_eq!(
        first.character.id, second.character.id,
        "same character row"
    );
    assert_eq!(
        first.device.id, second.device.id,
        "device reused, not spawned per login"
    );
    assert_eq!(
        first.character.number, second.character.number,
        "number held for life"
    );
    assert!(first
        .character
        .number
        .as_deref()
        .expect("assigned")
        .starts_with("555-"));
    assert_ne!(
        first.identity.session_id, second.identity.session_id,
        "new session per mint"
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn concurrent_mints_get_distinct_numbers(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 8).await;

    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..32 {
        let app = app.clone();
        tasks.spawn(async move {
            identity::mint_session(&app, tenant, world, &format!("char-{i}"), None, TTL).await
        });
    }
    let mut numbers = std::collections::HashSet::new();
    while let Some(res) = tasks.join_next().await {
        let minted = res.expect("task").expect("mint");
        assert!(
            numbers.insert(minted.character.number.expect("assigned")),
            "duplicate number handed out"
        );
    }
    assert_eq!(numbers.len(), 32);
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn cooldown_blocks_retired_numbers(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;

    // Retire the entire 555-XXXX space inside the cooldown window: every
    // candidate must be rejected and the mint must fail `internal` after the
    // 10-attempt cap (a full number space is an operator problem).
    let mut tx = world_tx(&app, world).await.expect("world_tx");
    sqlx::query(
        "INSERT INTO retired_numbers (world_id, number, freed_at) \
         SELECT $1, '555-' || lpad(g::text, 4, '0'), now() FROM generate_series(0, 9999) g",
    )
    .bind(world)
    .execute(&mut *tx)
    .await
    .expect("retire all numbers");
    tx.commit().await.expect("commit");

    let res = mint(&app, tenant, world, "char-1").await;
    assert!(
        matches!(res, Err(Fail::Internal(_))),
        "exhausted number space must be internal, not a code"
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn verify_honors_revocation_and_rejects_bad_tokens(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;
    let secret = "test-secret";

    let minted = mint(&app, tenant, world, "char-1").await.expect("mint");
    let token = mint_jwt(secret, &minted.identity).expect("jwt");

    let ok = verify(&app, secret, &token)
        .await
        .expect("valid token verifies");
    assert_eq!(ok.session_id, minted.identity.session_id);
    assert_eq!(ok.character_id, minted.identity.character_id);

    // Tampered signature.
    let mut tampered = token.clone();
    tampered.pop();
    tampered.push(if token.ends_with('A') { 'B' } else { 'A' });
    assert_eq!(
        verify(&app, secret, &tampered).await.expect_err("tampered"),
        VerifyError::Unauthorized
    );

    // Expired claims (crafted directly — mint_jwt never emits these).
    let expired = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &serde_json::json!({
            "sid": minted.identity.session_id, "tenant": tenant, "world": world,
            "char": minted.identity.character_id, "device": minted.identity.device_id,
            "exp": 1_000_000,
        }),
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("encode");
    assert_eq!(
        verify(&app, secret, &expired).await.expect_err("expired"),
        VerifyError::Unauthorized
    );

    // Revoked session: signature still good, row check must reject.
    let mut tx = world_tx(&app, world).await.expect("world_tx");
    sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1")
        .bind(minted.identity.session_id)
        .execute(&mut *tx)
        .await
        .expect("revoke");
    tx.commit().await.expect("commit");
    assert_eq!(
        verify(&app, secret, &token).await.expect_err("revoked"),
        VerifyError::Unauthorized
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn settings_roundtrip_and_size_cap(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;
    let who = mint(&app, tenant, world, "char-1")
        .await
        .expect("mint")
        .identity;

    for scope in [SettingsScope::Device, SettingsScope::Character] {
        let doc = serde_json::json!({"wallpaper": "w1", "airplane": false});
        identity::set_settings(&app, &who, scope, doc.clone())
            .await
            .expect("set");
        let got = identity::get_settings(&app, &who, scope)
            .await
            .expect("get");
        assert_eq!(got, doc, "whole-document replace");
    }

    let huge = serde_json::json!({ "blob": "x".repeat(17 * 1024) });
    let res = identity::set_settings(&app, &who, SettingsScope::Device, huge).await;
    assert!(matches!(res, Err(Fail::Code(ErrCode::TooLarge))));
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn me_and_app_login(admin: PgPool) {
    let (world, tenant, _) = seed_world_tenant(&admin).await;
    let app = app_pool(&admin, 4).await;
    let who = mint(&app, tenant, world, "char-1")
        .await
        .expect("mint")
        .identity;
    let stranger = mint(&app, tenant, world, "char-2")
        .await
        .expect("mint")
        .identity;

    // Seed one app account per character.
    let (mine, theirs) = (
        opn_core::infra::ids::new_id(),
        opn_core::infra::ids::new_id(),
    );
    let mut tx = world_tx(&app, world).await.expect("world_tx");
    for (id, ch, handle) in [
        (mine, who.character_id, "me"),
        (theirs, stranger.character_id, "them"),
    ] {
        sqlx::query(
            "INSERT INTO app_accounts (id, world_id, character_id, app_id, handle) \
             VALUES ($1, $2, $3, 'chirp', $4)",
        )
        .bind(id)
        .bind(world)
        .bind(ch)
        .bind(handle)
        .execute(&mut *tx)
        .await
        .expect("seed app account");
    }
    tx.commit().await.expect("commit");

    // Foreign account → forbidden; own → lands in the session's active map.
    let res = identity::app_login(&app, &who, "chirp", theirs).await;
    assert!(matches!(res, Err(Fail::Code(ErrCode::Forbidden))));
    identity::app_login(&app, &who, "chirp", mine)
        .await
        .expect("own account");

    let me = identity::me(&app, &who).await.expect("me");
    assert_eq!(me.character.id, who.character_id);
    assert_eq!(me.device.id, who.device_id);
    assert_eq!(me.accounts.len(), 1, "own accounts only");
    assert_eq!(me.accounts[0].handle, "me");
    assert_eq!(
        me.active_accounts["chirp"],
        mine.to_string(),
        "per-session active map"
    );

    // share_presence toggle is a real column, not settings blob.
    identity::set_share_presence(&app, &who, false)
        .await
        .expect("toggle");
    let me = identity::me(&app, &who).await.expect("me");
    assert!(!me.character.share_presence);
}

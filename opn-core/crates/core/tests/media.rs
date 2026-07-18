//! Sprint 5 media tests (OPN-CORE.md §10.6): the presigned-upload vertical
//! slice against a real MinIO from the dev stack. Proves the exit criteria —
//! a photo round-trips (request → POST → commit → gallery → presigned GET →
//! attaches to a message), MinIO enforces the cap on the POST itself, and the
//! verify sweep catches a cap bypass uploaded through a laxer policy.
//!
//! Needs the compose stack up (postgres + minio + bucket `opn`). Like the
//! redis-backed tests, these fail loudly if the stack is down — that is the
//! point (rule 4: verify against the real thing).

mod common;

use common::{app_pool, seed_world_tenant, test_config};
use contracts::types::{MediaKind, UploadTarget, UploadTicket};
use opn_core::config::Config;
use opn_core::infra::auth::{mint_jwt, Identity};
use opn_core::primitives::{channels, identity, media};
use opn_core::state::AppState;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// Real S3 config from the dev stack (env overrides for CI), everything else
/// inert-but-valid like `test_config`.
fn media_config() -> Config {
    let mut cfg = test_config();
    cfg.s3_endpoint =
        std::env::var("S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into());
    cfg.s3_bucket = std::env::var("S3_BUCKET").unwrap_or_else(|_| "opn".into());
    cfg.s3_key = std::env::var("S3_KEY").unwrap_or_else(|_| "opn".into());
    cfg.s3_secret = std::env::var("S3_SECRET").unwrap_or_else(|_| "opnsecret".into());
    cfg.s3_region = "us-east-1".into();
    cfg
}

async fn state_and_identity(admin: &PgPool, framework_ref: &str) -> (AppState, Identity, String) {
    let (world_id, tenant_id, _key) = seed_world_tenant(admin).await;
    let pool = app_pool(admin, 4).await;
    let state = common::test_state(pool, media_config()).await;
    let minted = identity::mint_session(&state.pg, tenant_id, world_id, framework_ref, None, 600)
        .await
        .expect("mint session");
    let number = minted.character.number.clone().expect("number");
    (state, minted.identity, number)
}

/// Mint a second character in the SAME world/tenant as an existing identity.
async fn second_identity(
    state: &AppState,
    first: &Identity,
    framework_ref: &str,
) -> (Identity, String) {
    // Reuse the world; tenant id is carried on the identity.
    let minted = identity::mint_session(
        &state.pg,
        first.tenant_id,
        first.world_id,
        framework_ref,
        None,
        600,
    )
    .await
    .expect("mint session 2");
    (
        minted.identity,
        minted.character.number.clone().expect("number"),
    )
}

/// POST bytes to a presigned target the way a browser would. Returns the S3
/// status so the test can assert who rejected the upload.
async fn s3_post(target: &UploadTarget, bytes: Vec<u8>, mime: &str) -> reqwest::StatusCode {
    let mut form = reqwest::multipart::Form::new();
    for (k, v) in target.fields.as_object().expect("fields object") {
        form = form.text(k.clone(), v.as_str().expect("field str").to_owned());
    }
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("f")
        .mime_str(mime)
        .expect("mime");
    form = form.part("file", part);
    reqwest::Client::new()
        .post(&target.url)
        .multipart(form)
        .send()
        .await
        .expect("post to minio")
        .status()
}

fn original(t: &UploadTicket) -> &UploadTarget {
    t.targets
        .iter()
        .find(|t| t.role == "original")
        .expect("original target")
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn request_upload_commit_roundtrip(admin: PgPool) {
    let (state, who, _num) = state_and_identity(&admin, "alice").await;

    // request → two targets (photo issues original + thumb).
    let ticket = media::request_upload(&state, &who, MediaKind::Photo, 1024, "image/jpeg")
        .await
        .expect("request_upload");
    assert_eq!(ticket.targets.len(), 2, "photo issues original + thumb");

    // upload the bytes to MinIO through the policy.
    let bytes = vec![7u8; 1024];
    let status = s3_post(original(&ticket), bytes.clone(), "image/jpeg").await;
    assert!(status.is_success(), "minio accepted the upload: {status}");

    // commit → live.
    media::commit(&state, &who, ticket.media_id)
        .await
        .expect("commit");

    // gallery via the REAL router (route coverage, rule 3).
    let token = mint_jwt(&state.cfg.jwt_secret, &who).expect("jwt");
    let page = get_media(&state, &token).await;
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "one live item in the gallery");
    let url = items[0]["url"].as_str().expect("presigned url");

    // fetch the object back through the presigned GET.
    let got = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .expect("presigned get")
        .bytes()
        .await
        .expect("body");
    assert_eq!(got.as_ref(), bytes.as_slice(), "round-tripped bytes match");

    // un-gate: the committed media attaches to a message. Send it to a second
    // character over their direct thread.
    let (bob, bob_num) = second_identity(&state, &who, "bob").await;
    let _ = bob;
    let ch = channels::open_direct(&state, &who, &bob_num)
        .await
        .expect("open_direct");
    let channel_id: Uuid = serde_json::from_value(ch["channel_id"].clone()).expect("channel_id");
    let body = serde_json::json!({ "media_ids": [ticket.media_id] });
    let msg_body: contracts::types::MessageBody = serde_json::from_value(body).expect("body shape");
    channels::send(&state, &who, channel_id, Uuid::now_v7(), &msg_body)
        .await
        .expect("send with owned live media is allowed");

    // a foreign/unknown media id on the same channel is forbidden.
    let bad = serde_json::json!({ "media_ids": [Uuid::now_v7()] });
    let bad_body: contracts::types::MessageBody = serde_json::from_value(bad).expect("body");
    let err = channels::send(&state, &who, channel_id, Uuid::now_v7(), &bad_body).await;
    assert!(
        matches!(
            err,
            Err(opn_core::primitives::Fail::Code(
                contracts::ErrCode::Forbidden
            ))
        ),
        "unowned media attachment is forbidden",
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn caps_enforced(admin: PgPool) {
    let (state, who, _num) = state_and_identity(&admin, "alice").await;

    // Core rejects an over-cap declaration and a bad mime before any S3 call.
    assert!(matches!(
        media::request_upload(
            &state,
            &who,
            MediaKind::Photo,
            3 * 1024 * 1024,
            "image/jpeg"
        )
        .await,
        Err(opn_core::primitives::Fail::Code(
            contracts::ErrCode::TooLarge
        )),
    ));
    assert!(matches!(
        media::request_upload(&state, &who, MediaKind::Photo, 1024, "application/pdf").await,
        Err(opn_core::primitives::Fail::Code(
            contracts::ErrCode::Invalid
        )),
    ));

    // MinIO rejects an over-range POST *itself* (the cap is not Core's word).
    let ticket = media::request_upload(&state, &who, MediaKind::Photo, 1024, "image/jpeg")
        .await
        .expect("request_upload");
    let status = s3_post(original(&ticket), vec![0u8; 4096], "image/jpeg").await;
    assert!(
        status.is_client_error(),
        "minio rejects an upload past content-length-range: {status}",
    );
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn commit_foreign_forbidden(admin: PgPool) {
    let (state, alice, _num) = state_and_identity(&admin, "alice").await;
    let (bob, _bn) = second_identity(&state, &alice, "bob").await;

    let ticket = media::request_upload(&state, &alice, MediaKind::Audio, 1024, "audio/mpeg")
        .await
        .expect("request_upload");

    // Bob commits Alice's pending media → forbidden (exists, not his).
    assert!(matches!(
        media::commit(&state, &bob, ticket.media_id).await,
        Err(opn_core::primitives::Fail::Code(
            contracts::ErrCode::Forbidden
        )),
    ));
    // An unknown id → not_found.
    assert!(matches!(
        media::commit(&state, &bob, Uuid::now_v7()).await,
        Err(opn_core::primitives::Fail::Code(
            contracts::ErrCode::NotFound
        )),
    ));
}

#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn verify_reverts_cap_bypass(admin: PgPool) {
    let (state, who, _num) = state_and_identity(&admin, "alice").await;

    // Declare a small photo (bytes=1000) → a 1000-byte policy is issued.
    let ticket = media::request_upload(&state, &who, MediaKind::Photo, 1000, "image/jpeg")
        .await
        .expect("request_upload");

    // Forge a laxer policy (10 MB) for the SAME object key — simulates a client
    // that found a way past the issued cap — and upload an oversized object.
    let key = state.s3.object_key(who.world_id, ticket.media_id, false);
    let now = time::OffsetDateTime::now_utc();
    let (url, fields) = state
        .s3
        .post_policy(&key, "image/jpeg", 10 * 1024 * 1024, now)
        .expect("laxer policy");
    let laxer = UploadTarget {
        role: "original".into(),
        url,
        fields,
    };
    let status = s3_post(&laxer, vec![9u8; 5000], "image/jpeg").await;
    assert!(
        status.is_success(),
        "laxer policy upload accepted: {status}"
    );

    media::commit(&state, &who, ticket.media_id)
        .await
        .expect("commit");

    // The verify sweep HEADs the object, sees 5000 > declared 1000, reverts.
    let reverted = media::verify_live(&state, who.world_id)
        .await
        .expect("verify_live");
    assert_eq!(reverted, 1, "the oversized object was reverted");

    // The row is back to pending — the next reap deletes it, and it no longer
    // shows in the gallery.
    let cnt: i64 = {
        let mut tx = opn_core::infra::db::world_tx(&state.pg, who.world_id)
            .await
            .expect("tx");
        let c = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM media WHERE id = $1 AND state = 'pending'",
        )
        .bind(ticket.media_id)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
        tx.commit().await.expect("commit");
        c
    };
    assert_eq!(cnt, 1, "cap-bypasser reverted to pending");
}

async fn get_media(state: &AppState, token: &str) -> Value {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use opn_core::http::app_router;
    use tower::ServiceExt;

    let res = app_router(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/media")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK, "gallery 200");
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

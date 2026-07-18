//! API-key-authed tenant routes (OPN-CORE.md §6, §11).

use axum::extract::{FromRequestParts, Json, State};
use axum::http::{request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use contracts::types::SessionMintResponse;
use contracts::{ErrBody, ErrCode};
use serde::Deserialize;
use uuid::Uuid;

use crate::infra::auth::{api_key_hash, mint_jwt};
use crate::primitives::{identity, Fail};
use crate::state::AppState;

/// Authenticated tenant, extracted from `Authorization: Bearer opn_...`
/// (bare key accepted too). The sha256 of the presented key IS the lookup
/// key — indexed, no KDF (§11).
pub struct TenantAuth {
    pub tenant_id: Uuid,
    pub world_id: Uuid,
}

impl FromRequestParts<AppState> for TenantAuth {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let raw = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
            .ok_or_else(|| err_response(ErrCode::Unauthorized, "missing api key"))?;

        let row: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT id, world_id FROM tenants WHERE api_key_hash = $1")
                .bind(api_key_hash(raw))
                .fetch_optional(&state.pg)
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "tenant auth lookup failed");
                    err_response(ErrCode::Internal, "internal")
                })?;
        let (tenant_id, world_id) =
            row.ok_or_else(|| err_response(ErrCode::Unauthorized, "unknown api key"))?;
        Ok(TenantAuth {
            tenant_id,
            world_id,
        })
    }
}

pub fn status_of(code: ErrCode) -> StatusCode {
    match code {
        ErrCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrCode::Forbidden => StatusCode::FORBIDDEN,
        ErrCode::NotFound => StatusCode::NOT_FOUND,
        ErrCode::Invalid => StatusCode::BAD_REQUEST,
        ErrCode::Conflict => StatusCode::CONFLICT,
        ErrCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ErrCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub fn err_response(code: ErrCode, msg: &str) -> Response {
    (
        status_of(code),
        Json(ErrBody {
            code,
            msg: msg.into(),
        }),
    )
        .into_response()
}

/// HTTP mapping of a handler failure: internal errors log here and leak no
/// detail (§7).
pub fn fail_response(f: Fail) -> Response {
    match f {
        Fail::Code(code) => err_response(code, code_msg(code)),
        Fail::Internal(e) => {
            tracing::error!(error = %e, "handler internal error");
            err_response(ErrCode::Internal, "internal")
        }
    }
}

fn code_msg(code: ErrCode) -> &'static str {
    match code {
        ErrCode::Unauthorized => "unauthorized",
        ErrCode::Forbidden => "forbidden",
        ErrCode::NotFound => "not found",
        ErrCode::Invalid => "invalid",
        ErrCode::Conflict => "conflict",
        ErrCode::RateLimited => "rate limited",
        ErrCode::TooLarge => "too large",
        ErrCode::Internal => "internal",
    }
}

#[derive(Deserialize)]
pub struct MintSessionRequest {
    pub framework_ref: String,
    pub device_id: Option<Uuid>,
}

/// `POST /v1/tenants/self/sessions` — the auth bootstrap (§6, OPN.md §3).
pub async fn mint_session(
    State(state): State<AppState>,
    tenant: TenantAuth,
    Json(body): Json<MintSessionRequest>,
) -> Response {
    if body.framework_ref.is_empty() || body.framework_ref.len() > 128 {
        return err_response(ErrCode::Invalid, "framework_ref must be 1..=128 chars");
    }
    let minted = match identity::mint_session(
        &state.pg,
        tenant.tenant_id,
        tenant.world_id,
        &body.framework_ref,
        body.device_id,
        state.cfg.session_ttl_secs,
    )
    .await
    {
        Ok(m) => m,
        Err(f) => return fail_response(f),
    };
    let token = match mint_jwt(&state.cfg.jwt_secret, &minted.identity) {
        Ok(t) => t,
        Err(e) => return fail_response(Fail::Internal(e)),
    };
    Json(SessionMintResponse {
        token,
        session_id: minted.identity.session_id,
        character: minted.character,
        device: minted.device,
    })
    .into_response()
}
